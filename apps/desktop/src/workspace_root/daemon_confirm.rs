//! The Restart and Kill all confirms, mounted busy: confirming keeps the
//! dialog up with a spinner until the daemon work finishes, then the outcome
//! shows as a toast. Every surface (palette, toast button, Settings) reaches
//! these through `relay_lifecycle::ui`.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{Either, select};
use gpui::{AnyWindowHandle, App, AsyncApp, AsyncWindowContext, Context, Focusable as _, WeakEntity, Window};

use super::WorkspaceRoot;
use super::kill_all::{SessionSplit, close_every_terminal_tab, shown_pty_ids, split_counts};
use crate::relay_lifecycle::state::Busy;
use crate::relay_lifecycle::{KillAllError, RelayLifecycle, lifecycle, ui};
use crate::shell::confirm_dialog::{ConfirmCallback, ConfirmPrompt};
use crate::shell::toast::ToastKind;

/// How long a confirm waits to count the daemon's sessions. A daemon that
/// takes longer is likely the wedged one the user is restarting.
const PRE_COUNT_TIMEOUT: Duration = Duration::from_secs(2);

/// The daemon's sessions, or `None` when it did not answer in time.
async fn pre_count(
    lifecycle: &RelayLifecycle,
    cx: &AsyncWindowContext,
) -> Option<Result<Vec<String>, KillAllError>> {
    let listed = std::pin::pin!(lifecycle.list_pty_ids());
    let timer = cx.background_executor().timer(PRE_COUNT_TIMEOUT);
    match select(listed, timer).await {
        Either::Left((listed, _)) => Some(listed),
        Either::Right(_) => None,
    }
}

impl WorkspaceRoot {
    /// Ask before restarting the daemon, with how many sessions it ends.
    pub(crate) fn open_restart_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lifecycle) = self.daemon_ready(cx) else {
            return;
        };
        cx.spawn_in(window, async move |root, cx| {
            let count = pre_count(&lifecycle, cx).await.and_then(Result::ok).map(|ids| ids.len());
            let foreign_serve = lifecycle.foreign_serve();
            let _ = root.update_in(cx, |root, window, cx| {
                let prompt = ConfirmPrompt {
                    title: "Restart the terminal daemon?".into(),
                    body: ui::restart_body(count, foreign_serve).into(),
                    on_confirm: restart_on_confirm(lifecycle, foreign_serve, cx.weak_entity()),
                    confirm_label: Some("Restart".into()),
                    on_cancel: Some(restore_focus_on_cancel(cx.weak_entity())),
                    secondary: None,
                };
                root.mount_busy_confirm_dialog(prompt, "Restarting…", window, cx);
            });
        })
        .detach();
    }

    /// Ask before ending every session, split into the ones open tabs show
    /// and the rest.
    pub(crate) fn open_kill_all_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lifecycle) = self.daemon_ready(cx) else {
            return;
        };
        cx.spawn_in(window, async move |root, cx| {
            let listed = match pre_count(&lifecycle, cx).await {
                Some(Ok(listed)) => listed,
                None => {
                    cx.update(|_, cx| ui::show(cx, ui::kill_all_unanswered_notice())).ok();
                    return;
                }
                Some(Err(err)) => {
                    if let Some(notice) = ui::kill_all_notice(0, &Err(err)) {
                        cx.update(|_, cx| ui::show(cx, notice)).ok();
                    }
                    return;
                }
            };
            // Read outside this root's update: the walk reads every window's
            // root, this one included.
            let Ok(shown) = cx.update(|_, cx| shown_pty_ids(cx)) else {
                return;
            };
            let split = split_counts(&listed, &shown);
            if split == SessionSplit::default() {
                cx.update(|_, cx| crate::shell::toast::toast(cx, ToastKind::Info, "No terminal sessions are running."))
                    .ok();
                return;
            }
            let _ = root.update_in(cx, |root, window, cx| {
                let prompt = ConfirmPrompt {
                    title: "Kill all terminal sessions?".into(),
                    body: ui::kill_all_body(split).into(),
                    on_confirm: kill_all_on_confirm(lifecycle, split, cx.weak_entity()),
                    confirm_label: Some("Kill all".into()),
                    on_cancel: Some(restore_focus_on_cancel(cx.weak_entity())),
                    secondary: None,
                };
                root.mount_busy_confirm_dialog(prompt, "Killing…", window, cx);
            });
        })
        .detach();
    }

    /// The lifecycle, when a restart or kill-all may start now; says why not
    /// otherwise.
    fn daemon_ready(&self, cx: &mut Context<Self>) -> Option<Arc<RelayLifecycle>> {
        if self.confirm_pending(cx) {
            return None;
        }
        if ui::busy(cx).is_some() {
            crate::shell::toast::toast(
                cx,
                ToastKind::Info,
                "The terminal daemon is busy — try again when it finishes.",
            );
            return None;
        }
        let lifecycle = lifecycle();
        if lifecycle.is_none() {
            crate::shell::toast::toast(
                cx,
                ToastKind::Info,
                "The terminal daemon is not running. Relaunch OxiMux to start it.",
            );
        }
        lifecycle
    }

    /// Mount a confirm that stays up, busy, from the click until
    /// [`Self::finish_confirm_dialog`]. `false` when another confirm is up.
    pub(crate) fn mount_busy_confirm_dialog(
        &mut self,
        prompt: ConfirmPrompt,
        busy_label: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.mount_confirm_dialog(prompt, window, cx) {
            return false;
        }
        if let Some(dialog) = &self.confirm_dialog {
            dialog.update(cx, |d, _| d.set_busy_on_confirm(busy_label));
        }
        true
    }

    /// Resolve the busy confirm — the only kind that can still be up once its
    /// work finishes — and give focus back.
    pub(crate) fn finish_confirm_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dialog) = self.confirm_dialog.clone() {
            dialog.update(cx, |d, cx| d.finish(cx));
        }
        self.restore_focus_after_confirm(window, cx);
    }

    /// Focus what the confirm was opened over: Settings, if it is still open
    /// (so Escape closes it and nothing types into a pane hidden behind it),
    /// else the active pane.
    fn restore_focus_after_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_modal.read(cx).is_open() {
            let (settings, root) = (self.settings_modal.clone(), cx.weak_entity());
            // Checked again when it runs: a Settings that closed meanwhile
            // leaves its handle unrendered, and focus there reaches nothing.
            window.defer(cx, move |window, cx| {
                if settings.read(cx).is_open() {
                    settings.read(cx).focus_handle(cx).focus(window, cx);
                } else {
                    let _ = root.update(cx, |root, cx| {
                        crate::shell::workspace_ops::refocus_active_pane(root, window, cx)
                    });
                }
            });
        } else {
            crate::shell::workspace_ops::refocus_active_pane(self, window, cx);
        }
    }
}

