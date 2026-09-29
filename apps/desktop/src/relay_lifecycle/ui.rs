//! What the user sees of the daemon's lifecycle: the confirm copy for Restart
//! and Kill all, the toasts their outcomes and the daemon's own events raise,
//! the Settings status line, and the entry points every surface shares — the
//! palette, a toast's button, a Settings chip — so each goes through the one
//! confirm.
//!
//! The copy is built by pure functions over the outcome, so it is tested
//! without a daemon or a window.

use std::time::{Duration, Instant};

use gpui::{App, Context, Window};

use super::state::{Busy, DaemonStatus, RelayDaemonState, failure_reason};
use super::{
    KillAllError, KillAllOutcome, RelayLifecycleEvent, RespawnFailure, RespawnReason, RestartError,
    RestartOutcome, lifecycle,
};
use crate::platform::window_registry;
use oximux_relay_supervisor::StopError;
use crate::shell::toast::{ToastAction, ToastKind, toast, toast_with_actions};
use crate::workspace_root::WorkspaceRoot;
use crate::workspace_root::kill_all::SessionSplit;

/// At most one of each daemon alert (not responding, keeps stopping, pipe
/// held) per this window: they tend to arrive in bursts, and the first says
/// it all. Per alert, so a mild one never hides a worse one.
const ALERT_DEBOUNCE: Duration = Duration::from_secs(30);

/// A toast to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: ToastKind,
    pub text: String,
    /// Offer a "Restart" button (it opens the restart confirm).
    pub offer_restart: bool,
    /// Offer an "Open log" button.
    pub offer_log: bool,
    /// Debounced with the other daemon alerts.
    pub alert: bool,
}

impl Notice {
    fn new(kind: ToastKind, text: impl Into<String>) -> Self {
        Self { kind, text: text.into(), offer_restart: false, offer_log: false, alert: false }
    }

    fn with_restart(mut self) -> Self {
        self.offer_restart = true;
        self
    }

    fn with_log(mut self) -> Self {
        self.offer_log = true;
        self
    }

    fn alerting(mut self) -> Self {
        self.alert = true;
        self
    }
}

fn sessions(n: usize) -> String {
    if n == 1 { "1 terminal session".into() } else { format!("{n} terminal sessions") }
}

const SERVE_NOTE: &str = "oximux serve is also using this daemon and must be restarted afterwards.";

/// The restart confirm's body. `sessions` is `None` when the daemon could not
/// be asked.
pub fn restart_body(count: Option<usize>, foreign_serve: bool) -> String {
    let who = match count {
        Some(n) => format!("{} will restart", sessions(n)),
        None => "Every terminal session will restart".into(),
    };
    let mut body = format!(
        "{who}, including agent CLI tabs and terminals inside agent chats. Shells come back \
         with their scrollback, and agent CLIs resume their conversation. A command an agent \
         chat is running in its terminal will be stopped."
    );
    if foreign_serve {
        body.push(' ');
        body.push_str(SERVE_NOTE);
    }
    body
}

/// The kill-all confirm's body.
pub fn kill_all_body(split: SessionSplit) -> String {
    let total = split.visible + split.other;
    let mut parts = Vec::new();
    if split.visible > 0 {
        parts.push(format!("{} in open terminal and agent CLI tabs, which close", split.visible));
    }
    if split.other > 0 {
        parts.push(format!(
            "{} {}in background projects, terminals inside agent chats and oximux serve",
            split.other,
            if split.visible > 0 { "more " } else { "" }
        ));
    }
    let scope = if parts.is_empty() { String::new() } else { format!(": {}", parts.join(", and ")) };
    format!(
        "Ends all {}{scope}. The daemon keeps running, and agent chat conversations are kept.",
        sessions(total)
    )
}

/// What a finished restart tells the user; `None` when there is nothing to
/// say (the app is quitting).
pub fn restart_notice(result: &Result<RestartOutcome, RestartError>, foreign_serve: bool) -> Option<Notice> {
    let notice = match result {
        Ok(_) if foreign_serve => Notice::new(
            ToastKind::Success,
            "Terminal daemon restarted. Restart oximux serve to use the new one.",
        ),
        Ok(_) => Notice::new(ToastKind::Success, "Terminal daemon restarted."),
        Err(RestartError::Quitting) => return None,
        // It had agreed to stop — its sessions are gone and it takes no new
        // ones — but could not be finished off.
        Err(RestartError::Stop(
            StopError::IdentityUnknown { shutdown_accepted: true, .. }
            | StopError::DaemonSurvived { shutdown_accepted: true, .. },
        )) => Notice::new(
            ToastKind::Error,
            "The daemon stopped its sessions but did not exit, so no new one started. \
             Relaunch OxiMux. See the log.",
        )
        .with_log(),
        Err(RestartError::Stop(StopError::IdentityUnknown { .. })) => {
            Notice::new(ToastKind::Error, "Couldn't verify the daemon process — not restarted. See the log.")
                .with_log()
        }
        Err(RestartError::Stop(err)) => {
            Notice::new(ToastKind::Error, format!("Restart failed — {err}.")).with_log()
        }
        Err(RestartError::Respawn(failure)) => Notice::new(
            ToastKind::Error,
            format!(
                "Restart failed — the new daemon did not start ({}). New terminals run inside \
                 OxiMux until it is relaunched.",
                failure_reason(failure)
            ),
        )
        .with_log(),
        Err(RestartError::Internal) => {
            Notice::new(ToastKind::Error, "Restart failed unexpectedly. See the log.").with_log()
        }
    };
    Some(notice)
}

