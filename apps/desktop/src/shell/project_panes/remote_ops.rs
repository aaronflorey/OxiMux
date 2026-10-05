//! Remote-scoped ops on `ProjectPanes` — the cross-group half of the
//! remote tab surface.
//!
//! The group's own remote openers live in `pane_group/remote_tabs.rs`;
//! these dispatch across every group a project surface owns: dedupe an
//! already-open session chat or an already-attached host PTY wherever it
//! landed, open a terminal tab on the active group, and rebind the whole
//! panes tree when the host entity it scopes to was replaced.

use super::*;

impl ProjectPanes {
    /// Focus an already-open remote session chat wherever it lives across
    /// this panes' groups — host chat bindings are one-per-session per
    /// connection, so the viewer the first open created is the only view
    /// that may own it; a second `register_chat` elsewhere would steal
    /// the binding and orphan the survivor's frames.
    pub fn focus_remote_session_tab(
        &mut self,
        session_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let hit = self.groups.iter().find_map(|(id, group)| {
            group.update(cx, |g, cx| {
                g.remote_session_tab_index(session_id, cx).map(|idx| {
                    g.set_active(idx, window, cx);
                    *id
                })
            })
        });
        let Some(id) = hit else {
            return false;
        };
        self.set_active_group(id, window, cx);
        true
    }

    /// Focus an already-attached remote terminal wherever it lives across
    /// this panes' groups — one live attachment per PTY per connection,
    /// so the owning view wins focus rather than a second mount racing
    /// `attach_terminal`'s refusal.
    pub fn focus_remote_terminal_tab(
        &mut self,
        pty_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let hit = self.groups.iter().find_map(|(id, group)| {
            group.update(cx, |g, cx| {
                g.remote_terminal_tab_index(pty_id, cx).map(|idx| {
                    g.set_active(idx, window, cx);
                    *id
                })
            })
        });
        let Some(id) = hit else {
            return false;
        };
        self.set_active_group(id, window, cx);
        true
    }

    /// Open a terminal tab attached to an EXISTING host PTY in the
    /// active group — remote-bound panes only. The rail's terminal rows
    /// drive this: reopening re-attaches the original PTY rather than
    /// spawning a replacement.
    pub fn open_remote_terminal_in_active_group(
        &mut self,
        pty_id: &str,
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.focus_remote_terminal_tab(pty_id, window, cx) {
            return;
        }
        let target_id = self
            .groups
            .contains_key(&self.manager.active_group_id())
            .then(|| self.manager.active_group_id())
            .or_else(|| self.manager.in_order_groups().first().copied());
        let Some(target_id) = target_id else {
            return;
        };
        self.set_active_group(target_id, window, cx);
        if let Some(target) = self.groups.get(&target_id).cloned() {
            target.update(cx, |g, cx| {
                g.open_remote_terminal_attach(pty_id, label, window, cx);
            });
        }
    }

