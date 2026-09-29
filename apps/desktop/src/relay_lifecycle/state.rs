//! What every daemon surface reads: one global, kept current by one
//! foreground loop that drains the lifecycle's events.

use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gpui::{App, AsyncApp, Global, SharedString};
use oximux_relay_proto::PidRecord;

use super::{RelayLifecycleEvent, RespawnFailure, lifecycle};

/// The daemon as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonStatus {
    Running { pid: Option<u32>, session_id: String },
    Unreachable { reason: SharedString },
    /// The relay never came up at boot; terminals run inside the app.
    InProcess,
}

/// A long operation the user started. While set, Restart and Kill all are
/// disabled everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Restarting,
    KillingAll,
}

/// The running daemon is from another app version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleInfo {
    pub daemon_version: String,
    pub app_version: String,
}

/// What Settings shows beyond the status: fetched when its daemon section
/// is on screen and after every lifecycle event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Details {
    /// Sessions the daemon lists; `None` when it could not be asked.
    pub sessions: Option<usize>,
    /// The daemon's pid record: its version and when it started.
    pub record: Option<PidRecord>,
}

/// How long [`Details`] are good for while Settings shows them.
const DETAILS_TTL: Duration = Duration::from_secs(5);

pub struct RelayDaemonState {
    pub status: DaemonStatus,
    pub busy: Option<Busy>,
    pub stale: Option<StaleInfo>,
    pub details: Option<Details>,
    /// When the last fetch of `details` began; `None` before the first.
    details_at: Option<Instant>,
    /// When each daemon alert last showed, by its text (see `ui::show`).
    pub(super) last_alerts: std::collections::HashMap<String, Instant>,
    /// Every notice shown that says the daemon is down or unanswered, by its
    /// text: what a recovery takes down (see `ui::retract_alerts`).
    pub(super) down_notices: std::collections::HashSet<String>,
}

impl RelayDaemonState {
    pub(super) fn new(status: DaemonStatus, stale: Option<StaleInfo>) -> Self {
        Self {
            status,
            busy: None,
            stale,
            details: None,
            details_at: None,
            last_alerts: Default::default(),
            down_notices: Default::default(),
        }
    }
}

impl Global for RelayDaemonState {}

/// Install the global and start the loop that keeps it current. Called once,
/// at app init, after the relay boot.
pub fn install(cx: &mut App) {
    let Some(lifecycle) = lifecycle() else {
        cx.set_global(RelayDaemonState::new(DaemonStatus::InProcess, None));
        return;
    };
    cx.set_global(RelayDaemonState::new(
        DaemonStatus::Running { pid: lifecycle.pid(), session_id: lifecycle.current_session() },
        lifecycle.take_stale_at_boot(),
    ));
    let Some(mut events) = lifecycle.take_events() else {
        tracing::warn!("relay lifecycle events already drained elsewhere");
        return;
    };
    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Some(event) = events.next().await {
            cx.update(|cx| apply(cx, event));
        }
    })
    .detach();
}