/// What a finished kill-all tells the user. `total` is the count the confirm
/// showed: the sweep's own count misses the sessions the tabs had already
/// begun closing.
pub fn kill_all_notice(total: usize, result: &Result<KillAllOutcome, KillAllError>) -> Option<Notice> {
    let notice = match result {
        Ok(KillAllOutcome { after: 0, .. }) => {
            Notice::new(ToastKind::Success, format!("Ended {}.", sessions(total)))
        }
        // The daemon answers a close only once its session has ended, so a
        // count here is a close it refused or never answered.
        Ok(KillAllOutcome { after, .. }) => Notice::new(
            ToastKind::Warning,
            format!("Couldn't confirm that {} ended.", sessions(*after)),
        )
        .with_restart()
        .with_log(),
        Err(KillAllError::Replaced) => Notice::new(
            ToastKind::Info,
            "The terminal daemon restarted meanwhile — nothing else was ended.",
        ),
        Err(KillAllError::Unreachable(reason)) => Notice::new(
            ToastKind::Error,
            format!("Kill all failed — the daemon did not answer ({reason})."),
        )
        .with_restart()
        .with_log(),
        Err(KillAllError::Quitting) => return None,
        Err(KillAllError::Internal) => {
            Notice::new(ToastKind::Error, "Kill all failed unexpectedly. See the log.").with_log()
        }
    };
    Some(notice)
}

/// Kill all asked the daemon what to end and heard nothing back in time.
pub fn kill_all_unanswered_notice() -> Notice {
    Notice::new(
        ToastKind::Warning,
        "The terminal daemon did not answer, so nothing was ended. Restart it instead.",
    )
    .with_restart()
    .with_log()
}

/// What a lifecycle event tells the user, beyond the status it updates. A
/// manual restart's own outcome is reported by [`restart_notice`], so its
/// events say nothing here.
pub fn event_notice(event: &RelayLifecycleEvent) -> Option<Notice> {
    let notice = match event {
        RelayLifecycleEvent::RespawnFailed { reason: RespawnReason::Manual, .. } => return None,
        RelayLifecycleEvent::RespawnFailed { failure, .. } => match failure {
            RespawnFailure::KeepsStopping => {
                Notice::new(ToastKind::Error, "Terminal daemon keeps stopping — it was not restarted.")
                    .with_restart()
                    .with_log()
            }
            RespawnFailure::EndpointHeld => Notice::new(
                ToastKind::Error,
                "Another process is holding the terminal daemon's pipe name.",
            )
            .with_log(),
            other => Notice::new(
                ToastKind::Error,
                format!(
                    "Terminal daemon could not be restarted ({}). New terminals run inside \
                     OxiMux until it is relaunched.",
                    failure_reason(other)
                ),
            )
            .with_log(),
        }
        .alerting(),
        RelayLifecycleEvent::Probed { responsive: false } => {
            Notice::new(ToastKind::Warning, "Terminal daemon is not responding.")
                .with_restart()
                .with_log()
                .alerting()
        }
        RelayLifecycleEvent::PreviousDaemonRetired { foreign_serve } => Notice::new(
            ToastKind::Info,
            if *foreign_serve {
                "Terminal daemon updated — terminals were restarted. Restart oximux serve too."
            } else {
                "Terminal daemon updated — terminals were restarted."
            },
        ),
        RelayLifecycleEvent::Respawned { .. }
        | RelayLifecycleEvent::RestartFailed { .. }
        | RelayLifecycleEvent::Probed { responsive: true } => return None,
    };
    Some(notice)
}

/// The unreachable reason a failed probe sets.
pub const NOT_RESPONDING: &str = "not responding";

/// How the Settings status line reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Busy,
    Warn,
    Error,
}

/// The Settings status line: `running · PID 84589 · up 3d 4h · 7 sessions`.
pub fn status_line(state: &RelayDaemonState, now_epoch_secs: u64) -> (String, Tone) {
    match (&state.busy, &state.status) {
        (Some(Busy::Restarting), _) => ("restarting…".into(), Tone::Busy),
        (Some(Busy::KillingAll), _) => ("killing sessions…".into(), Tone::Busy),
        (None, DaemonStatus::InProcess) => ("in-process (daemon unavailable)".into(), Tone::Warn),
        (None, DaemonStatus::Unreachable { reason }) if reason == NOT_RESPONDING => {
            (NOT_RESPONDING.into(), Tone::Error)
        }
        (None, DaemonStatus::Unreachable { reason }) => (format!("unavailable — {reason}"), Tone::Error),
        (None, DaemonStatus::Running { pid, .. }) => {
            let mut line = String::from("running");
            if let Some(pid) = pid {
                line.push_str(&format!(" · PID {pid}"));
            }
            let details = state.details.as_ref();
            if let Some(started) = details.and_then(|d| d.record.as_ref()).map(|r| r.started_at_epoch_secs)
                && started > 0
            {
                line.push_str(&format!(" · up {}", uptime(now_epoch_secs.saturating_sub(started))));
            }
            if let Some(n) = details.and_then(|d| d.sessions) {
                line.push_str(&format!(" · {n} session{}", if n == 1 { "" } else { "s" }));
            }
            (line, Tone::Ok)
        }
    }
}

