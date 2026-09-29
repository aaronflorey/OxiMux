//! "Is the daemon answering?" — asked when something suggests it may not be:
//! a relay spawn that failed, the daemon section of Settings opening.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use oximux_relay_proto::{Request, Response};

use super::{RelayLifecycle, RelayLifecycleEvent};

/// How long a live daemon has to answer `Stats`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// At most one "not responding" report per this window.
const UNREACHABLE_DEBOUNCE: Duration = Duration::from_secs(30);

impl RelayLifecycle {
    /// Ask the daemon for its stats and report whether it answered. At most
    /// one probe runs at a time; extra calls while one is out are dropped.
    /// Goes through the lifecycle's own client, never `SHARED_BACKEND`, so a
    /// wedged daemon holding that mutex cannot hold the probe too.
    pub fn probe(self: &Arc<Self>) {
        // A restart or respawn is replacing the daemon right now; it reports
        // its own outcome, and probing the one on its way out says nothing.
        if self.respawn_lock.try_lock().is_err() {
            return;
        }
        if self.probe_in_flight.swap(true, Ordering::SeqCst) {
            return;
        }
        let lifecycle = Arc::clone(self);
        self.handle.spawn(async move {
            let session = lifecycle.current_session();
            let answered = matches!(
                tokio::time::timeout(PROBE_TIMEOUT, lifecycle.client().request(Request::Stats)).await,
                Ok(Ok(Response::StatsOk(_)))
            );
            lifecycle.probe_in_flight.store(false, Ordering::SeqCst);
            lifecycle.report_probe(answered, &session);
        });
    }

    /// Report a probe of the daemon that served `session` — dropped when that
    /// is no longer the current one, so a stale answer about a replaced daemon
    /// never overwrites the status of its successor.
    pub(super) fn report_probe(&self, responsive: bool, session: &str) {
        if self.current_session() != session {
            return;
        }
        if !responsive {
            let mut last = self.last_unreachable.lock().unwrap_or_else(|p| p.into_inner());
            if last.is_some_and(|at| at.elapsed() < UNREACHABLE_DEBOUNCE) {
                return;
            }
            *last = Some(Instant::now());
            tracing::warn!("relay daemon is not responding");
        }
        self.emit(RelayLifecycleEvent::Probed { responsive });
    }
}
