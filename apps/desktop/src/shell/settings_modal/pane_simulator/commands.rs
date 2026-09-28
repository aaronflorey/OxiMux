//! "Common commands": the `oximux sim` lines an agent (or a person) runs most,
//! each with a copy button.

use gpui::{AnyElement, ClipboardItem, Context, IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

use super::super::SettingsModal;
use super::super::controls::icon_button;
use super::super::layout::{SettingEntry, entry_stacked};

/// A command and what it does.
const COMMANDS: [(&str, &str); 5] = [
    ("oximux sim devices", "every simulator, emulator and phone"),
    ("oximux sim attach \"iPhone 17\"", "attach one to this worktree, booting it"),
    ("oximux sim screenshot", "a PNG where one pixel is one point"),
    ("oximux sim tap --label Settings", "tap an element by its accessibility label"),
    ("oximux sim ax", "the accessibility tree, with frames"),
];

pub(super) fn entry(theme: Theme, density: Density, typography: &Typography, cx: &mut Context<SettingsModal>) -> SettingEntry {
    let lines = COMMANDS.iter().enumerate().map(|(idx, &(command, what))| line(idx, command, what, theme, density, typography, cx));
    entry_stacked(
        "Common commands",
        "Run in a terminal inside a worktree open in OxiMux; they act on that worktree's device.",
        div().flex().flex_col().gap(px(6.0)).w_full().children(lines),
    )
}

fn line(
    idx: usize,
    command: &'static str,
    what: &'static str,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .w_full()
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(density.r_chip))
        .border_1()
        .border_color(theme.border_inactive)
        .bg(theme.bg_panel_alt)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().font_family(typography.family_mono.clone()).text_size(px(typography.t_body_sm)).text_color(theme.fg_base).child(command))
                .child(div().text_size(px(typography.t_sub_label)).text_color(theme.fg_subtle).child(what)),
        )
        .child(icon_button(("sim-copy-command", idx), "icons/copy.svg", "Copy", false, theme, density, move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(command.to_owned()));
        }, cx))
        .into_any_element()
}
