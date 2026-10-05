//! The rail's remote-host section: paired hosts, their live connection
//! state, and each connected host's projects. A project click mounts the
//! remote project in the same panes area a local project uses — the remote
//! workspace is no longer a separate full-window view. Rendered at the
//! bottom of the workspace list's scroll column so local and remote
//! entries scroll together.

use gpui::{
    App, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, div,
    prelude::FluentBuilder, px, svg,
};
use gpui::AnyElement;
use oximux_remote_proto::{ProjectSummaryWire, SessionSummary};
use oximux_remote_session::ConnState;
use oximux_settings::{Density, Theme, Typography};

use crate::workspace_root::WorkspaceRoot;

/// One rail row's snapshot of a remote host — a saved hosts-book entry or a
/// live connection. `PartialEq` keeps the rail's render dirty-check cheap:
/// only a visible change (state, projects, active mark) repaints.
#[derive(Clone, PartialEq)]
pub(crate) struct RemoteRailHost {
    pub endpoint_id: String,
    pub name: String,
    /// Live conn state, or `Disconnected` for saved hosts never dialed.
    pub state: ConnState,
    /// Last failure string, for the row's subtitle.
    pub error: Option<String>,
    pub read_only: bool,
    /// `true` when a live [`crate::shell::remote_host::RemoteHost`] entity
    /// exists for this endpoint (dial attempted this session).
    pub live: bool,
    /// Projects reported by `ListProjects` on the last connected session.
    pub projects: Vec<ProjectSummaryWire>,
    /// Live agent sessions reported by `ListSessions` on the last connected
    /// session — clicking one opens a chat tab bound to it.
    pub sessions: Vec<SessionSummary>,
    /// Path of the active remote project on this host — drives the active
    /// row highlight, the remote mirror of `active_project_id`.
    pub active_path: Option<String>,
}

/// Render the whole remote section appended under the workspace list: a
/// group header, one block per host, and a "Connect a host…" affordance
/// that opens the pairing modal. `weak_root` reaches `WorkspaceRoot` for
/// connect/disconnect/pair/activate dispatches — the same upcall pattern
/// local project rows use for their menus.
pub(crate) fn render_remote_section(
    hosts: &[RemoteRailHost],
    book_loaded: bool,
    book_error: Option<&str>,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    let mut col = div()
        .flex()
        .flex_col()
        .w_full()
        .pt(px(density.pad_panel))
        .child(
            div()
                .px(px(density.pad_panel))
                .pb(px(density.gap_inline))
                .text_size(px(typography.t_sub_label))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_subtle)
                .child("REMOTE"),
        );
    if let Some(err) = book_error {
        col = col.child(
            div()
                .px(px(density.pad_panel))
                .pb(px(density.gap_inline))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.status_warn)
                .child(format!("Could not read the saved-hosts file: {err}")),
        );
    } else if !book_loaded {
        col = col.child(
            div()
                .px(px(density.pad_panel))
                .pb(px(density.gap_inline))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child("Loading saved hosts…"),
        );
    }
    for host in hosts {
        col = col.child(render_host_block(host, weak_root.clone(), theme, density, typography));
    }
    // Pairing entry point — always present so the feature is discoverable
    // even with zero saved hosts.
    col.child(add_host_row(weak_root, theme, density, typography))
        .into_any_element()
}

fn render_host_block(
    host: &RemoteRailHost,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    let group: SharedString = format!("remote-host-{}", host.endpoint_id).into();
    let connected = matches!(host.state, ConnState::Connected);
    let busy = matches!(host.state, ConnState::Connecting | ConnState::WaitingToRetry { .. });
    let (status_label, status_color) = match &host.state {
        ConnState::Connected => (
            if host.read_only { "read-only" } else { "connected" }.to_string(),
            theme.status_ok,
        ),
        ConnState::Connecting => ("connecting…".to_string(), theme.status_info),
        ConnState::WaitingToRetry { attempt, .. } => {
            (format!("reconnecting ({attempt})"), theme.status_warn)
        }
        ConnState::Unreachable { .. } => ("unreachable".to_string(), theme.status_error),
        ConnState::Disconnected => ("saved".to_string(), theme.fg_subtle),
    };

    let endpoint = host.endpoint_id.clone();
    let action_label: &'static str = if connected || busy {
        "Disconnect"
    } else {
        "Connect"
    };
    let weak_for_action = weak_root.clone();
    let ep_for_action = endpoint.clone();
    let action_btn = div()
        .id(SharedString::from(format!("remote-host-action-{endpoint}")))
        .flex()
        .items_center()
        .h(px(density.h_row))
        .px(px(density.gap_inline))
        .rounded(px(density.r_xs))
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_muted)
        .invisible()
        .group_hover(group.clone(), |s| s.visible())
        .hover(|s| s.bg(theme.bg_overlay).text_color(theme.fg_base))
        .child(action_label)
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _window, cx| {
            cx.stop_propagation();
            let ep = ep_for_action.clone();
            let _ = weak_for_action.update(cx, |root, cx| {
                root.remote_hosts.update(cx, |hosts, cx| {
                    if connected || busy {
                        hosts.disconnect(&ep, cx);
                    } else {
                        hosts.connect_saved(&ep, cx);
                    }
                });
            });
        });

    // Forget affordance — removes the book row AND drops the live entity,
    // so it only surfaces while disconnected (no "pull the rug out from
    // under a connected project" ambiguity).
    let weak_for_remove = weak_root.clone();
    let ep_for_remove = endpoint.clone();
    let remove_btn = div()
        .id(SharedString::from(format!("remote-host-remove-{endpoint}")))
        .flex()
        .items_center()
        .justify_center()
        .h(px(density.h_row))
        .px(px(density.gap_inline))
        .rounded(px(density.r_xs))
        .text_size(px(typography.t_sub_label))
        .text_color(theme.fg_muted)
        .invisible()
        .group_hover(group.clone(), |s| s.visible())
        .hover(|s| s.bg(theme.bg_overlay).text_color(theme.status_error))
        .child("✕")
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new("Forget this host").build(window, cx)
        })
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _window, cx| {
            cx.stop_propagation();
            let ep = ep_for_remove.clone();
            let _ = weak_for_remove.update(cx, |root, cx| {
                root.remote_hosts.update(cx, |hosts, cx| hosts.remove(&ep, cx));
            });
        });

    let mut header = div()
        .id(SharedString::from(format!("remote-host-row-{endpoint}")))
        .group(group)
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .child(
            svg()
                .path("icons/globe.svg")
                .size(px(12.))
                .text_color(theme.fg_muted),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(px(typography.t_body_sm))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_muted)
                .overflow_hidden()
                .whitespace_nowrap()
                .child(host.name.clone()),
        )
        .child(
            div()
                .text_size(px(typography.t_sub_label))
                .text_color(status_color)
                .child(status_label),
        )
        .child(action_btn)
        .when(!connected && !busy, |d| d.child(remove_btn));
    if let Some(cause) = host
        .error
        .as_ref()
        .filter(|_| matches!(host.state, ConnState::Unreachable { .. }))
    {
        header = header.tooltip({
            let cause = cause.clone();
            move |window, cx| {
                gpui_component::tooltip::Tooltip::new(cause.clone()).build(window, cx)
            }
        });
    }

    let mut block = div().flex().flex_col().w_full().child(header);
    if connected {
        for project in &host.projects {
            block = block.child(render_project_row(
                host,
                project,
                weak_root.clone(),
                theme,
                density,
                typography,
            ));
        }
        for session in &host.sessions {
            block = block.child(render_session_row(
                host,
                session,
                weak_root.clone(),
                theme,
                density,
                typography,
            ));
        }
    }
    block.into_any_element()
}

