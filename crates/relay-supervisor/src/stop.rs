//! Stopping the running daemon for a restart.
//!
//! Graceful first: `Shutdown { kill_sessions: true }` has the daemon checkpoint
//! and end every session itself, then exit. Only when it will not — wedged,
//! not answering, or still alive long after agreeing — does this fall back to
//! signals, and then only at a pid that verifies as the daemon. Either way,
//! any session that outlived the daemon is swept afterwards.

use std::time::Duration;

use oximux_relay_client::{ClientError, RelayClient, endpoint_answers};
use oximux_relay_proto::{Request, Response};

use crate::identity::{Stopped, stop_verified_daemon, wait_dead};
use crate::{RelaySupervisor, pid_alive, sweep_session_survivors};

/// How long the "is anything still listening" check may take.
const ENDPOINT_CHECK_TIMEOUT: Duration = Duration::from_millis(500);
/// A daemon that agreed to stop may be mid-exit when it is verified; one more
/// look after this long tells "exiting" from "unreadable".
const EXITING_RECHECK: Duration = Duration::from_secs(1);

/// How long each stage may take.
#[derive(Debug, Clone, Copy)]
pub struct StopTimeouts {
    /// For the daemon to answer the shutdown request.
    pub rpc: Duration,
    /// For its process to be gone once it agreed. Covers its own kill grace
    /// for the sessions plus teardown.
    pub exit: Duration,
    /// Between SIGTERM and SIGKILL in the fallback.
    pub signal_grace: Duration,
}

impl Default for StopTimeouts {
    fn default() -> Self {
        Self {
            rpc: Duration::from_secs(5),
            exit: Duration::from_secs(7),
            signal_grace: Duration::from_secs(3),
        }
    }
}

/// How the daemon ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopPath {
    /// It shut down on request, ending its sessions itself.
    Rpc,
    /// It had to be signalled.
    Signal,
    /// It was already gone.
    AlreadyDead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StopError {
    /// There is no pid record to verify or watch the daemon by.
    #[error("the relay daemon has no pid record")]
    NoPidRecord,
    /// The daemon could not be verified — its pid did not check out, or a
    /// daemon still answers on its endpoint — so it was not signalled.
    /// `shutdown_accepted` means it had agreed to stop: its sessions are
    /// already ended and it refuses new ones, so it is not usable either.
    #[error("couldn't verify the relay daemon process (pid {pid})")]
    IdentityUnknown { pid: u32, shutdown_accepted: bool },
    /// It was verified and killed, and it is still running.
    #[error("the relay daemon (pid {pid}) survived being killed")]
    DaemonSurvived { pid: u32, shutdown_accepted: bool },
}

impl RelaySupervisor {
    /// Stop the daemon `client` is connected to. On `Ok` it is gone, and so is
    /// every session it had.
    pub async fn stop_daemon(
        &self,
        client: &RelayClient,
        timeouts: StopTimeouts,
    ) -> Result<StopPath, StopError> {
        let Some((pid, expect)) = self.expect_current() else {
            // A daemon that exits cleanly takes its pid file with it — after a
            // restart whose replacement failed to start, say. Nothing answering
            // on the endpoint then means nothing to stop, so a retry can go on
            // to start one; something answering without a record cannot be
            // verified, and is left alone.
            if self.endpoint_still_answers().await {
                return Err(StopError::NoPidRecord);
            }
            tracing::info!(step = "stopped", path = ?StopPath::AlreadyDead, "relay stop: no daemon and no pid record");
            return Ok(StopPath::AlreadyDead);
        };
        // Already dead (a crash just before the restart): the request would
        // only wait on a connection nobody reads.
        let accepted = pid_alive(pid) && request_shutdown(client, true, timeouts.rpc).await;
        tracing::info!(step = "rpc", accepted, pid, "relay stop");
        let path = if accepted && wait_dead(pid, timeouts.exit).await {
            StopPath::Rpc
        } else {
            let mut stopped = stop_verified_daemon(pid, &expect, timeouts.signal_grace).await;
            if stopped == Stopped::Unknown && accepted {
                tokio::time::sleep(EXITING_RECHECK).await;
                stopped = stop_verified_daemon(pid, &expect, timeouts.signal_grace).await;
            }
            match stopped {
                Stopped::Stopped => StopPath::Signal,
                // "Not the pid on record" is not "no daemon": a wedged daemon
                // whose record was overwritten still owns the endpoint, and
                // replacing it would run two daemons and every agent twice.
                Stopped::NotRunning if self.endpoint_still_answers().await => {
                    return Err(StopError::IdentityUnknown { pid, shutdown_accepted: accepted });
                }
                Stopped::NotRunning => StopPath::AlreadyDead,
                Stopped::Unknown => {
                    return Err(StopError::IdentityUnknown { pid, shutdown_accepted: accepted });
                }
                Stopped::Survived => {
                    return Err(StopError::DaemonSurvived { pid, shutdown_accepted: accepted });
                }
            }
        };
        tracing::info!(step = "stopped", ?path, pid, "relay stop");
        // A daemon that did not exit on its own left its socket and pid file
        // behind — unless a new daemon has already claimed them.
        if path != StopPath::Rpc && self.read_pid_record().is_none_or(|r| r.pid == pid) {
            let _ = std::fs::remove_file(self.socket_path());
            let _ = std::fs::remove_file(self.pid_path());
        }
        // Idempotent, and cheap when the daemon ended its sessions itself: a
        // signalled daemon's sessions got only a hangup, and a session spawned
        // after its snapshot may have been missed either way.
        let swept = sweep_session_survivors(&self.checkpoints_dir()).await;
        if swept > 0 {
            tracing::warn!(swept, "ended sessions that outlived the relay daemon");
        }
        Ok(path)
    }

    async fn endpoint_still_answers(&self) -> bool {
        // A check that cannot finish counts as "answers": when in doubt, do
        // not start a second daemon.
        tokio::time::timeout(ENDPOINT_CHECK_TIMEOUT, endpoint_answers(&self.socket_path()))
            .await
            .unwrap_or(true)
    }
}

/// Ask the daemon to stop — ending its sessions with `kill_sessions`, else
/// only if it has none. Whether it agreed.
pub(crate) async fn request_shutdown(client: &RelayClient, kill_sessions: bool, within: Duration) -> bool {
    match tokio::time::timeout(within, client.request(Request::Shutdown { kill_sessions })).await {
        Ok(Ok(Response::Ok)) => true,
        // The reply raced the daemon's exit: it was sent, so it was seen.
        Ok(Err(ClientError::Disconnected)) => true,
        Ok(Ok(other)) => {
            tracing::info!(kill_sessions, ?other, "relay refused the shutdown request");
            false
        }
        Ok(Err(err)) => {
            tracing::warn!(kill_sessions, %err, "relay shutdown request failed");
            false
        }
        Err(_) => {
            tracing::warn!(kill_sessions, "relay did not answer the shutdown request");
            false
        }
    }
}
