//! The terminal daemon's card, opened from the status bar's TTY count: its
//! status and version, and Restart / Kill all as icon buttons. On macOS it
//! floats in the status popover window, elsewhere it is drawn in the main
//! window. Either verb closes the card and opens the same confirm every other
//! surface does, so this card only ever asks.

use gpui::{
    AnyElement, App, ClickEvent, Hsla, InteractiveElement, IntoElement, ParentElement, Styled, Window,
    div, px,
};
use gpui::prelude::FluentBuilder as _;
use gpui::StatefulInteractiveElement as _;
use gpui_component::Icon;
use gpui_component::tooltip::Tooltip;
use oximux_settings::{Density, Theme, Typography};

use crate::relay_lifecycle::ui::{DaemonView, Tone};
use crate::shell::usage_meter::line_h;

/// Wide enough for `running · PID 12345 · up 3d 4h · 13 sessions` on one line.
pub const CARD_WIDTH: f32 = 300.0;

/// What a card verb runs.
type Verb = Box<dyn Fn(&mut Window, &mut App)>;

/// The icon buttons, the size of the kit's XSmall.
const BUTTON_H: f32 = 20.0;

/// The colour a status [`Tone`] reads in.
pub fn tone_color(tone: Tone, theme: &Theme) -> Hsla {
    match tone {
        Tone::Ok => theme.status_ok,
        Tone::Busy => theme.status_info,
        Tone::Warn => theme.status_warn,
        Tone::Error => theme.status_error,
    }
}

/// The card's height, from the same tokens it lays out with: a window hosting
/// it is sized before anything renders.
pub fn card_height(density: Density, typography: &Typography) -> f32 {
    let gap = density.gap_inline * 1.5;
    density.pad_panel * 2.0
        + line_h(typography.t_body_sm).max(BUTTON_H)
        + gap
        + 1.0
        + gap
        + line_h(typography.t_body_sm)
        + density.gap_inline
        + line_h(typography.t_sub_label)
        // Border, top and bottom.
        + 2.0
}

pub fn render(
    view: &DaemonView,
    theme: Theme,
    density: Density,
    typography: &Typography,
    on_restart: impl Fn(&mut Window, &mut App) + 'static,
    on_kill: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    // Icon-only, so the tooltip is what names each verb. gpui's own tooltip,
    // not the kit Button's: that one draws through a `Root` overlay, and the
    // popover window has no `Root` (its opaque background would square off
    // the card's corners).
    let icon_button = |id: &'static str,
                       icon: &'static str,
                       tip: &'static str,
                       enabled: bool,
                       on_click: Verb| {
        let hover_bg = theme.hover_overlay;
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .size(px(BUTTON_H))
            .rounded(px(density.r_chip))
            .child(
                Icon::default()
                    .path(icon)
                    .size(px(14.0))
                    .text_color(if enabled { theme.fg_base } else { theme.fg_subtle }),
            )
            .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
            .when(enabled, move |button| {
                button.cursor_pointer().hover(move |s| s.bg(hover_bg)).on_click(
                    move |_: &ClickEvent, window, cx| on_click(window, cx),
                )
            })
    };
    let header = div()
        .flex()
        .items_center()
        .justify_between()
        .h(px(line_h(typography.t_body_sm).max(BUTTON_H)))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_base)
                .child(Icon::default().path("icons/square-terminal.svg").size(px(13.0)).text_color(theme.fg_muted))
                .child("Terminal daemon"),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(2.0))
                .child(icon_button(
                    "daemon-card-restart",
                    "icons/rotate-cw.svg",
                    "Restart daemon",
                    view.can_restart,
                    Box::new(on_restart),
                ))
                .child(icon_button(
                    "daemon-card-kill-all",
                    "icons/trash.svg",
                    "Kill all sessions",
                    view.can_kill,
                    Box::new(on_kill),
                )),
        );
    // One line each: the card's height is fixed before it renders.
    let line = |text: String, size: f32, colour: Hsla| {
        div().w_full().min_w_0().truncate().text_size(px(size)).text_color(colour).child(text)
    };
    div()
        .id("daemon-card")
        .flex()
        .flex_col()
        .size_full()
        .gap(px(density.gap_inline * 1.5))
        .p(px(density.pad_panel))
        .rounded(px(density.r_card))
        .bg(theme.bg_panel)
        .border_1()
        .border_color(theme.border_active)
        // A click on the card itself must not reach the backdrop that closes it.
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(header)
        .child(div().w_full().h(px(1.0)).bg(theme.border_inactive))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(density.gap_inline))
                .child(line(view.status.0.clone(), typography.t_body_sm, tone_color(view.status.1, &theme)))
                .child(line(
                    view.version.0.clone(),
                    typography.t_sub_label,
                    if view.version.1 { theme.status_warn } else { theme.fg_subtle },
                )),
        )
        .into_any_element()
}
