//! Remote-arm tab openers: `PaneGroup` extension impl for remote-scoped
//! groups.
//!
//! Lifted out of `tabs.rs`, which sits at the 3000-LOC hard cap `xtask
//! file-size-lint` enforces. Each opener is the remote mirror of a local
//! one there: the spawn/create RPC replaces the local subprocess spawn,
//! the tab lands when the answer arrives (no placeholder stage — the
//! attach/chat driver replays into the live view), and a refused call is
//! a silent no-op matching a failed local spawn. Read-only pairings and
//! dead sessions simply open nothing.

use super::*;

use crate::shell::remote_scope::RemoteScope;
use crate::shell::terminal_view::{DEFAULT_COLS, DEFAULT_ROWS};

impl PaneGroup {
    /// Remote arm of [`Self::open_terminal_tab`]: `TermSpawn` on the host at
    /// `cwd`, then a normal `TermAttach` (replay + live frames, same single
    /// terminal path the workspace used). The tab lands when the RPC
    /// answers — spawn is one round trip and the attach driver replays into
    /// an already-live view, so there is no placeholder to paint meanwhile.
    /// A refused spawn (read-only pairing, dead session, dropped group) is a
    /// silent no-op matching a failed local PTY spawn.
    pub(crate) fn open_remote_terminal_tab(
        &mut self,
        scope: RemoteScope,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.spawn_remote_terminal(scope, cwd, window, cx, |this, view, observer, window, cx| {
            let n = this.next_terminal_n;
            this.next_terminal_n += 1;
            let tab = PaneGroupTab {
                label: SharedString::from(format!("Terminal {n}")),
                content: PaneContent::Terminal(TerminalSplitTree::new_single(view, observer)),
                kind: PaneGroupTabKind::Terminal,
                color: None,
                custom_title: None,
                pinned: false,
                is_preview: false,
                external_mutation: None,
                restore_rank: None,
                _observer: None,
                _status_task: None,
            };
            this.tabs.push(tab);
            this.tab_order.push(this.tabs.len() - 1);
            this.active = this.tabs.len() - 1;
            this.bump_mru(this.active);
            this.focus_active(window, cx);
            this.pin_tab_strip_to_end();
            cx.notify();
        });
    }

    /// Shared remote spawn → attach → mount: `TermSpawn` on the host at
    /// `cwd`, a normal `TermAttach` on the answer, then `apply` positions the
    /// mounted view (new tab / extra leaf / split). One round trip — the
    /// terminal driver's replay paints into the live view, so there is no
    /// placeholder stage. A refused spawn (read-only pairing, dead session,
    /// dropped group) or a tab that vanished mid-flight is a silent no-op,
    /// matching a failed local PTY spawn.
    pub(crate) fn spawn_remote_terminal(
        &mut self,
        scope: RemoteScope,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl FnOnce(
            &mut Self,
            Entity<TerminalView>,
            Subscription,
            &mut Window,
            &mut Context<Self>,
        ) + 'static,
    ) {
        let Some(host) = scope.host.upgrade() else { return; };
        let Some(spawned) = host.update(cx, |host, _cx| {
            host.spawn_terminal(cwd.to_string_lossy().into_owned(), DEFAULT_COLS, DEFAULT_ROWS)
        }) else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(pty_id)) = spawned.await else { return; };
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(host) = scope.host.upgrade() else { return; };
                let Some((backend, _control)) =
                    host.update(cx, |host, _cx| host.attach_terminal(&pty_id))
                else {
                    return;
                };
                let ids = SurfaceIds::fresh(scope.surface_tag());
                let theme = this.theme;
                let density = this.density;
                let typography = this.typography.clone();
                let view = cx.new(|cx| {
                    TerminalView::mount(
                        backend,
                        oximux_pty::remote_backend::REMOTE_SESSION,
                        ids,
                        theme,
                        density,
                        typography,
                        window,
                        cx,
                    )
                });
                Self::wire_opener(&view, cx);
                let observer = cx.observe(&view, |_this, _view, cx| cx.notify());
                apply(this, view, observer, window, cx);
            });
        })
        .detach();
    }

    /// Remote arm of [`Self::open_agent_chat_tab`]: `CreateSession` on the
    /// host — the remote equivalent of spawning the agent subprocess
    /// locally — then a remote-bound `AgentChatView` registered through the
    /// same host hook the workspace used and pushed through the shared tab
    /// path. The sessions listing picks the row's real title up on its next
    /// refresh.
    pub(crate) fn open_remote_agent_chat_tab(
        &mut self,
        scope: RemoteScope,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = scope.host.upgrade() else { return; };
        let Some(created) = host.update(cx, |host, _cx| {
            host.create_session(cwd.to_string_lossy().into_owned())
        }) else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(session_id)) = created.await else { return; };
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(host) = scope.host.upgrade() else { return; };
                let theme = this.theme;
                let density = this.density;
                let typography = this.typography.clone();
                let id_for_view = session_id.clone();
                let view = cx.new(|cx| {
                    crate::shell::agent_chat::AgentChatView::new_remote(
                        id_for_view,
                        theme,
                        density,
                        typography,
                        window,
                        cx,
                    )
                });
                host.update(cx, |host, cx| {
                    host.register_chat(session_id.clone(), session_id.clone(), &view, cx);
                });
                this.push_agent_chat_view(view, cwd, None, window, cx);
            });
        })
        .detach();
    }
}
