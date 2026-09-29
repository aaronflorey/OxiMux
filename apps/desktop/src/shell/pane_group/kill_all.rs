//! Kill all's part in one tab strip: close every terminal tab, end every
//! chat's companion terminal, and name the daemon sessions the strip shows.
//! Split out of `tabs.rs`, which sits at the file-size cap.

use super::*;
use crate::shell::agent_chat::ChatViewMode;

impl PaneGroup {
    /// Close every tab whose content is a terminal — shells and cockpit
    /// agent CLIs alike, pinned ones too, since kill all is explicit. Each
    /// goes through [`Self::close_tab`], so an agent's session is cancelled
    /// and persistence updates as on a manual close. Returns how many closed.
    pub fn close_terminal_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let mut closed = 0;
        for idx in (0..self.tabs.len()).rev() {
            if matches!(self.tabs[idx].content, PaneContent::Terminal(_)) {
                self.close_tab(idx, window, cx);
                closed += 1;
            }
        }
        closed
    }

    /// End every chat's companion terminal, handing the session back to the
    /// chat the way switching back from terminal view does: reap the CLI,
    /// then fold, drop the terminal and reconnect. The chat tabs stay open.
    /// Returns how many companions were ended.
    ///
    /// On the relay the companion is closed first, and waited for: a cancel
    /// returns before the CLI is gone, and a single-writer backend (Codex)
    /// refuses the chat's resume while the CLI still holds the thread.
    ///
    /// A companion still being spawned has no session yet and is not ended.
    pub fn end_companion_terminals(&mut self, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let companions: Vec<_> = self
            .tabs
            .iter()
            .filter_map(|tab| match &tab.content {
                PaneContent::AgentChat(view) => {
                    let chat = view.read(cx);
                    let session = chat.companion_session_id()?;
                    Some((view.clone(), session, chat.companion_relay_pty_id(cx)))
                }
                _ => None,
            })
            .collect();
        for (view, session, relay_pty) in &companions {
            let (view, session, relay_pty) = (view.clone(), *session, relay_pty.clone());
            let runtime = self.cli_runtime.clone();
            cx.spawn_in(window, async move |_group, cx| {
                if let (Some(pty), Some(lifecycle)) = (relay_pty, crate::relay_lifecycle::lifecycle())
                    && !lifecycle.close_session(pty).await
                {
                    tracing::warn!("kill all: companion terminal did not confirm its end");
                }
                // Now only bookkeeping: its own close finds the session gone.
                if let Err(err) = runtime.cancel(session).await {
                    tracing::warn!(?err, "kill all: companion terminal cancel failed");
                }
                let _ = view.update_in(cx, |v, window, cx| {
                    // Leaving terminal view focuses the composer; keep focus
                    // where it was unless it sat in this chat's terminal.
                    let had_focus = v.active_focus_handle(cx).contains_focused(window, cx);
                    let prior = window.focused(cx);
                    v.set_view_mode(ChatViewMode::Chat, window, cx);
                    v.drop_companion_terminal(cx);
                    v.reconnect_after_handoff(cx);
                    if !had_focus && let Some(prior) = prior {
                        prior.focus(window, cx);
                    }
                });
            })
            .detach();
        }
        companions.len()
    }

    /// The daemon ids of every terminal in this strip's tabs, split panes
    /// included. Empty for in-process terminals.
    pub fn relay_pty_ids(&self, cx: &App) -> Vec<String> {
        self.tabs
            .iter()
            .filter_map(|tab| match &tab.content {
                PaneContent::Terminal(tree) => Some(tree),
                _ => None,
            })
            .flat_map(|tree| tree.iter_all_views().map(|(_, _, view)| view.read(cx).relay_pty_id()))
            .flatten()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use crate::shell::pane_content::PaneContent;
    use crate::shell::pane_group::daemon_loss_tests::make_group;

    // Terminal tabs close, a pinned one included, and anything else stays.
    // A cockpit agent CLI tab is a terminal tab like these; closing one here
    // would cancel its agent on a tokio thread, which this scheduler forbids.
    #[gpui::test]
    async fn only_terminal_tabs_close(cx: &mut TestAppContext) {
        let (window, dir) = make_group(cx);
        cx.update(gpui_component::init);
        window
            .update(cx, |g, win, cx| {
                g.open_terminal_tab(win, cx).expect("a shell");
                g.toggle_pin(0, cx);
                g.open_terminal_tab(win, cx).expect("another shell");
            })
            .expect("window alive");
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "notes\n").expect("write");
        window
            .update(cx, |g, win, cx| g.open_preview_editor_tab(file.clone(), win, cx))
            .expect("window alive");
        cx.run_until_parked();

        let closed = window
            .update(cx, |g, win, cx| g.close_terminal_tabs(win, cx))
            .expect("window alive");
        cx.run_until_parked();

        assert_eq!(closed, 2);
        cx.read(|app| {
            let group = window.read(app).expect("group");
            assert_eq!(group.tab_count(), 1);
            assert!(matches!(group.active_tab().expect("a tab").content, PaneContent::Editor(_)));
        });
    }
}