/// A cancelled confirm drops its own focus; hand it back.
fn restore_focus_on_cancel(root: WeakEntity<WorkspaceRoot>) -> ConfirmCallback {
    Rc::new(move |window, cx| {
        let _ = root.update(cx, |root, cx| root.restore_focus_after_confirm(window, cx));
    })
}

/// Start the restart; when it ends, clear the busy flag, resolve the dialog
/// and report.
fn restart_on_confirm(
    lifecycle: Arc<RelayLifecycle>,
    foreign_serve: bool,
    root: WeakEntity<WorkspaceRoot>,
) -> ConfirmCallback {
    Rc::new(move |window, cx| {
        if !claim_busy(window, cx, &root, Busy::Restarting) {
            return;
        }
        let restart = lifecycle.restart();
        let (root, handle) = (root.clone(), window.window_handle());
        cx.spawn(async move |cx: &mut AsyncApp| {
            let result = restart.await;
            let notice = ui::restart_notice(&result, foreign_serve);
            cx.update(|cx| finished(cx, handle, &root, Busy::Restarting, notice));
        })
        .detach();
    })
}

/// Close the tabs, sweep the daemon; when it ends, clear the busy flag,
/// resolve the dialog and report against the count the confirm showed.
fn kill_all_on_confirm(
    lifecycle: Arc<RelayLifecycle>,
    split: SessionSplit,
    root: WeakEntity<WorkspaceRoot>,
) -> ConfirmCallback {
    Rc::new(move |window, cx| {
        if !claim_busy(window, cx, &root, Busy::KillingAll) {
            return;
        }
        let (lifecycle, root, handle) = (Arc::clone(&lifecycle), root.clone(), window.window_handle());
        // A task, not inline: closing tabs updates every window, and this
        // callback runs inside the dialog's.
        cx.spawn(async move |cx: &mut AsyncApp| {
            let shown = cx.update(|cx| {
                let shown = shown_pty_ids(cx);
                close_every_terminal_tab(cx);
                shown
            });
            let result = lifecycle.kill_all_sessions(shown.into_iter().collect()).await;
            let total = split.visible + split.other;
            let notice = ui::kill_all_notice(total, &result);
            cx.update(|cx| finished(cx, handle, &root, Busy::KillingAll, notice));
        })
        .detach();
    })
}

/// Mark `busy` as running, unless another window's confirm got there first —
/// then say so and resolve this dialog, which confirming left busy.
fn claim_busy(window: &mut Window, cx: &mut App, root: &WeakEntity<WorkspaceRoot>, busy: Busy) -> bool {
    if ui::busy(cx).is_none() {
        ui::set_busy(cx, Some(busy));
        return true;
    }
    let (root, handle) = (root.clone(), window.window_handle());
    // Deferred: this runs inside the dialog's own update.
    cx.defer(move |cx| {
        let _ = handle.update(cx, |_, window, cx| {
            root.update(cx, |root, cx| root.finish_confirm_dialog(window, cx))
        });
        crate::shell::toast::toast(
            cx,
            ToastKind::Info,
            "The terminal daemon is busy — try again when it finishes.",
        );
    });
    false
}

/// Clear `busy` (if it is still this operation's), resolve the dialog and
/// report.
fn finished(
    cx: &mut App,
    handle: AnyWindowHandle,
    root: &WeakEntity<WorkspaceRoot>,
    busy: Busy,
    notice: Option<ui::Notice>,
) {
    if ui::busy(cx) == Some(busy) {
        ui::set_busy(cx, None);
    }
    let root = root.clone();
    let _ = handle.update(cx, |_, window, cx| {
        root.update(cx, |root, cx| root.finish_confirm_dialog(window, cx))
    });
    if let Some(notice) = notice {
        ui::show(cx, notice);
    }
}
