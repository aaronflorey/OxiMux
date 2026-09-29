//! Replacing a dead daemon: bring a new one up, swap it in everywhere the old
//! one was reachable, and re-arm the watch.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use oximux_relay_client::{RelayBackend, RelayClient};
use oximux_relay_supervisor::SupervisorError;

use super::{RelayLifecycle, RelayLifecycleEvent, RespawnFailure, RespawnReason};
use crate::shell::terminal_view::{APP_QUITTING, shared_backend};

/// What one call to [`RelayLifecycle::respawn`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespawnOutcome {
    Respawned { new_session: String },
    /// The session to replace is no longer current: another caller already
    /// replaced it. Nothing was touched and nothing was emitted.
    AlreadyRespawned,
    /// The app is quitting; the next launch boots a daemon anyway.
    Skipped,
    Failed(RespawnFailure),
}

// Bounded respawn retry: 5 attempts, 500ms → 8s exponential backoff
// (7.5s total sleep + supervisor-call latency). Enough to ride out a
// socket race or a fork stalled under load, short enough that the user
// isn't left wondering.
const RESPAWN_MAX_ATTEMPTS: u32 = 5;
// The retry loop's post-loop arm is unreachable only while at least one
// attempt runs — keep that true at compile time.
const _: () = assert!(RESPAWN_MAX_ATTEMPTS >= 1);
const RESPAWN_BASE_DELAY: Duration = Duration::from_millis(500);
const RESPAWN_MAX_DELAY: Duration = Duration::from_secs(8);

fn respawn_backoff_delay(attempt: u32) -> Duration {
    RESPAWN_BASE_DELAY
        .saturating_mul(1u32 << attempt.saturating_sub(1).min(16))
        .min(RESPAWN_MAX_DELAY)
}

impl RelayLifecycle {
    /// Replace the daemon that served `dead_session`. Single-flight: see the
    /// module docs.
    pub async fn respawn(
        self: &Arc<Self>,
        reason: RespawnReason,
        dead_session: String,
    ) -> RespawnOutcome {
        let _held = self.respawn_lock.lock().await;
        self.respawn_locked(reason, dead_session).await
    }

    /// [`Self::respawn`] for a caller that already holds `respawn_lock` (the
    /// manual restart holds it across stop + respawn).
    pub(super) async fn respawn_locked(
        self: &Arc<Self>,
        reason: RespawnReason,
        dead_session: String,
    ) -> RespawnOutcome {
        if self.current_session() != dead_session {
            tracing::info!(session_id = %dead_session, "relay already respawned; nothing to do");
            return RespawnOutcome::AlreadyRespawned;
        }
        {
            let mut throttle = self.crash_throttle.lock().unwrap_or_else(|p| p.into_inner());
            match reason {
                RespawnReason::Crash if !throttle.admit(Instant::now()) => {
                    drop(throttle);
                    tracing::warn!("relay keeps stopping; not respawning it again");
                    return self.fail(reason, RespawnFailure::KeepsStopping);
                }
                RespawnReason::Crash => {}
                RespawnReason::Manual => throttle.reset(),
            }
        }
        tracing::warn!(session_id = %dead_session, ?reason, "replacing the relay daemon");
        // The app is tearing down — views are dropping and the next launch
        // runs a full supervisor boot anyway. Don't race it with a respawn.
        // Best-effort check: a quit that STARTS after this load lets the
        // respawn run during teardown — accepted; worst case is a swapped-in
        // backend nobody reads plus one stray notification, and runtime drop
        // waits out the re-armed heartbeat tick (~1s) at exit.
        if APP_QUITTING.load(Ordering::SeqCst) {
            tracing::info!("app quitting; skipping relay respawn");
            return RespawnOutcome::Skipped;
        }
        let client = match self.start_daemon().await {
            Ok(client) => Arc::new(client),
            Err(None) => return RespawnOutcome::Skipped,
            Err(Some(failure)) => return self.fail(reason, failure),
        };
        let new_session = client.server_session_id().to_owned();
        if !self.swap_backend(&client).await {
            // Unreachable in practice: the lifecycle is only installed when
            // boot installed a backend. Log rather than install — consumers
            // cached `None` at boot and won't re-check.
            tracing::warn!("relay respawned but no shared backend was installed at boot");
            return self.fail(reason, RespawnFailure::Other("no shared backend".into()));
        }
        *self.client.write().unwrap_or_else(|p| p.into_inner()) = Arc::clone(&client);
        *self.current_session.write().unwrap_or_else(|p| p.into_inner()) = new_session.clone();
        // The epoch moved; whatever death a restart claimed is behind us.
        *self.expected_death.lock().unwrap_or_else(|p| p.into_inner()) = None;
        // Remote terminals cached their source at boot; move it, not them.
        if let Some(terminals) = crate::remote_control::relay_terminals::installed() {
            terminals.rebind(client);
        }
        match self.supervisor.read_pid() {
            Some(pid) => self.arm_heartbeat(pid, new_session.clone()),
            None => tracing::warn!("respawned relay PID file missing; crash heartbeat disabled"),
        }
        // Pruned only now, so a respawn that failed leaves the rows for the
        // next launch to cold-restore from — and only after a crash: panes a
        // manual restart is still recovering keep theirs until the recovered
        // sessions are persisted over them, so a quit in between still
        // cold-restores them.
        if reason == RespawnReason::Crash {
            self.prune_session_rows(dead_session.clone()).await;
        }
        tracing::info!(session_id = %new_session, "relay daemon respawned; shared backend swapped in place");
        if reason == RespawnReason::Crash {
            notify_user(
                "OxiMux relay restarted",
                "The terminal daemon stopped unexpectedly and was restarted. \
                 Shells are back with their scrollback; what ran in them was stopped.",
            );
        }
        self.emit(RelayLifecycleEvent::Respawned {
            dead_session,
            new_session: new_session.clone(),
            reason,
        });
        RespawnOutcome::Respawned { new_session }
    }

