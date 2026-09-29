//! Kill all, the foreground half: which of the daemon's sessions the open
//! windows show, and closing those tabs.
//!
//! The confirm splits the daemon's list into sessions a tab shows (closing the
//! tab ends them) and the rest — background projects, terminals inside agent
//! chats, `oximux serve`'s. [`close_every_terminal_tab`] then closes the tabs,
//! before the daemon sweep (`RelayLifecycle::kill_all_sessions`) ends whatever
//! is left.

use std::collections::HashSet;

use gpui::{App, Context, Window};

use super::WorkspaceRoot;
use crate::platform::window_registry;

/// The daemon's sessions as the kill-all confirm counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionSplit {
    /// Shown by a terminal or agent CLI tab in an open window.
    pub visible: usize,
    /// Everything else the daemon lists.
    pub other: usize,
}

/// Split the daemon's `listed` sessions by whether a tab `shown` them. A tab
/// whose session is gone is not counted: the daemon's list is the truth.
pub fn split_counts(listed: &[String], shown: &HashSet<String>) -> SessionSplit {
    let visible = listed.iter().filter(|id| shown.contains(*id)).count();
    SessionSplit { visible, other: listed.len() - visible }
}

/// The daemon ids of every terminal a tab shows, in every window and every
/// project, the floating terminal included.
pub fn shown_pty_ids(cx: &App) -> HashSet<String> {
    let mut ids = HashSet::new();
    for (_, workspace) in window_registry::all_windows(cx) {
        workspace.read(cx).collect_shown_pty_ids(cx, &mut ids);
    }
    ids
}

/// Close every terminal and agent CLI tab in every window, and end every
/// chat's companion terminal. Returns how many tabs closed.
///
/// Updates each window in turn, so it must run outside any window's update —
/// from a spawned task, not from inside a view's handler.
pub fn close_every_terminal_tab(cx: &mut App) -> usize {
    let mut closed = 0;
    for (persist_id, workspace) in window_registry::all_windows(cx) {
        let Some(handle) = window_registry::window_handle(cx, &persist_id) else {
            continue;
        };
        let updated = handle.update(cx, |_, window, cx| {
            workspace.update(cx, |root, cx| root.close_terminal_tabs(window, cx))
        });
        match updated {
            Ok(n) => closed += n,
            Err(err) => tracing::warn!(%persist_id, ?err, "kill all: window not updatable"),
        }
    }
    closed
}

impl WorkspaceRoot {
    fn collect_shown_pty_ids(&self, cx: &App, out: &mut HashSet<String>) {
        for panes in self.all_project_panes() {
            for group in panes.read(cx).group_entities() {
                out.extend(group.read(cx).relay_pty_ids(cx));
            }
        }
        if let Some(floating) = &self.floating_terminal {
            out.extend(floating.read(cx).views().filter_map(|v| v.read(cx).relay_pty_id()));
        }
    }

    fn close_terminal_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let mut closed = 0;
        for panes in self.all_project_panes() {
            for group in panes.read(cx).group_entities() {
                closed += group.update(cx, |g, cx| {
                    g.end_companion_terminals(window, cx);
                    g.close_terminal_tabs(window, cx)
                });
            }
        }
        // Closing its last tab emits `Close`, which drops the card and
        // persists the empty set.
        if let Some(floating) = self.floating_terminal.clone() {
            closed += floating.update(cx, |f, cx| {
                let n = f.tab_count();
                for idx in (0..n).rev() {
                    f.close_tab(idx, window, cx);
                }
                n
            });
        }
        // The closed tabs may have held focus; without a focused element no
        // chord reaches the window. Not while a confirm is up (kill all's
        // own, busy): it keeps focus, and hands it back when it finishes.
        if !self.confirm_pending(cx) {
            crate::shell::workspace_ops::refocus_active_pane(self, window, cx);
        }
        closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_split_adds_up_to_the_daemons_list() {
        let listed = ids(&["a", "b", "c", "d"]);
        let shown: HashSet<String> = ids(&["a", "c", "gone"]).into_iter().collect();

        let split = split_counts(&listed, &shown);

        assert_eq!(split, SessionSplit { visible: 2, other: 2 });
        assert_eq!(split.visible + split.other, listed.len());
    }

    #[test]
    fn nothing_listed_is_nothing_to_split() {
        let shown: HashSet<String> = ids(&["a"]).into_iter().collect();
        assert_eq!(split_counts(&[], &shown), SessionSplit::default());
    }
}
