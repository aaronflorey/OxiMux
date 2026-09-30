//! The keep-awake card, opened from the status bar's coffee chip: the mode
//! with whether the machine is being held awake, three rows to change the
//! mode, and — when something other than the mode is holding it — which
//! thing, as a link to the Settings pane that owns it. On macOS it floats in
//! the status popover window, elsewhere it is drawn in the main window.
//!
//! "Off" does not mean "will sleep": remote access and armed schedules hold
//! the machine on their own. So an "Off · Active" card always names its cause.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClickEvent, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::Icon;
use oximux_settings::{Density, Theme, Typography};

use crate::agent_awake::{AwakeMode, AwakeStatus};
use crate::awake_settings::label;
use crate::shell::settings_modal::SettingsPane;
use crate::shell::usage_meter::line_h;

/// Wide enough for the longest row description and footnote on one line,
/// with room to spare so a wider UI face does not clip them.
pub const CARD_WIDTH: f32 = 320.0;

/// The status bar's chip icon.
pub const ICON: &str = "icons/coffee.svg";

/// Vertical padding inside a mode row, around its two lines.
const ROW_PAD_Y: f32 = 4.0;
/// The radio dot's diameter.
const DOT: f32 = 10.0;

const MODES: [(AwakeMode, &str); 3] = [
    (AwakeMode::On, "Keep this computer awake continuously"),
    (AwakeMode::Agent, "Stay awake while an agent is working"),
    (AwakeMode::Off, "Allow normal system sleep behavior"),
];

/// A cause of the hold that the mode does not govern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldBy {
    Remote,
    Schedule,
}

impl HeldBy {
    fn clause(self) -> &'static str {
        match self {
            Self::Remote => "remote access",
            Self::Schedule => "an armed schedule",
        }
    }

    /// The Settings pane that owns this cause.
    pub fn pane(self) -> SettingsPane {
        match self {
            Self::Remote => SettingsPane::Remote,
            Self::Schedule => SettingsPane::Schedules,
        }
    }
}

/// Whether the mode itself currently wants the machine awake.
fn mode_active(status: &AwakeStatus) -> bool {
    match status.mode {
        AwakeMode::On => true,
        AwakeMode::Agent => status.agents > 0,
        AwakeMode::Off => false,
    }
}

/// `"<Mode> · Active|Inactive"` — the chip's state and the card's header.
pub fn status_text(status: &AwakeStatus) -> String {
    let state = if status.asserted { "Active" } else { "Inactive" };
    format!("{} · {state}", label(status.mode))
}

pub fn tooltip_text(status: &AwakeStatus) -> String {
    format!("Keep computer awake, {}", status_text(status))
}

/// Every live cause the mode does not govern, in display order.
pub fn held_by(status: &AwakeStatus) -> Vec<HeldBy> {
    let mut causes = Vec::new();
    if status.remote {
        causes.push(HeldBy::Remote);
    }
    if status.scheduled {
        causes.push(HeldBy::Schedule);
    }
    causes
}

/// One footnote line for `cause`. "Also" when the mode is holding too, so the
/// line never reads as the only reason.
pub fn held_by_line(status: &AwakeStatus, cause: HeldBy) -> String {
    let prefix = if mode_active(status) { "Also held by" } else { "Held by" };
    format!("{prefix} {}", cause.clause())
}

/// The always-shown limits, one line each; On adds the battery cost.
pub fn caveat_lines(mode: AwakeMode) -> Vec<&'static str> {
    let mut lines = vec!["The display can still turn off.", "Closing the lid still sleeps."];
    if mode == AwakeMode::On {
        lines.push("Uses battery while unplugged.");
    }
    lines
}

fn row_h(typography: &Typography) -> f32 {
    line_h(typography.t_body_sm) + line_h(typography.t_sub_label) + ROW_PAD_Y * 2.0
}