fn apply(cx: &mut App, event: RelayLifecycleEvent) {
    let reprobe = reprobe_after(&event);
    let state = cx.global_mut::<RelayDaemonState>();
    // Already down for a known reason (it keeps stopping, a restart failed):
    // a probe that goes unanswered says nothing new, and neither its reason
    // nor a second alert should cover the first.
    let known_down = matches!(event, RelayLifecycleEvent::Probed { responsive: false })
        && matches!(&state.status, DaemonStatus::Unreachable { reason } if reason != super::ui::NOT_RESPONDING);
    let notice = if known_down { None } else { super::ui::event_notice(&event) };
    let recovered = recovers(&event, &state.status);
    match event {
        RelayLifecycleEvent::Respawned { new_session, .. } => {
            state.status = DaemonStatus::Running {
                pid: lifecycle().and_then(|l| l.pid()),
                session_id: new_session,
            };
            // Whatever daemon is running now was started by this app.
            state.stale = None;
        }
        RelayLifecycleEvent::RespawnFailed { failure, .. } => {
            state.status = DaemonStatus::Unreachable { reason: failure_reason(&failure) };
        }
        RelayLifecycleEvent::RestartFailed { reason } => {
            state.status = DaemonStatus::Unreachable { reason: SharedString::from(reason.to_string()) };
        }
        RelayLifecycleEvent::Probed { responsive: false } => {
            if !known_down {
                state.status = DaemonStatus::Unreachable { reason: super::ui::NOT_RESPONDING.into() };
            }
        }
        // Answering again after being reported unreachable.
        RelayLifecycleEvent::Probed { responsive: true } => {
            if let (DaemonStatus::Unreachable { .. }, Some(lifecycle)) = (&state.status, lifecycle()) {
                state.status = DaemonStatus::Running {
                    pid: lifecycle.pid(),
                    session_id: lifecycle.current_session(),
                };
            }
        }
        // Nothing to change in the status; the notice itself is the toast
        // the restart surfaces add.
        RelayLifecycleEvent::PreviousDaemonRetired { foreign_serve } => {
            tracing::info!(foreign_serve, "terminals restarted once for the daemon upgrade");
        }
    }
    cx.refresh_windows();
    if recovered {
        super::ui::retract_alerts(cx);
    }
    if let Some(notice) = notice {
        super::ui::show(cx, notice);
    }
    if reprobe
        && let Some(lifecycle) = lifecycle()
    {
        lifecycle.probe();
    }
    refresh_details(cx);
}

/// Whether `event` finds the daemon up again after `status` — respawned, or
/// answering after it did not — which makes every notice it raised while
/// down stale.
pub(super) fn recovers(event: &RelayLifecycleEvent, status: &DaemonStatus) -> bool {
    match event {
        RelayLifecycleEvent::Respawned { .. } => true,
        RelayLifecycleEvent::Probed { responsive: true } => matches!(status, DaemonStatus::Unreachable { .. }),
        _ => false,
    }
}

/// Whether `event` is worth a fresh probe: whatever changed, the status
/// should say whether the daemon answers now. Never a probe's own answer —
/// that would probe again, answer again, and never stop — nor a respawn that
/// failed, which left no daemon to ask.
pub(super) fn reprobe_after(event: &RelayLifecycleEvent) -> bool {
    !matches!(event, RelayLifecycleEvent::Probed { .. } | RelayLifecycleEvent::RespawnFailed { .. })
}

/// Fetch [`Details`] again. Asks nothing that raises a lifecycle event, so
/// the event loop may call it.
pub fn refresh_details(cx: &mut App) {
    let Some(lifecycle) = lifecycle() else {
        return;
    };
    if !cx.has_global::<RelayDaemonState>() {
        return;
    }
    cx.global_mut::<RelayDaemonState>().details_at = Some(Instant::now());
    cx.spawn(async move |cx: &mut AsyncApp| {
        let sessions = lifecycle.list_pty_ids().await.ok().map(|ids| ids.len());
        let record = cx
            .background_executor()
            .spawn(async move { lifecycle.supervisor().read_pid_record() })
            .await;
        cx.update(|cx| {
            if cx.has_global::<RelayDaemonState>() {
                cx.global_mut::<RelayDaemonState>().details = Some(Details { sessions, record });
                cx.refresh_windows();
            }
        });
    })
    .detach();
}

/// [`refresh_details`], and a probe, when the last fetch is older than
/// [`DETAILS_TTL`] — for a surface that shows them, to call as it renders.
pub fn refresh_details_if_stale(cx: &mut App) {
    let stale = cx
        .try_global::<RelayDaemonState>()
        .is_some_and(|s| s.details_at.is_none_or(|at| at.elapsed() >= DETAILS_TTL));
    if stale {
        if let Some(lifecycle) = lifecycle() {
            lifecycle.probe();
        }
        refresh_details(cx);
    }
}

pub(super) fn failure_reason(failure: &RespawnFailure) -> SharedString {
    match failure {
        RespawnFailure::KeepsStopping => "keeps stopping".into(),
        RespawnFailure::EndpointHeld => "another process holds its pipe name".into(),
        RespawnFailure::VersionMismatch => "a daemon from another build is running".into(),
        RespawnFailure::Other(text) => SharedString::from(text.to_string()),
    }
}
