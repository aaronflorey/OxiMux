//! Floating status popover — a borderless panel **window** hosting a card the
//! status bar opens (the usage meter's, the terminal daemon's) above the
//! inline browser.
//!
//! Why a separate window: the inline browser's webview is a native view
//! layered over the GPU canvas, so an in-window GPUI element can't be drawn on
//! top of a visible page. A `WindowKind::PopUp` panel composites at the
//! popup window level (above every normal window's native child views), so the
//! themed card floats over the page without hiding it.
//!
//! Dismissal (the tricky part): GPUI has no cross-window "click outside" event,
//! so the panel opens focused and closes when it resigns key — i.e. the moment
//! the user clicks anything else — plus an explicit Escape. The owner debounces
//! re-open so the same click that dismisses (on the status-bar chip) doesn't
//! immediately reopen it.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Context, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, Styled, Subscription, Task, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowId, WindowKind, WindowOptions, div, point, px,
    size,
};
use oximux_agents::session_log::now_unix_ms;
use oximux_agents::session_log::usage::ProviderUsage;
use oximux_settings::{Density, Theme, Typography};

use super::{awake_card, daemon_card};
use super::status_bar::StatusPopoverKind;
use crate::relay_lifecycle::state::{RelayDaemonState, refresh_details_if_stale};
use crate::relay_lifecycle::ui;
use crate::shell::usage_meter;
use crate::workspace_root::WorkspaceRoot;

/// Re-open is suppressed for this long after a dismissal so the chip click that
/// closes the popover (which also resigns the panel's key status) doesn't race
/// straight back into a re-open.
const REOPEN_DEBOUNCE_MS: i64 = 300;

/// When each kind's card last closed (unix ms) and which window owned it,
/// written *synchronously* the instant the panel resigns key (the owner's
/// entity-stored handle is cleared a turn later via `defer`, which is too late
/// for the re-open debounce). Per kind, so clicking one chip while the other's
/// card is open opens it; per window, so a click in another window that closes
/// this card is not mistaken for the click that dismissed it there.
static LAST_CLOSED: Mutex<[Option<(i64, WindowId)>; 3]> = Mutex::new([None, None, None]);

fn slot(kind: StatusPopoverKind) -> usize {
    match kind {
        StatusPopoverKind::Usage => 0,
        StatusPopoverKind::Daemon => 1,
        StatusPopoverKind::Awake => 2,
    }
}

/// How often an open keep-awake card re-reads its state. The window never
/// re-renders on its own, and holds change from everywhere (an agent starting,
/// a phone binding), so without it an open card would go stale.
const AWAKE_CARD_TICK: Duration = Duration::from_secs(2);

/// Record that `owner`'s `kind` card just closed.
fn note_closed(kind: StatusPopoverKind, owner: WindowId) {
    LAST_CLOSED.lock().unwrap_or_else(|p| p.into_inner())[slot(kind)] = Some((now_unix_ms(), owner));
}

/// Whether `owner`'s `kind` card closed within [`REOPEN_DEBOUNCE_MS`] — the
/// chip click that dismissed it, which must not reopen it.
pub fn just_closed(kind: StatusPopoverKind, owner: WindowId) -> bool {
    let last = LAST_CLOSED.lock().unwrap_or_else(|p| p.into_inner())[slot(kind)];
    last.is_some_and(|(at, window)| window == owner && now_unix_ms() - at < REOPEN_DEBOUNCE_MS)
}

const MARGIN: f32 = 8.0;

/// What a popover shows, snapshotted or read live.
pub enum StatusPopoverBody {
    /// The usage card, from the rows at open time.
    Usage(Vec<ProviderUsage>),
    /// The daemon card, read live from `RelayDaemonState`. Its verbs run in
    /// the owner window, where the confirm opens.
    Daemon,
    /// The keep-awake card, read live from the process-global holder.
    Awake,
}

impl StatusPopoverBody {
    fn kind(&self) -> StatusPopoverKind {
        match self {
            Self::Usage(_) => StatusPopoverKind::Usage,
            Self::Daemon => StatusPopoverKind::Daemon,
            Self::Awake => StatusPopoverKind::Awake,
        }
    }
}