/// The card's height for `status`, from the same tokens it lays out with: its
/// host is sized before anything renders. Counts only the lines this status
/// shows — held-by links and the mode's caveats — so there is no empty space
/// under them; a host re-fits when the count changes (see `status_popover`).
pub fn card_height(status: &AwakeStatus, density: Density, typography: &Typography) -> f32 {
    let gap = density.gap_inline * 1.5;
    let sub = line_h(typography.t_sub_label);
    let held = held_by(status).len();
    let held_block = if held == 0 { 0.0 } else { sub * held as f32 + density.gap_inline };
    density.pad_panel * 2.0
        + line_h(typography.t_body_sm)
        + gap
        + 1.0
        + gap
        + row_h(typography) * MODES.len() as f32
        + gap
        + held_block
        + sub * caveat_lines(status.mode).len() as f32
        // Border, top and bottom.
        + 2.0
}

pub fn render(
    status: &AwakeStatus,
    theme: Theme,
    density: Density,
    typography: &Typography,
    on_select: impl Fn(AwakeMode, &mut Window, &mut App) + 'static,
    on_open_pane: impl Fn(SettingsPane, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let on_select = Rc::new(on_select);
    let on_open_pane = Rc::new(on_open_pane);
    let one_line = |text: String, size: f32, colour| {
        div().w_full().min_w_0().truncate().text_size(px(size)).text_color(colour).child(text)
    };

    let header = div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .h(px(line_h(typography.t_body_sm)))
        .text_size(px(typography.t_body_sm))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_color(theme.fg_base)
                .child(Icon::default().path(ICON).size(px(13.0)).text_color(theme.fg_muted))
                .child("Keep computer awake"),
        )
        .child(
            div()
                .flex_none()
                .text_color(if status.asserted { theme.fg_base } else { theme.fg_muted })
                .child(status_text(status)),
        );

    let rows = MODES.into_iter().map(|(mode, description)| {
        let selected = status.mode == mode;
        let on_select = on_select.clone();
        let hover_bg = theme.hover_overlay;
        div()
            .id(("awake-card-mode", mode as usize))
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(6.0))
            .py(px(ROW_PAD_Y))
            .rounded(px(density.r_chip))
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            .on_click(move |_: &ClickEvent, window, cx| on_select(mode, window, cx))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(DOT))
                    .rounded_full()
                    .border_1()
                    .border_color(if selected { theme.fg_base } else { theme.fg_subtle })
                    .when(selected, |dot| {
                        dot.child(div().size(px(DOT / 2.0)).rounded_full().bg(theme.fg_base))
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(one_line(label(mode).to_string(), typography.t_body_sm, theme.fg_base))
                    .child(one_line(description.to_string(), typography.t_sub_label, theme.fg_muted)),
            )
    });

    // Each cause is its own line and its own link, so neither can be clipped
    // by the other's length.
    let held = held_by(status).into_iter().map(|cause| {
        let on_open_pane = on_open_pane.clone();
        let link = theme.focus_ring;
        div()
            .id(("awake-card-held-by", cause as usize))
            .w_full()
            .min_w_0()
            .truncate()
            .text_size(px(typography.t_sub_label))
            .text_color(link)
            .cursor_pointer()
            .hover(|s| s.underline())
            .on_click(move |_: &ClickEvent, window, cx| on_open_pane(cause.pane(), window, cx))
            .child(held_by_line(status, cause))
    });

    div()
        .id("awake-card")
        .flex()
        .flex_col()
        .size_full()
        .gap(px(density.gap_inline * 1.5))
        .p(px(density.pad_panel))
        .rounded(px(density.r_card))
        .bg(theme.bg_overlay)
        .border_1()
        .border_color(theme.border_active)
        // A click on the card itself must not reach the backdrop that closes it.
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(header)
        .child(div().w_full().h(px(1.0)).bg(theme.border_inactive))
        .child(div().flex().flex_col().children(rows))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(density.gap_inline))
                .when(!held_by(status).is_empty(), |d| d.child(div().flex().flex_col().children(held)))
                .child(div().flex().flex_col().children(
                    caveat_lines(status.mode)
                        .into_iter()
                        .map(|line| one_line(line.to_string(), typography.t_sub_label, theme.fg_subtle)),
                )),
        )
        .into_any_element()
}