    /// Bring a daemon up, retrying transient failures. `Err(None)` means the
    /// app began quitting mid-retry.
    async fn start_daemon(&self) -> Result<RelayClient, Option<RespawnFailure>> {
        // Bounded retry: a transient spawn failure (socket race, slow fork
        // under load) must not permanently end daemon-backed terminals for
        // the rest of the session. Version mismatch and a held pipe name are
        // NOT transient — someone else owns the endpoint — so they end it now.
        for attempt in 1..=RESPAWN_MAX_ATTEMPTS {
            if APP_QUITTING.load(Ordering::SeqCst) {
                tracing::info!("app quitting; abandoning relay respawn retries");
                return Err(None);
            }
            match self.supervisor.ensure_running().await {
                Ok(client) => return Ok(client),
                Err(SupervisorError::VersionMismatch) => {
                    return Err(Some(RespawnFailure::VersionMismatch));
                }
                Err(SupervisorError::EndpointHeld) => {
                    return Err(Some(RespawnFailure::EndpointHeld));
                }
                Err(err) if attempt < RESPAWN_MAX_ATTEMPTS => {
                    let delay = respawn_backoff_delay(attempt);
                    tracing::warn!(
                        ?err,
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        "relay respawn attempt failed; backing off"
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(err) => {
                    return Err(Some(RespawnFailure::Other(err.to_string().into())));
                }
            }
        }
        unreachable!("the final attempt returns")
    }

    /// Publish a backend for `client` in `SHARED_BACKEND`, in place. On the
    /// blocking pool: the main thread may hold that mutex inside a `block_on`
    /// whose reply needs this runtime's workers.
    async fn swap_backend(&self, client: &Arc<RelayClient>) -> bool {
        let backend = RelayBackend::new(Arc::clone(client), self.handle.clone());
        let swapped = tokio::task::spawn_blocking(move || {
            let Some(shared) = shared_backend() else {
                return false;
            };
            let mut guard = shared.lock().unwrap_or_else(|p| p.into_inner());
            // Seed BEFORE the swap publishes the new backend: orphaned
            // sessions get one synthetic loss each, and the id floor moves
            // past them so no live view's id is ever re-minted.
            backend.seed_daemon_losses(guard.sessions_to_carry());
            *guard = Box::new(backend);
            true
        })
        .await;
        swapped.unwrap_or_else(|err| {
            tracing::warn!(?err, "relay backend swap panicked");
            false
        })
    }

    async fn prune_session_rows(&self, dead_session: String) {
        // SQLite delete is blocking — keep it off the runtime worker.
        let repo = self.repo.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(err) = repo.delete_for_session(&dead_session) {
                tracing::warn!(?err, "pruning pane_relay_ids for dead session failed");
            }
        })
        .await;
    }

    fn fail(&self, reason: RespawnReason, failure: RespawnFailure) -> RespawnOutcome {
        tracing::warn!(?reason, ?failure, "relay respawn failed; PTYs fall back to in-process");
        // A manual restart reports its own failure in the window it was asked
        // from; the banner is for a crash, which nobody was watching for.
        if reason == RespawnReason::Crash {
            let (title, message) = if failure == RespawnFailure::KeepsStopping {
                (
                    "OxiMux relay keeps stopping",
                    "The terminal daemon stopped several times in a minute and was not restarted. \
                     Relaunch OxiMux to start it again.",
                )
            } else {
                (
                    "OxiMux relay could not be restarted",
                    "New terminals will run in-process (no quit-survival) until you relaunch OxiMux.",
                )
            };
            notify_user(title, message);
        }
        self.emit(RelayLifecycleEvent::RespawnFailed { reason, failure: failure.clone() });
        RespawnOutcome::Failed(failure)
    }
}

fn notify_user(title: &str, message: &str) {
    #[cfg(target_os = "macos")]
    crate::notifier::mac::post_system_banner(title, message);
    #[cfg(not(target_os = "macos"))]
    let _ = (title, message);
}

#[cfg(test)]
mod tests {
    use super::*;

    // The respawn backoff must grow geometrically from the base, cap at
    // the max, and never overflow on absurd attempt numbers — a transient
    // daemon-spawn failure rides this exact schedule before giving up.
    #[test]
    fn respawn_backoff_grows_and_caps() {
        assert_eq!(respawn_backoff_delay(1), Duration::from_millis(500));
        assert_eq!(respawn_backoff_delay(2), Duration::from_secs(1));
        assert_eq!(respawn_backoff_delay(3), Duration::from_secs(2));
        assert_eq!(respawn_backoff_delay(4), Duration::from_secs(4));
        assert_eq!(respawn_backoff_delay(5), RESPAWN_MAX_DELAY);
        assert_eq!(respawn_backoff_delay(64), RESPAWN_MAX_DELAY);
        // Total worst-case wait stays well under a minute.
        let total: Duration = (1..RESPAWN_MAX_ATTEMPTS).map(respawn_backoff_delay).sum();
        assert!(total <= Duration::from_secs(30));
    }
}
