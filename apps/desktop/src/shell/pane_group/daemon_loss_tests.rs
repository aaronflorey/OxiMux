//! A tab whose terminal died with its daemon is dispatched by kind: a cockpit
//! agent tab is queued to resume its conversation (which needs the workspace
//! and a window), never respawned as a bare shell.

use std::sync::{Arc, atomic::AtomicBool};

use gpui::{TestAppContext, WindowHandle};
use oximux_agents::CliRuntime;
use oximux_core::{AgentAdapter, AgentSessionId, AgentSnapshot, AgentStatus};
use oximux_settings::{Density, Theme, Typography};
use tempfile::TempDir;

use crate::notifier::null::NullNotifier;
use crate::shell::pane_content::PaneContent;
use crate::shell::pane_group::PaneGroup;
use crate::shell::terminal_view::{TerminalViewEvent, spawn_local_pty_dormant};

pub(super) fn make_group(cx: &mut TestAppContext) -> (WindowHandle<PaneGroup>, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let cwd = dir.path().to_path_buf();
    let window = cx.add_window(|_win, cx| {
        PaneGroup::new(
            cwd,
            Theme::default(),
            Density::default(),
            Typography::default(),
            Arc::new(CliRuntime::new()),
            Arc::new(NullNotifier),
            Arc::new(AtomicBool::new(true)),
            cx,
        )
    });
    (window, dir)
}

/// An agent tab whose own terminal is a session of this group's runtime, as a
/// live agent's is. (Adopted rather than started: no CLI is launched.)
fn agent_tab(
    window: &WindowHandle<PaneGroup>,
    dir: &TempDir,
    cx: &mut TestAppContext,
) -> (AgentSessionId, oximux_pty::TerminalSessionId) {
    let (backend, term) = spawn_local_pty_dormant(80, 24).expect("dormant backend");
    let runtime = cx.read(|app| Arc::clone(&window.read(app).expect("group").cli_runtime));
    runtime.register_adapter(AgentAdapter::ClaudeCode, Arc::new(oximux_agents::ClaudeCodeAdapter));
    let session = runtime
        .adopt_session(AgentAdapter::ClaudeCode, backend.clone(), term, None)
        .expect("adopt");
    let (_status_tx, status_rx) =
        tokio::sync::watch::channel(AgentSnapshot::from_status(AgentStatus::Idle));
    let worktree = dir.path().to_path_buf();
    window
        .update(cx, |g, win, cx| {
            g.push_agent_tab(
                AgentAdapter::ClaudeCode,
                "claude-code",
                worktree,
                None,
                None,
                session,
                status_rx,
                backend,
                term,
                None,
                None,
                win,
                cx,
            )
        })
        .expect("window alive");
    (session, term)
}

fn active_view(
    window: &WindowHandle<PaneGroup>,
    cx: &mut TestAppContext,
) -> gpui::Entity<crate::shell::terminal_view::TerminalView> {
    cx.read(|app| {
        let group = window.read(app).expect("group");
        let PaneContent::Terminal(tree) = &group.active_tab().expect("a tab").content else {
            panic!("a terminal tab");
        };
        tree.active_view().expect("its view").clone()
    })
}

#[gpui::test]
async fn a_lost_agent_tab_is_queued_to_resume(cx: &mut TestAppContext) {
    let tokio = tokio::runtime::Runtime::new().expect("tokio");
    let _entered = tokio.enter();
    let (window, dir) = make_group(cx);
    let (session, term) = agent_tab(&window, &dir, cx);
    let view = active_view(&window, cx);

    cx.update(|cx| {
        view.update(cx, |_, cx| cx.emit(TerminalViewEvent::DaemonLost { session_id: term }))
    });

    cx.read(|app| {
        let group = window.read(app).expect("group");
        assert_eq!(group.pending_lost_agents, [(session, term)], "queued for the workspace");
    });
    view.read_with(cx, |v, _| {
        assert!(!v.is_recovering_from_loss(), "never respawned as a shell");
    });
}

// A shell split into an agent tab is still a shell: it respawns, and the
// agent is not resumed on its account.
#[gpui::test]
async fn a_shell_split_into_an_agent_tab_respawns_as_a_shell(cx: &mut TestAppContext) {
    let tokio = tokio::runtime::Runtime::new().expect("tokio");
    let _entered = tokio.enter();
    let (window, dir) = make_group(cx);
    let _ = agent_tab(&window, &dir, cx);
    window
        .update(cx, |g, win, cx| {
            g.on_split_sub_pane_right(&crate::actions::SplitSubPaneRight, win, cx)
        })
        .expect("window alive");
    let shell = active_view(&window, cx);
    let shell_term = shell.read_with(cx, |v, _| v.session_id());

    cx.update(|cx| {
        shell.update(cx, |_, cx| cx.emit(TerminalViewEvent::DaemonLost { session_id: shell_term }))
    });

    cx.read(|app| {
        assert!(window.read(app).expect("group").pending_lost_agents.is_empty());
    });
}