    /// Point every group's remote scope at a REPLACEMENT host entity and
    /// re-register the remote-bound chat tabs on it — runs the per-group
    /// no-op when the scope still resolves to this entity, so callers may
    /// invoke it on every cached-surface reuse.
    pub(crate) fn rebind_remote_host(
        &mut self,
        host: &Entity<crate::shell::remote_host::RemoteHost>,
        cx: &mut Context<Self>,
    ) {
        if let Some(scope) = self.remote.as_mut()
            && !scope.host_is(host)
        {
            scope.rebind(host);
        }
        for group in self.groups.values() {
            group.update(cx, |g, cx| g.rebind_remote_host(host, cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use gpui::{AppContext, TestAppContext, WindowHandle};
    use oximux_agents::CliRuntime;
    use oximux_remote_session::hosts_store::HostEntry;
    use oximux_settings::{Density, Theme, Typography};
    use tempfile::TempDir;

    use crate::notifier::null::NullNotifier;
    use crate::shell::remote_host::RemoteHost;
    use crate::shell::remote_scope::RemoteScope;

    /// Remote-scoped `ProjectPanes` inside a test window; the host entity
    /// is never dialed so ops degrade deterministically. The host must
    /// stay alive for the whole test — the scope's weak handle upgrades
    /// only while it does.
    fn make_remote_panes(
        cx: &mut TestAppContext,
        endpoint: &str,
    ) -> (WindowHandle<ProjectPanes>, Entity<RemoteHost>, TempDir) {
        cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let dir = TempDir::new().expect("tempdir");
        let host = cx.update(|cx| {
            cx.new(|cx| {
                RemoteHost::new(
                    HostEntry {
                        name: "test-host".into(),
                        endpoint_id: endpoint.into(),
                        enrollment: None,
                        read_only: false,
                        protocol_version: None,
                    },
                    cx,
                )
            })
        });
        let window = cx.add_window(|window, cx| {
            let scope = RemoteScope::new(&host, endpoint.into());
            ProjectPanes::new(
                dir.path().to_path_buf(),
                Some(scope),
                Theme::default(),
                Density::default(),
                Typography::default(),
                Arc::new(CliRuntime::new()),
                Arc::new(NullNotifier),
                window,
                cx,
            )
        });
        (window, host, dir)
    }

    /// A chat opened in one group is found by the panes-level dedupe no
    /// matter which group the call arrives from — the workspace scans
    /// every cached surface through this before re-mounting, so a second
    /// `register_chat` can never clobber the owning group's binding (which
    /// would leave the survivor detached on the host's per-session map).
    #[gpui::test]
    fn remote_session_tab_focuses_across_groups(cx: &mut TestAppContext) {
        let (window, _host, dir) = make_remote_panes(cx, "ab12");
        window
            .update(cx, |panes, window, cx| {
                panes.open_remote_session_chat_in_active_group(
                    "sess-9",
                    "A session",
                    window,
                    cx,
                );
                // A sibling group sharing the same host scope — the split
                // path builds groups exactly this way.
                let sibling = super::super::build_group(
                    dir.path().to_path_buf(),
                    panes.remote.clone(),
                    panes.theme,
                    panes.density,
                    panes.typography.clone(),
                    panes.cli_runtime.clone(),
                    panes.notifier.clone(),
                    panes.window_active.clone(),
                    cx,
                );
                panes.groups.insert(PaneGroupId(1), sibling);
                assert!(panes.focus_remote_session_tab("sess-9", window, cx));
                assert!(!panes.focus_remote_session_tab("sess-other", window, cx));
            })
            .unwrap();
    }

    /// Two remote-scoped `ProjectPanes` over ONE host entity — the case
    /// the chat-dedupe finding named. `RemoteHost::chats` keeps one
    /// binding per session id, so the workspace probes EVERY host-keyed
    /// panes surface through `focus_remote_session_tab` before mounting:
    /// the surface owning the tab finds it (and the workspace activates
    /// its key), a sibling surface correctly reports it is not the owner
    /// — and the host still carries a single binding afterwards, because
    /// no second `register_chat` ran to overwrite it.
    #[gpui::test]
    fn remote_session_open_dedupes_across_panes(cx: &mut TestAppContext) {
        let (window_a, host, _dir_a) = make_remote_panes(cx, "ab12");
        // Second surface pinned to the SAME host — a project mount and a
        // session mount both resolve this endpoint.
        let dir_b = TempDir::new().expect("tempdir");
        cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
        let window_b = cx.add_window(|window, cx| {
            let scope = RemoteScope::new(&host, "ab12".into());
            ProjectPanes::new(
                dir_b.path().to_path_buf(),
                Some(scope),
                Theme::default(),
                Density::default(),
                Typography::default(),
                Arc::new(CliRuntime::new()),
                Arc::new(NullNotifier),
                window,
                cx,
            )
        });
        window_a
            .update(cx, |panes, window, cx| {
                panes.open_remote_session_chat_in_active_group(
                    "sess-9",
                    "A session",
                    window,
                    cx,
                );
            })
            .unwrap();
        // The host registered exactly one binding for the session.
        cx.update(|cx| {
            assert_eq!(host.read(cx).chat_title("sess-9"), Some("A session"));
        });
        // The owning surface's probe finds the tab (this is what the
        // workspace calls on each host-keyed panes before re-mounting);
        // the sibling correctly reports the tab is not its own.
        window_a
            .update(cx, |panes, window, cx| {
                assert!(panes.focus_remote_session_tab("sess-9", window, cx));
            })
            .unwrap();
        window_b
            .update(cx, |panes, window, cx| {
                assert!(!panes.focus_remote_session_tab("sess-9", window, cx));
            })
            .unwrap();
        // …and it is STILL one binding after — no second registration.
        cx.update(|cx| {
            assert_eq!(host.read(cx).chat_title("sess-9"), Some("A session"));
        });
    }
}
