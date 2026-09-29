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

fn make_group(cx: &mut TestAppContext) -> (WindowHandle<PaneGroup>, TempDir) {
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

#[gpui::test]
async fn a_lost_agent_tab_is_queued_to_resume(cx: &mut TestAppContext) {
    let (window, dir) = make_group(cx);
    let session = AgentSessionId::new(41);
    let (_status_tx, status_rx) =
        tokio::sync::watch::channel(AgentSnapshot::from_status(AgentStatus::Idle));
    let (backend, term) = spawn_local_pty_dormant(80, 24).expect("dormant backend");
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
    let view = cx.read(|app| {
        let group = window.read(app).expect("group");
        let PaneContent::Terminal(tree) = &group.active_tab().expect("agent tab").content else {
            panic!("an agent tab hosts a terminal");
        };
        tree.active_view().expect("its view").clone()
    });

    cx.update(|cx| {
        view.update(cx, |_, cx| cx.emit(TerminalViewEvent::DaemonLost { session_id: term }))
    });

    cx.read(|app| {
        let group = window.read(app).expect("group");
        assert_eq!(group.pending_lost_agents, [session], "queued for the workspace to resume");
    });
    view.read_with(cx, |v, _| {
        assert!(!v.is_recovering_from_loss(), "never respawned as a shell");
    });
}
