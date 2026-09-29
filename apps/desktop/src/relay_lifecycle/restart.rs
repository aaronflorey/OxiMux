//! The user-facing restart: stop the daemon (ending every session with it),
//! bring a new one up, and let pane recovery take it from there.
//!
//! Coalesced: every caller while one is in flight gets the same future, so a
//! double click — or the palette and Settings at once — restarts once. The
//! whole sequence holds `respawn_lock`, so the crash heartbeat can neither
//! claim the death this causes nor respawn in the middle of it.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::FutureExt as _;
use futures::future::{BoxFuture, Shared};
use oximux_relay_supervisor::{StopError, StopPath, StopTimeouts};

use super::{RelayLifecycle, RelayLifecycleEvent, RespawnFailure, RespawnOutcome, RespawnReason};
use crate::shell::terminal_view::APP_QUITTING;

/// One restart, shared by everyone who asked for it while it ran.
pub type RestartFuture = Shared<BoxFuture<'static, Result<RestartOutcome, RestartError>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartOutcome {
    pub old_pid: Option<u32>,
    pub new_pid: Option<u32>,
    pub new_session: String,
    /// How the old daemon ended. `None` when it had already been replaced —
    /// a crash respawn got there first — so there was nothing to stop.
    pub stop_path: Option<StopPath>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RestartError {
    /// The old daemon could not be stopped. It is still the current one —
    /// usable unless the error says it had already accepted the shutdown.
    #[error("{0}")]
    Stop(StopError),
    /// The old daemon is gone and a new one would not start.
    #[error("the new daemon did not start ({0:?})")]
    Respawn(RespawnFailure),
    #[error("the app is quitting")]
    Quitting,
    /// The restart itself failed unexpectedly (see the log).
    #[error("the restart failed unexpectedly")]
    Internal,
}

/// Held by the running restart. However it ends — panics included — the slot
/// empties so the next request runs, and a panic mid-restart hands the daemon
/// back to the heartbeat rather than leaving crash detection off.
struct InFlight(Arc<RelayLifecycle>);

impl Drop for InFlight {
    fn drop(&mut self) {
        *self.0.restart_in_flight.lock().unwrap_or_else(|p| p.into_inner()) = None;
        if std::thread::panicking() {
            *self.0.expected_death.lock().unwrap_or_else(|p| p.into_inner()) = None;
            self.0.rearm_heartbeat();
        }
    }
}

impl RelayLifecycle {
    /// Restart the daemon, or join the restart already running.
    pub fn restart(self: &Arc<Self>) -> RestartFuture {
        let mut slot = self.restart_in_flight.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(running) = slot.as_ref() {
            return running.clone();
        }
        let lifecycle = Arc::clone(self);
        // Spawned rather than polled by the callers: the sequence needs this
        // runtime's timers, and the callers may be on the UI thread. The slot
        // is still locked here, so the task's clear cannot run before the
        // slot is filled.
        let task = self.handle.spawn(async move {
            let _in_flight = InFlight(Arc::clone(&lifecycle));
            lifecycle.run_restart().await
        });
        let shared = async move {
            task.await.unwrap_or_else(|err| {
                tracing::error!(?err, "relay restart task ended abnormally");
                Err(if err.is_panic() { RestartError::Internal } else { RestartError::Quitting })
            })
        }
        .boxed()
        .shared();
        *slot = Some(shared.clone());
        shared
    }

    async fn run_restart(self: &Arc<Self>) -> Result<RestartOutcome, RestartError> {
        #[cfg(test)]
        self.restart_runs.fetch_add(1, Ordering::SeqCst);
        if APP_QUITTING.load(Ordering::SeqCst) {
            return Err(RestartError::Quitting);
        }
        // Snapshot BEFORE the lock: a crash respawn parked on the lock may win
        // it, and then the daemon to stop is no longer the one the user saw.
        let session = self.current_session();
        let _held = self.respawn_lock.lock().await;
        // A crash respawn can hold the lock for half a minute; a quit that
        // began meanwhile must not be answered by ending every session.
        if APP_QUITTING.load(Ordering::SeqCst) {
            return Err(RestartError::Quitting);
        }
        if self.current_session() != session {
            tracing::info!("relay was replaced while the restart waited; nothing to stop");
            return Ok(RestartOutcome {
                old_pid: None,
                new_pid: self.pid(),
                new_session: self.current_session(),
                stop_path: None,
            });
        }
        let old_pid = self.pid();
        tracing::info!(step = "claim", ?old_pid, "relay restart");
        *self.expected_death.lock().unwrap_or_else(|p| p.into_inner()) = Some(session.clone());
        self.abort_heartbeat();

        let stopped = self.supervisor.stop_daemon(&self.client(), StopTimeouts::default()).await;
        let stop_path = match stopped {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!(%err, "relay restart could not stop the daemon");
                *self.expected_death.lock().unwrap_or_else(|p| p.into_inner()) = None;
                // Not replaced: keep watching the daemon we still have.
                self.rearm_heartbeat();
                self.emit(RelayLifecycleEvent::RestartFailed { reason: err.to_string().into() });
                return Err(RestartError::Stop(err));
            }
        };
        tracing::info!(step = "stopped", ?stop_path, "relay restart");

        match self.respawn_locked(RespawnReason::Manual, session).await {
            RespawnOutcome::Respawned { new_session } => {
                tracing::info!(step = "respawned", session_id = %new_session, "relay restart");
                Ok(RestartOutcome {
                    old_pid,
                    new_pid: self.pid(),
                    new_session,
                    stop_path: Some(stop_path),
                })
            }
            // The lock was held and the epoch checked, so no one else could
            // have replaced it; report it the same way the early return does.
            RespawnOutcome::AlreadyRespawned => Ok(RestartOutcome {
                old_pid,
                new_pid: self.pid(),
                new_session: self.current_session(),
                stop_path: Some(stop_path),
            }),
            RespawnOutcome::Skipped => Err(RestartError::Quitting),
            RespawnOutcome::Failed(failure) => {
                *self.expected_death.lock().unwrap_or_else(|p| p.into_inner()) = None;
                Err(RestartError::Respawn(failure))
            }
        }
    }
}