/// `3d 4h`, `4h 12m`, `12m`, `<1m`.
pub fn uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60);
    match (d, h, m) {
        (0, 0, 0) => "<1m".into(),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// The long operation running, if any. While set, Restart and Kill all do
/// nothing.
pub fn busy(cx: &App) -> Option<Busy> {
    cx.try_global::<RelayDaemonState>().and_then(|s| s.busy)
}

pub fn set_busy(cx: &mut App, busy: Option<Busy>) {
    if cx.has_global::<RelayDaemonState>() {
        cx.global_mut::<RelayDaemonState>().busy = busy;
        cx.refresh_windows();
    }
}

/// Show `notice` as a toast on the active window. An alert within
/// [`ALERT_DEBOUNCE`] of the same one is dropped.
pub fn show(cx: &mut App, notice: Notice) {
    if cx.has_global::<RelayDaemonState>() {
        let state = cx.global_mut::<RelayDaemonState>();
        if notice.alert {
            if state.last_alerts.get(&notice.text).is_some_and(|at| at.elapsed() < ALERT_DEBOUNCE) {
                return;
            }
            state.last_alerts.insert(notice.text.clone(), Instant::now());
        }
        // Offering a restart is saying the daemon is down or unanswered.
        if notice.alert || notice.offer_restart {
            state.down_notices.insert(notice.text.clone());
        }
    }
    let mut actions = Vec::new();
    if notice.offer_restart {
        actions.push(ToastAction::new("Restart", request_restart));
    }
    if notice.offer_log {
        actions.push(ToastAction::new("Open log", open_log));
    }
    if actions.is_empty() {
        toast(cx, notice.kind, notice.text);
    } else {
        toast_with_actions(cx, notice.kind, notice.text, actions);
    }
}

/// The daemon is up again: take down, in every window, the notices that said
/// it was not, and let the next failure alert at once rather than after the
/// debounce.
pub(super) fn retract_alerts(cx: &mut App) {
    if !cx.has_global::<RelayDaemonState>() {
        return;
    }
    let state = cx.global_mut::<RelayDaemonState>();
    state.last_alerts.clear();
    let stale = std::mem::take(&mut state.down_notices);
    if stale.is_empty() {
        return;
    }
    // Every window, not only the active one: an alert raised while another
    // window was in front is still up there.
    for (_, root) in window_registry::all_windows(cx) {
        let layer = root.read(cx).toast_layer.clone();
        layer.update(cx, |layer, cx| layer.dismiss_matching(|text| stale.contains(text), cx));
    }
}

/// The daemon's log file, when the daemon runs.
pub fn log_path() -> Option<std::path::PathBuf> {
    lifecycle().map(|l| l.supervisor().log_path())
}

/// Reveal the daemon's log in the file manager — its folder, if the file is
/// not there yet.
pub fn open_log(cx: &mut App) {
    let Some(path) = log_path() else {
        return;
    };
    if path.exists() {
        cx.reveal_path(&path);
    } else if let Some(dir) = path.parent() {
        cx.reveal_path(dir);
    }
}

/// Open the restart confirm in the active window. Deferred, so any handler —
/// a toast's button, a Settings chip — may call it.
pub fn request_restart(cx: &mut App) {
    with_active_root(cx, |root, window, cx| root.open_restart_confirm(window, cx));
}

/// Open the kill-all confirm in the active window. Deferred like
/// [`request_restart`].
pub fn request_kill_all(cx: &mut App) {
    with_active_root(cx, |root, window, cx| root.open_kill_all_confirm(window, cx));
}

fn with_active_root(
    cx: &mut App,
    f: impl FnOnce(&mut WorkspaceRoot, &mut Window, &mut Context<WorkspaceRoot>) + 'static,
) {
    cx.defer(move |cx| {
        // The active window, else any workspace window: a panel or spike
        // window has no root to open a confirm in.
        let active = cx
            .active_window()
            .and_then(|h| window_registry::workspace_for_window(cx, h.window_id()).map(|root| (h, root)));
        let target = active.or_else(|| {
            window_registry::all_windows(cx)
                .into_iter()
                .find_map(|(id, root)| window_registry::window_handle(cx, &id).map(|h| (h, root)))
        });
        let Some((handle, root)) = target else {
            return;
        };
        let _ = handle.update(cx, |_, window, cx| root.update(cx, |root, cx| f(root, window, cx)));
    });
}

#[cfg(test)]
mod tests;