/// The popup window's root view: renders its card and owns the dismiss
/// triggers (resign-key + Escape).
pub struct StatusPopover {
    body: StatusPopoverBody,
    theme: Theme,
    density: Density,
    typography: Typography,
    owner: WeakEntity<WorkspaceRoot>,
    /// The window whose status bar opened this card.
    owner_window: AnyWindowHandle,
    focus_handle: FocusHandle,
    /// Set once the panel has actually become key, so the initial
    /// not-yet-active tick doesn't dismiss it before it ever shows.
    seen_active: bool,
    _activation: Subscription,
    /// The keep-awake card's refresh; `None` for the other cards.
    _tick: Option<Task<()>>,
}

impl StatusPopover {
    #[allow(clippy::too_many_arguments)]
    fn new(
        body: StatusPopoverBody,
        theme: Theme,
        density: Density,
        typography: Typography,
        owner: WeakEntity<WorkspaceRoot>,
        owner_window: AnyWindowHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.seen_active = true;
            } else if this.seen_active {
                this.dismiss(window, cx);
            }
        });
        let tick = matches!(body, StatusPopoverBody::Awake).then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(AWAKE_CARD_TICK).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            })
        });
        Self {
            body,
            theme,
            density,
            typography,
            owner,
            owner_window,
            focus_handle,
            seen_active: false,
            _activation: activation,
            _tick: tick,
        }
    }

    fn dismiss(&self, window: &mut Window, cx: &mut Context<Self>) {
        close(self.body.kind(), self.owner.clone(), self.owner_window, window, cx);
    }
}

/// Close a popover window and tell its owner. Stamps the close time
/// synchronously so the chip toggle can swallow the very click that dismissed
/// it; the owner's handle is cleared on a later turn, never inline — this runs
/// from the activation observer, which fires *during* the same chip click whose
/// handler is also updating `WorkspaceRoot`, and a nested `update` on that
/// entity would panic.
fn close(
    kind: StatusPopoverKind,
    owner: WeakEntity<WorkspaceRoot>,
    owner_window: AnyWindowHandle,
    window: &mut Window,
    cx: &mut App,
) {
    note_closed(kind, owner_window.window_id());
    window.remove_window();
    cx.defer(move |cx| {
        let _ = owner.update(cx, |root, _| root.note_status_popover_closed(kind));
    });
}

/// Close the `kind` card, bring its owner window forward and run `verb` there —
/// a daemon verb's confirm, or a Settings pane, is modal to that window, not to
/// this one.
fn close_then_in_owner(
    kind: StatusPopoverKind,
    owner: WeakEntity<WorkspaceRoot>,
    owner_window: AnyWindowHandle,
    window: &mut Window,
    cx: &mut App,
    verb: impl FnOnce(&mut WorkspaceRoot, &mut Window, &mut Context<WorkspaceRoot>) + 'static,
) {
    close(kind, owner.clone(), owner_window, window, cx);
    cx.defer(move |cx| {
        let _ = owner_window.update(cx, |_, window, cx| {
            window.activate_window();
            let _ = owner.update(cx, |root, cx| verb(root, window, cx));
        });
    });
}

/// A daemon card verb, as a click handler.
fn in_owner_window(
    owner: WeakEntity<WorkspaceRoot>,
    owner_window: AnyWindowHandle,
    verb: fn(&mut WorkspaceRoot, &mut Window, &mut Context<WorkspaceRoot>),
) -> impl Fn(&mut Window, &mut App) + 'static {
    move |window, cx| {
        close_then_in_owner(StatusPopoverKind::Daemon, owner.clone(), owner_window, window, cx, verb);
    }
}

