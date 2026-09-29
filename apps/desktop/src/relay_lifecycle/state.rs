//! What every daemon surface reads: one global, kept current by one
//! foreground loop that drains the lifecycle's events.

use futures::StreamExt as _;
use gpui::{App, AsyncApp, Global, SharedString};

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

pub struct RelayDaemonState {
    pub status: DaemonStatus,
    pub busy: Option<Busy>,
    pub stale: Option<StaleInfo>,
}

impl Global for RelayDaemonState {}

/// Install the global and start the loop that keeps it current. Called once,
/// at app init, after the relay boot.
pub fn install(cx: &mut App) {
    let Some(lifecycle) = lifecycle() else {
        cx.set_global(RelayDaemonState { status: DaemonStatus::InProcess, busy: None, stale: None });
        return;
    };
    cx.set_global(RelayDaemonState {
        status: DaemonStatus::Running {
            pid: lifecycle.pid(),
            session_id: lifecycle.current_session(),
        },
        busy: None,
        stale: lifecycle.take_stale_at_boot(),
    });
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
    let state = cx.global_mut::<RelayDaemonState>();
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
            state.status = DaemonStatus::Unreachable { reason: "not responding".into() };
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
}

fn failure_reason(failure: &RespawnFailure) -> SharedString {
    match failure {
        RespawnFailure::KeepsStopping => "keeps stopping".into(),
        RespawnFailure::EndpointHeld => "another process holds its pipe name".into(),
        RespawnFailure::VersionMismatch => "a daemon from another build is running".into(),
        RespawnFailure::Other(text) => SharedString::from(text.to_string()),
    }
}
