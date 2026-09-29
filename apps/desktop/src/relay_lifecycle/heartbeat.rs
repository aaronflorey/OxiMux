//! The per-daemon PID watch and who gets to act on a death it reports.

use std::sync::Arc;

use super::{RelayLifecycle, RespawnReason};

impl RelayLifecycle {
    /// Watch `pid` (the daemon serving `session`) and respawn when it dies,
    /// unless someone else has already claimed that death. Replaces — and
    /// aborts — any previous watch, so at most one heartbeat runs.
    pub(super) fn arm_heartbeat(self: &Arc<Self>, pid: u32, session: String) {
        let _enter = self.handle.enter();
        let lifecycle = Arc::clone(self);
        let watch = self.supervisor.watch_pid(pid, move || {
            if !lifecycle.claim_death(&session) {
                tracing::info!(session_id = %session, "relay death already owned; heartbeat stands down");
                return;
            }
            // `on_death` is sync; the respawn is async. Boxed so the future
            // does not contain itself through the re-arm below.
            let spawn_on = lifecycle.handle.clone();
            spawn_on.spawn(respawn_boxed(lifecycle, RespawnReason::Crash, session));
        });
        let previous = self
            .heartbeat
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .replace(watch.abort_handle());
        if let Some(previous) = previous {
            previous.abort();
        }
    }

    /// Stop watching the current daemon. The manual restart does this before
    /// it stops the daemon, so the death it causes is never seen as a crash.
    #[allow(dead_code)] // first caller: the manual restart (phase 3)
    pub(super) fn abort_heartbeat(&self) {
        if let Some(watch) = self.heartbeat.lock().unwrap_or_else(|p| p.into_inner()).take() {
            watch.abort();
        }
    }

    /// Whether the heartbeat may treat `session`'s death as a crash. False when
    /// a manual restart already owns it.
    pub(super) fn claim_death(&self, session: &str) -> bool {
        self.expected_death.lock().unwrap_or_else(|p| p.into_inner()).as_deref() != Some(session)
    }
}

// Type-erased so `respawn → arm_heartbeat → watch closure → respawn` does not
// make the async fn's future type contain itself.
fn respawn_boxed(
    lifecycle: Arc<RelayLifecycle>,
    reason: RespawnReason,
    dead_session: String,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        lifecycle.respawn(reason, dead_session).await;
    })
}
