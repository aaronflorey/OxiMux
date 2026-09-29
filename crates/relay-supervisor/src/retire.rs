//! Retiring the daemon of the protocol version this build replaced.
//!
//! A protocol bump renames the socket, so a new app never talks to the old
//! daemon — but that daemon keeps every session it had alive, and the new app
//! resumes those agents on its own daemon. Left alone, each agent would run
//! twice. So at boot, before anything is restored, the old daemon is stopped —
//! after proving the pid really is it — and then any of its sessions that
//! outlived it.

use std::time::Duration;

use crate::identity::{Expect, Stopped, mtime_secs, stop_verified_daemon};
use crate::{PREVIOUS_PID_FILENAME, PREVIOUS_SOCKET_FILENAME, PREVIOUS_TOKEN_FILENAME, RelaySupervisor};

/// Grace for the old daemon to take its final checkpoint and exit on SIGTERM.
const RETIRE_GRACE: Duration = Duration::from_secs(5);

/// What [`RelaySupervisor::retire_previous_protocol_daemon`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retired {
    /// No previous daemon on record.
    NoneFound,
    /// A previous daemon was running and has been stopped.
    Stopped,
    /// Its pid file was stale (no such daemon running); the files were removed.
    StaleFilesRemoved,
    /// Its pid could not be verified; it was left alone.
    Skipped,
    /// It was verified and signalled, and it is still running.
    Survived,
}

impl RelaySupervisor {
    /// Stop the previous protocol's daemon, if one is running. Must run before
    /// `ensure_running` and before any restore, so nothing is resumed twice.
    pub async fn retire_previous_protocol_daemon(&self) -> Retired {
        let pid_path = self.runtime_dir.join(PREVIOUS_PID_FILENAME);
        let Some(pid) = std::fs::read_to_string(&pid_path).ok().and_then(|s| s.trim().parse().ok())
        else {
            return Retired::NoneFound;
        };
        let expect = Expect {
            socket_path: self.runtime_dir.join(PREVIOUS_SOCKET_FILENAME),
            pid_path: pid_path.clone(),
            started_at: None,
            pid_file_mtime: mtime_secs(&pid_path),
        };
        let outcome = match stop_verified_daemon(pid, &expect, RETIRE_GRACE).await {
            Stopped::Stopped => Retired::Stopped,
            Stopped::NotRunning => Retired::StaleFilesRemoved,
            Stopped::Unknown => {
                tracing::warn!(pid, "could not verify the previous relay daemon; leaving it running");
                return Retired::Skipped;
            }
            Stopped::Survived => {
                tracing::warn!(pid, "the previous relay daemon survived being stopped");
                return Retired::Survived;
            }
        };
        tracing::info!(pid, ?outcome, "retired the previous protocol's relay daemon");
        // Stopping the daemon does not end its sessions — they only got a
        // hangup — and anything that ignored it would be resumed twice.
        let swept = crate::sweep_session_survivors(&self.checkpoints_dir()).await;
        if swept > 0 {
            tracing::warn!(swept, "ended sessions the previous daemon left running");
        }
        // A daemon that exited cleanly removed its own files; one that was
        // killed, or was never running, did not.
        for name in [PREVIOUS_SOCKET_FILENAME, PREVIOUS_TOKEN_FILENAME, PREVIOUS_PID_FILENAME] {
            let _ = std::fs::remove_file(self.runtime_dir.join(name));
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_previous_pid_file_is_a_no_op() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = RelaySupervisor::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        assert_eq!(s.retire_previous_protocol_daemon().await, Retired::NoneFound);
    }

    // A previous pid file naming a live process that is not the daemon — the
    // pid was recycled — must never get that process signalled.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_recycled_previous_pid_is_never_signalled() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = RelaySupervisor::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        let mut bystander = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        std::fs::write(dir.path().join(PREVIOUS_PID_FILENAME), bystander.id().to_string()).unwrap();

        assert_eq!(s.retire_previous_protocol_daemon().await, Retired::StaleFilesRemoved);
        assert!(crate::pid_alive(bystander.id()), "the bystander was left alone");
        assert!(!dir.path().join(PREVIOUS_PID_FILENAME).exists(), "stale files cleared");
        let _ = bystander.kill();
        let _ = bystander.wait();
    }
}