/// Upcall note for future tests: every mutation goes through `WorkspaceRoot`
/// (via `weak_root.update`) so the rail snapshot stays value-typed and
/// `PartialEq`-cheap — no `Entity` fields.
#[allow(dead_code)]
fn _imports(_: &App, _: &Window) {
}

fn render_project_row(
    host: &RemoteRailHost,
    project: &ProjectSummaryWire,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    let is_active = host.active_path.as_deref() == Some(project.path.as_str());
    let endpoint = host.endpoint_id.clone();
    let project = project.clone();
    let label = if project.name.is_empty() {
        project.path.clone()
    } else {
        project.name.clone()
    };
    div()
        .id(SharedString::from(format!(
            "remote-project-{}-{}",
            endpoint, project.path
        )))
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row))
        // Indented under the host header, same inset workspace rows use.
        .pl(px(density.pad_panel + 16.))
        .pr(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .cursor_pointer()
        .when(is_active, |d| d.bg(theme.selection))
        .hover(|s| s.bg(theme.bg_overlay))
        .child(
            svg()
                .path("icons/folder.svg")
                .size(px(12.))
                .text_color(if is_active { theme.fg_base } else { theme.fg_muted }),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(px(typography.t_body_sm))
                .text_color(if is_active { theme.fg_base } else { theme.fg_muted })
                .overflow_hidden()
                .whitespace_nowrap()
                .child(label),
        )
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            let ep = endpoint.clone();
            let project = project.clone();
            let _ = weak_root.update(cx, |root, cx| {
                let host = root.remote_hosts.read(cx).host(&ep);
                if let Some(host) = host {
                    root.set_active_remote(host, project, window, cx);
                }
            });
        })
        .into_any_element()
}

/// A host session's rail row — indented under the project rows (sessions
/// carry no cwd in the wire summary, so they belong to the host block, not
/// one project). Clicking mounts the host's active project if needed and
/// opens a chat tab bound to the live session.
fn render_session_row(
    host: &RemoteRailHost,
    session: &SessionSummary,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    let endpoint = host.endpoint_id.clone();
    let session_id = session.session_id.clone();
    let label = if session.title.is_empty() {
        session.session_id.chars().take(8).collect::<String>()
    } else {
        session.title.clone()
    };
    div()
        .id(SharedString::from(format!(
            "remote-session-{}-{}",
            endpoint, session.session_id
        )))
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row))
        .pl(px(density.pad_panel + 16.))
        .pr(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_overlay))
        .child(
            svg()
                .path("icons/sparkles.svg")
                .size(px(12.))
                .text_color(theme.fg_muted),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_muted)
                .overflow_hidden()
                .whitespace_nowrap()
                .child(label),
        )
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            let ep = endpoint.clone();
            let session_id = session_id.clone();
            let _ = weak_root.update(cx, |root, cx| {
                let host = root.remote_hosts.read(cx).host(&ep);
                if let Some(host) = host {
                    root.open_remote_session(host, &session_id, window, cx);
                }
            });
        })
        .into_any_element()
}

fn add_host_row(
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    div()
        .id("remote-add-host")
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_row))
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .cursor_pointer()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .hover(|s| s.text_color(theme.fg_base))
        .child(
            svg()
                .path("icons/plus.svg")
                .size(px(12.))
                .text_color(theme.fg_subtle),
        )
        .child("Connect a host…")
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            let _ = weak_root.update(cx, |root, cx| root.open_remote_pairing(window, cx));
        })
        .into_any_element()
}