/// The card drawn in the main window (off macOS), with its size: a row closes
/// nothing, a link closes the card and opens its Settings pane.
pub fn inline(
    root: gpui::WeakEntity<crate::workspace_root::WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> (AnyElement, f32, f32) {
    let status = crate::agent_awake::global().status();
    let pane_root = root.clone();
    let card = render(
        &status,
        theme,
        density,
        typography,
        move |mode, _window, cx| {
            let _ = root.update(cx, |this, cx| this.select_awake_mode(mode, cx));
        },
        move |pane, window, cx| {
            let _ = pane_root.update(cx, |this, cx| {
                this.status_popover_open = None;
                this.open_settings_pane(pane, window, cx);
                cx.notify();
            });
        },
    );
    (card, CARD_WIDTH, card_height(&status, density, typography))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(mode: AwakeMode) -> AwakeStatus {
        AwakeStatus { mode, ..AwakeStatus::default() }
    }

    #[test]
    fn status_text_names_mode_and_state() {
        for mode in [AwakeMode::On, AwakeMode::Agent, AwakeMode::Off] {
            let idle = status(mode);
            assert_eq!(status_text(&idle), format!("{} · Inactive", label(mode)));
            let held = AwakeStatus { asserted: true, ..idle };
            assert_eq!(status_text(&held), format!("{} · Active", label(mode)));
        }
        assert_eq!(
            tooltip_text(&AwakeStatus { asserted: true, ..status(AwakeMode::Agent) }),
            "Keep computer awake, Agent · Active"
        );
    }

    #[test]
    fn off_with_remote_is_held_by_remote() {
        let s = AwakeStatus { asserted: true, remote: true, ..status(AwakeMode::Off) };
        assert_eq!(held_by(&s), vec![HeldBy::Remote]);
        assert_eq!(held_by_line(&s, HeldBy::Remote), "Held by remote access");
        assert_eq!(HeldBy::Remote.pane(), SettingsPane::Remote);
    }

    #[test]
    fn off_with_a_schedule_is_held_by_the_schedule() {
        let s = AwakeStatus { asserted: true, scheduled: true, ..status(AwakeMode::Off) };
        assert_eq!(held_by(&s), vec![HeldBy::Schedule]);
        assert_eq!(held_by_line(&s, HeldBy::Schedule), "Held by an armed schedule");
        assert_eq!(HeldBy::Schedule.pane(), SettingsPane::Schedules);
    }

    #[test]
    fn a_running_agent_makes_it_also_held_by() {
        let s = AwakeStatus { asserted: true, agents: 2, remote: true, ..status(AwakeMode::Agent) };
        assert_eq!(held_by_line(&s, HeldBy::Remote), "Also held by remote access");
        let idle_agent = AwakeStatus { agents: 0, ..s };
        assert_eq!(held_by_line(&idle_agent, HeldBy::Remote), "Held by remote access");
        let on = AwakeStatus { scheduled: true, ..status(AwakeMode::On) };
        assert_eq!(held_by_line(&on, HeldBy::Schedule), "Also held by an armed schedule");
    }

    #[test]
    fn nothing_but_the_mode_means_no_held_by_line() {
        let s = AwakeStatus { asserted: true, agents: 1, ..status(AwakeMode::Agent) };
        assert!(held_by(&s).is_empty());
    }

    #[test]
    fn only_on_warns_about_battery() {
        assert!(caveat_lines(AwakeMode::On).contains(&"Uses battery while unplugged."));
        for mode in [AwakeMode::Agent, AwakeMode::Off] {
            assert_eq!(caveat_lines(mode).len(), 2);
        }
    }

    /// Sized to the lines the status shows: each held-by cause and the On
    /// battery caveat add exactly one line; no cause adds no footer gap.
    #[test]
    fn the_card_fits_the_lines_it_shows() {
        let (density, typography) = (Density::default(), Typography::default());
        let sub = line_h(typography.t_sub_label);
        let h = |s: AwakeStatus| card_height(&s, density, &typography);
        let off = status(AwakeMode::Off);
        let on = status(AwakeMode::On);
        assert_eq!(h(on) - h(off), sub, "the battery line");
        let remote = AwakeStatus { remote: true, ..off };
        assert_eq!(h(remote) - h(off), sub + density.gap_inline, "first cause + its gap");
        let both = AwakeStatus { scheduled: true, ..remote };
        assert_eq!(h(both) - h(remote), sub, "second cause");
    }
}
