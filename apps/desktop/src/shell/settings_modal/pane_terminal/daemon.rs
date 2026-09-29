//! Settings → Terminal → "Terminal daemon": its status, version and log, and
//! the Restart and Kill all verbs. The verbs call `relay_lifecycle::ui`
//! directly rather than dispatching their actions — a click here may leave
//! nothing focused, and then an action never reaches the root.

use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{AnyElement, ClipboardItem, Context, IntoElement, ParentElement, Styled, div, px};
use oximux_settings::{Density, Theme, Typography};

use super::super::SettingsModal;
use super::super::controls::{ChipTone, action_chip, value_chip};
use super::super::layout::{SettingEntry, entries_card, entry, section_title};
use crate::relay_lifecycle::state::{DaemonStatus, RelayDaemonState, refresh_details_if_stale};
use crate::relay_lifecycle::ui;

/// The section under the Terminal settings. Keeps its details fresh while it
/// is on screen.
pub(super) fn render(
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> AnyElement {
    refresh_details_if_stale(cx);
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .pt(px(20.0))
        .child(section_title(
            "Terminal daemon",
            "Runs every terminal and agent CLI tab, so they survive closing OxiMux.",
            theme,
            typography,
        ))
        .child(entries_card(theme, density, typography, entries(theme, density, typography, cx)))
        .into_any_element()
}

/// The section's rows, also what settings search matches ("restart daemon",
/// "kill sessions").
pub(super) fn entries(
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    // Owned, so the global is not borrowed while the controls are built.
    let v = ui::daemon_view(cx.try_global::<RelayDaemonState>(), now);
    let in_process = matches!(
        cx.try_global::<RelayDaemonState>().map(|s| &s.status),
        None | Some(DaemonStatus::InProcess)
    );
    let text = |s: String, colour| {
        div().text_size(px(typography.t_body_sm)).text_color(colour).child(s).into_any_element()
    };
    let tone = crate::shell::chrome::daemon_card::tone_color(v.status.1, &theme);
    let status = text(v.status.0, tone);
    let version = text(v.version.0, if v.version.1 { theme.status_warn } else { theme.fg_muted });

    let log = ui::log_path();
    let log_shown = log.as_ref().map_or_else(|| "No log — the daemon is not running.".to_string(), |p| {
        let home = dirs::home_dir();
        match home.as_deref().and_then(|h| p.strip_prefix(h).ok()) {
            Some(rest) => format!("~/{}", rest.display()),
            None => p.display().to_string(),
        }
    });
    let log_controls = match log {
        Some(path) => {
            let copied = path.display().to_string();
            div()
                .flex()
                .flex_row()
                .gap(px(density.gap_inline))
                .child(value_chip("daemon-log-copy", "Copy path", theme, density, typography, move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                }, cx))
                .child(value_chip("daemon-log-reveal", "Reveal", theme, density, typography, |_, _, cx| {
                    ui::open_log(cx);
                }, cx))
                .into_any_element()
        }
        None => div().into_any_element(),
    };

    let restart = action_chip(
        "daemon-restart",
        "Restart",
        ChipTone::Neutral,
        v.can_restart,
        theme,
        density,
        typography,
        |_, _, cx| ui::request_restart(cx),
        cx,
    );
    let kill = action_chip(
        "daemon-kill-all",
        "Kill all",
        ChipTone::Danger,
        v.can_kill,
        theme,
        density,
        typography,
        |_, _, cx| ui::request_kill_all(cx),
        cx,
    );

    vec![
        entry("Status", "Whether the terminal daemon is up, and what it holds.", status),
        entry("Version", "The daemon's version. It updates when the daemon restarts.", version),
        entry("Log file", log_shown, log_controls),
        entry(
            "Restart daemon",
            if in_process {
                "Relaunch OxiMux to start the daemon."
            } else {
                "Restarts the terminal daemon. Shells keep their scrollback; agent CLIs resume."
            },
            restart,
        ),
        entry("Kill sessions", "Ends every terminal session. The daemon keeps running.", kill),
    ]
}
