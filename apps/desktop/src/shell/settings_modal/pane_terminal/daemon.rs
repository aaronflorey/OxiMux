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
use crate::relay_lifecycle::ui::{self, Tone};

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

/// What the section shows, owned so the global is not borrowed while the
/// controls are built.
struct View {
    status: (String, Tone),
    version: (String, bool),
    busy: bool,
    in_process: bool,
    sessions: Option<usize>,
}

fn view(state: Option<&RelayDaemonState>) -> View {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let Some(state) = state else {
        return View {
            status: ("unknown".into(), Tone::Warn),
            version: ("—".into(), false),
            busy: false,
            in_process: true,
            sessions: None,
        };
    };
    let recorded = state.details.as_ref().and_then(|d| d.record.as_ref()).map(|r| r.version.clone());
    let version = match (&state.stale, recorded) {
        (Some(stale), _) => (
            format!("v{} — app v{}, restart to update", stale.daemon_version, stale.app_version),
            true,
        ),
        (None, Some(version)) => (format!("v{version}"), false),
        (None, None) => ("—".into(), false),
    };
    View {
        status: ui::status_line(state, now),
        version,
        busy: state.busy.is_some(),
        in_process: state.status == DaemonStatus::InProcess,
        sessions: state.details.as_ref().and_then(|d| d.sessions),
    }
}

/// The section's rows, also what settings search matches ("restart daemon",
/// "kill sessions").
pub(super) fn entries(
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let v = view(cx.try_global::<RelayDaemonState>());
    let text = |s: String, colour| {
        div().text_size(px(typography.t_body_sm)).text_color(colour).child(s).into_any_element()
    };
    let tone = match v.status.1 {
        Tone::Ok => theme.status_ok,
        Tone::Busy => theme.status_info,
        Tone::Warn => theme.status_warn,
        Tone::Error => theme.status_error,
    };
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

    let can_restart = !v.busy && !v.in_process;
    let restart = action_chip(
        "daemon-restart",
        "Restart",
        ChipTone::Neutral,
        can_restart,
        theme,
        density,
        typography,
        |_, _, cx| ui::request_restart(cx),
        cx,
    );
    let can_kill = can_restart && v.sessions != Some(0);
    let kill = action_chip(
        "daemon-kill-all",
        "Kill all",
        ChipTone::Danger,
        can_kill,
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
            if v.in_process {
                "Relaunch OxiMux to start the daemon."
            } else {
                "Restarts the terminal daemon. Shells keep their scrollback; agent CLIs resume."
            },
            restart,
        ),
        entry("Kill sessions", "Ends every terminal session. The daemon keeps running.", kill),
    ]
}