impl Focusable for StatusPopover {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StatusPopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        let card = match &self.body {
            StatusPopoverBody::Usage(rows) => usage_meter::render_usage_popover(
                rows,
                now_unix_ms(),
                self.theme,
                self.density,
                &self.typography,
            )
            .into_any_element(),
            StatusPopoverBody::Daemon => {
                refresh_details_if_stale(cx);
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
                let view = ui::daemon_view(cx.try_global::<RelayDaemonState>(), now);
                daemon_card::render(
                    &view,
                    self.theme,
                    self.density,
                    &self.typography,
                    in_owner_window(self.owner.clone(), self.owner_window, WorkspaceRoot::open_restart_confirm),
                    in_owner_window(self.owner.clone(), self.owner_window, WorkspaceRoot::open_kill_all_confirm),
                )
            }
            StatusPopoverBody::Awake => {
                let status = crate::agent_awake::global().status();
                let owner = self.owner.clone();
                let (pane_owner, owner_window) = (self.owner.clone(), self.owner_window);
                awake_card::render(
                    &status,
                    self.theme,
                    self.density,
                    &self.typography,
                    move |mode, window, cx| {
                        let _ = owner.update(cx, |root, cx| root.select_awake_mode(mode, cx));
                        window.refresh();
                    },
                    move |pane, window, cx| {
                        close_then_in_owner(
                            StatusPopoverKind::Awake,
                            pane_owner.clone(),
                            owner_window,
                            window,
                            cx,
                            move |root, window, cx| root.open_settings_pane(pane, window, cx),
                        );
                    },
                )
            }
        };
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    this.dismiss(window, cx);
                }
            }))
            .child(card)
    }
}

/// Open a floating status popover anchored to the status-bar's bottom-right
/// corner, returning its handle for the owner to track (so a second chip click
/// can dismiss it).
pub fn open(
    body: StatusPopoverBody,
    theme: Theme,
    density: Density,
    typography: Typography,
    owner: WeakEntity<WorkspaceRoot>,
    window: &mut Window,
    cx: &mut App,
) -> Option<gpui::WindowHandle<StatusPopover>> {
    // Sized to the content: the window has to be sized before anything
    // renders, so each card's own measurement function is the only place this
    // can come from. The usage card grows with each configured account.
    let popover = match &body {
        StatusPopoverBody::Usage(rows) => size(
            px(usage_meter::POPOVER_WIDTH),
            px(usage_meter::popover_height(rows, density, &typography)),
        ),
        StatusPopoverBody::Daemon => size(
            px(daemon_card::CARD_WIDTH),
            px(daemon_card::card_height(density, &typography)),
        ),
        // The worst case: the window is never resized after it opens.
        StatusPopoverBody::Awake => size(
            px(awake_card::CARD_WIDTH),
            px(awake_card::card_height(density, &typography)),
        ),
    };
    let main = window.bounds();
    // Bottom-right of the main window, just above the status bar.
    let origin = point(
        main.origin.x + main.size.width - popover.width - px(MARGIN),
        main.origin.y + main.size.height - popover.height - px(density.h_status_bar) - px(MARGIN),
    );
    let display_id = window.display(cx).map(|d| d.id());
    let owner_window = window.window_handle();

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin,
            size: popover,
        })),
        titlebar: None,
        kind: WindowKind::PopUp,
        focus: true,
        show: true,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        display_id,
        ..Default::default()
    };

    cx.open_window(options, move |window, cx| {
        cx.new(|cx| StatusPopover::new(body, theme, density, typography, owner, owner_window, window, cx))
    })
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The click that dismissed a card must not reopen it — but only the click
    // on that window's chip, and only for that card: a click in another
    // window, or on the other chip, opens its card at once.
    #[test]
    fn only_the_dismissing_click_is_swallowed() {
        let (a, b) = (WindowId::from(1), WindowId::from(2));
        note_closed(StatusPopoverKind::Daemon, a);
        assert!(just_closed(StatusPopoverKind::Daemon, a));
        assert!(!just_closed(StatusPopoverKind::Daemon, b), "another window's chip");
        assert!(!just_closed(StatusPopoverKind::Usage, a), "the other chip");
        assert!(!just_closed(StatusPopoverKind::Awake, a), "each chip has its own slot");
    }
}
