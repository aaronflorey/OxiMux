//! Remote-project activation ops on `WorkspaceRoot`.
//!
//! Lifted out of `workspace_ops.rs`, which sits at the 3000-LOC hard cap
//! `xtask file-size-lint` enforces. These are the remote mirrors of the
//! local project ops there: `set_active_remote` mounts a host's project in
//! the same panes area, `set_active_remote_session` mounts a host session's
//! own cwd surface when the pairing never got a project listing, and
//! `open_remote_terminal` attaches a rail terminal row to the host's
//! original PTY — none of which touch the local-only plumbing (DB,
//! persisted layouts, pane buffers, terminal-daemon reconcile).

use gpui::{App, AppContext, Context, Entity, Window};

use crate::shell::project_panes::ProjectPanes;
use crate::shell::remote_host::RemoteHost;
use crate::shell::remote_hosts::RemoteHosts;
use crate::shell::remote_scope::RemoteScope;
use crate::shell::remote_workspace::Root;
use crate::shell::workspace::workspace_ops::defer_focus_active;
use crate::workspace_root::{RemoteActive, WorkspaceRoot};

/// Last path component for labels when no friendlier name exists — a
/// pseudo-project keyed on a terminal's cwd, a session root, anything whose
/// wire name may be empty.
fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Pseudo-`project_id` for a session-rooted surface. The `session:` prefix
/// namespaces it away from any real project path on the same host, so its
/// panes/sidebar caches and draft keys never collide with a project mount.
fn session_project_id(session_id: &str) -> String {
    format!("session:{session_id}")
}

/// Whether a `project_id` key names a session-rooted surface — set by
/// [`session_project_id`], never by a real host project path.
fn is_session_root(project_id: &str) -> bool {
    project_id.starts_with("session:")
}

/// Where a rail session-row click's chat tab lands.
#[derive(Debug)]
enum RemoteSessionTarget {
    /// The host's currently-active remote surface (a project mount, or a
    /// session mount for THIS session).
    Active(oximux_core::ProjectKey),
    /// The host's first listed project, freshly mounted — used when no
    /// remote surface is active yet and the pairing has a project listing.
    FirstProject,
    /// The requested session's own `session:{id}` mount (cached or new).
    Session(oximux_core::ProjectKey),
}

/// Which surface a session-row click should host its chat tab on. A
/// project surface hosts any session's chat; a session-rooted surface
/// hosts only ITS OWN session's — its Explorer/Git panels answer
/// session-scoped RPCs for that session, so opening a different one must
/// mount that session's own surface instead of leaving its chat on the
/// other session's root. Pure so the selection contract is unit-testable
/// without a `WorkspaceRoot` window.
fn remote_session_target(
    active: Option<&oximux_core::ProjectKey>,
    has_projects: bool,
    session_id: &str,
    host_id: &oximux_core::HostId,
) -> RemoteSessionTarget {
    let session_root = session_project_id(session_id);
    match active {
        Some(key) if key.project_id == session_root || !is_session_root(&key.project_id) => {
            RemoteSessionTarget::Active(key.clone())
        }
        // The active surface is ANOTHER session's mount — this session
        // gets its own (cached or new) surface, never a slot on that root.
        Some(_) => RemoteSessionTarget::Session(oximux_core::ProjectKey {
            host: host_id.clone(),
            project_id: session_root,
        }),
        None if has_projects => RemoteSessionTarget::FirstProject,
        None => RemoteSessionTarget::Session(oximux_core::ProjectKey {
            host: host_id.clone(),
            project_id: session_root,
        }),
    }
}

impl WorkspaceRoot {
    /// Activate a remote host's project in the same panes area a local
    /// project uses. Mirrors `set_active_project` where the concepts carry
    /// over — dirty rail, outgoing-sidebar pause, terminal throttle,
    /// observer + focus swap — and deliberately skips what doesn't: no
    /// persisted layout, no pane buffers, no relay reconcile, no
    /// `Repository` sidebar (all local-disk plumbing). `active_project`
    /// clears so every local-only surface (worktrees, merges, schedules)
    /// reads "no local project" instead of acting on the wrong host.
    pub(crate) fn set_active_remote(
        &mut self,
        host: Entity<RemoteHost>,
        project: oximux_remote_proto::ProjectSummaryWire,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry = host.read(cx).entry().clone();
        let Some(host_id) = RemoteHosts::host_id(&entry) else {
            return;
        };
        tracing::info!(endpoint = %entry.endpoint_id, path = %project.path, "active remote project set");
        let key = oximux_core::ProjectKey {
            host: host_id,
            project_id: project.path.clone(),
        };
        self.activate_remote_surface(
            &host,
            key,
            project.name.clone(),
            project.path.clone(),
            Root::Project(project.path),
            window,
            cx,
        );
    }

    /// Mount a host SESSION's own surface — chat tab plus file/git panels
    /// rooted at the session's cwd (`Root::Session`), for pairings that
    /// never received a project listing: fresh read-only enrollments skip
    /// `ListProjects`, and session-scoped tickets must not widen beyond
    /// their session anyway. The mount keys on `session:{id}` so each
    /// session keeps its own panes/sidebar and confinement is preserved.
    pub(crate) fn set_active_remote_session(
        &mut self,
        host: Entity<RemoteHost>,
        session_id: &str,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry = host.read(cx).entry().clone();
        let Some(host_id) = RemoteHosts::host_id(&entry) else {
            return;
        };
        let name = if title.is_empty() {
            session_id.to_string()
        } else {
            title.to_string()
        };
        let key = oximux_core::ProjectKey {
            host: host_id,
            project_id: session_project_id(session_id),
        };
        self.activate_remote_surface(
            &host,
            key,
            name,
            session_id.to_string(),
            Root::Session(session_id.to_string()),
            window,
            cx,
        );
    }

    /// Shared remote activation: park the outgoing remote sidebar's dirty
    /// buffers, swap in the cached (and re-bound) or freshly-built sidebar
    /// for this key, mount the cached/new panes, focus them. Everything
    /// activation-flavoured lives here so project rows, session rows, and
    /// dedupe hits all converge on one path.
    #[allow(clippy::too_many_arguments)]
    fn activate_remote_surface(
        &mut self,
        host: &Entity<RemoteHost>,
        key: oximux_core::ProjectKey,
        name: String,
        label_path: String,
        root: Root,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mark_rail_dirty(cx);
        let prior_open = self
            .right_sidebar
            .as_ref()
            .map(|s| s.read(cx).open)
            .unwrap_or(false);
        if let Some(outgoing_sidebar) = self.right_sidebar.as_ref() {
            outgoing_sidebar.read(cx).set_polling_focused(false);
        }
        self.park_remote_sidebar_drafts(cx);
        self.active_project = None;
        self.active_workspace_id = None;
        self.active_remote = Some(RemoteActive {
            key: key.clone(),
            host: host.downgrade(),
            name: name.clone(),
            path: label_path,
        });
        if let Some(cached) = self.right_sidebar_by_project.get(&key).cloned() {
            cached.update(cx, |s, cx| {
                s.open = prior_open;
                // The cache outlives its host entity (forget-and-re-pair
                // mints a fresh one) — re-register the panels when this
                // sidebar was bound to a different entity. Dirty buffers
                // ride through untouched.
                s.rebind_remote_host(host, cx);
            });
            self.right_sidebar = Some(cached);
        } else {
            self.install_remote_sidebar(host, &key, &name, &root, prior_open, window, cx);
        }
        // The panes' cwd only labels + seeds remote terminal spawns —
        // session mounts have no host path, so spawn's cwd is the host
        // fs root; nothing ever stats it locally.
        let cwd = match &root {
            Root::Project(path) => path.clone(),
            Root::Session(_) => "/".to_string(),
        };
        let panes = self.build_remote_project_panes_if_absent(host, &key, &cwd, window, cx);
        self._project_panes_observer = Some(cx.observe(&panes, |_, _, cx| cx.notify()));
        self.hide_inactive_project_terminals(&key, cx);
        defer_focus_active(window, cx, panes);
        cx.notify();
    }

    /// Re-activate a remote surface that already has a cached sidebar —
    /// the dedupe path when a session chat or terminal tab is found living
    /// under a different (host, key) mount than the active one. The name /
    /// label come from the cached panels' `Root` and the host's current
    /// listings so a rename since the original mount shows through.
    fn activate_cached_remote_surface(
        &mut self,
        host: &Entity<RemoteHost>,
        key: &oximux_core::ProjectKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(sidebar) = self.right_sidebar_by_project.get(key).cloned() else {
            return;
        };
        let Some(root) = sidebar
            .read(cx)
            .remote_panels()
            .map(|panels| panels.files.read(cx).root().clone())
        else {
            return;
        };
        let (name, label_path) = match &root {
            Root::Project(path) => {
                let host = host.read(cx);
                let name = host
                    .projects()
                    .iter()
                    .find(|p| p.path == *path)
                    .map(|p| p.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| basename(path));
                (name, path.clone())
            }
            Root::Session(id) => {
                let host = host.read(cx);
                let name = host
                    .sessions()
                    .iter()
                    .find(|s| s.session_id == *id)
                    .map(|s| s.title.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| id.clone());
                (name, id.clone())
            }
        };
        self.activate_remote_surface(host, key.clone(), name, label_path, root, window, cx);
    }

    /// Park the outgoing sidebar's host-file editor under its
    /// `(endpoint, surface)` key when it still has dirty buffers — the same
    /// reclaim contract the takeover workspace kept (`stash_view_drafts`),
    /// so swapping projects or hosts never silently destroys unsaved edits.
    /// Called by every `right_sidebar` assignment path (remote activation
    /// here, local `install_right_sidebar` via the same helper).
    pub(crate) fn park_remote_sidebar_drafts(&mut self, cx: &mut Context<Self>) {
        let Some(sidebar) = self.right_sidebar.clone() else {
            return;
        };
        let Some(remote) = sidebar.read(cx).remote_panels() else {
            return;
        };
        if remote.files.read(cx).dirty_buffers(cx) == 0 {
            return;
        }
        let draft_key = (
            remote.endpoint_id.clone(),
            remote.files.read(cx).root().key(),
        );
        self.remote_drafts.insert(draft_key, remote.files.clone());
    }

    /// Persist every dirty host-file buffer this window still holds —
    /// parked draft views plus each cached remote sidebar's live files
    /// panel — into the durable draft store. Called by `capture_session`
    /// (quit and last-window close) and by the window-close hook before a
    /// non-last window's `WorkspaceRoot` drops, mirroring how local pane
    /// buffers survive: the next mount of the same `(endpoint, surface)`
    /// rehydrates the drafts via `install_remote_sidebar`.
    pub fn capture_remote_drafts(&self, cx: &App) {
        let mut owned: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        let mut entries = Vec::new();
        let mut seen: std::collections::HashSet<(String, String, String)> =
            std::collections::HashSet::new();
        // Parked views and live sidebars can alias the same files entity —
        // `seen` keeps a shared buffer from serializing twice.
        let mut collect = |files: &Entity<crate::shell::remote_workspace::RemoteFilesView>,
                           endpoint: &str,
                           root_key: &str,
                           cx: &App| {
            owned.insert((endpoint.to_string(), root_key.to_string()));
            for entry in files.read(cx).capture_drafts(endpoint, root_key, cx) {
                if seen.insert((entry.endpoint.clone(), entry.root.clone(), entry.path.clone())) {
                    entries.push(entry);
                }
            }
        };
        for ((endpoint, root_key), view) in &self.remote_drafts {
            collect(view, endpoint, root_key, cx);
        }
        for sidebar in self.right_sidebar_by_project.values() {
            let Some(remote) = sidebar.read(cx).remote_panels() else {
                continue;
            };
            let root_key = remote.files.read(cx).root().key();
            collect(&remote.files, &remote.endpoint_id, &root_key, cx);
        }
        crate::shell::remote_workspace::draft_store::replace_for(&owned, entries);
    }

    /// Build, cache, and mount the remote surface's right sidebar. A parked
    /// editor for this `(host, root)` — dirty buffers that survived a
    /// previous sidebar swap — is reclaimed instead of minting a fresh
    /// browser. With no parked view, the durable draft store still applies:
    /// drafts captured before a window close or quit rehydrate into the
    /// fresh browser as dirty buffers.
    #[allow(clippy::too_many_arguments)]
    fn install_remote_sidebar(
        &mut self,
        host: &Entity<RemoteHost>,
        key: &oximux_core::ProjectKey,
        name: &str,
        root: &Root,
        prior_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let endpoint_id = host.read(cx).entry().endpoint_id.to_lowercase();
        let draft_key = (endpoint_id.clone(), root.key());
        let files_view = self.remote_drafts.remove(&draft_key);
        // The store drains on every mount: a parked view wins (fresher),
        // and its entries would only resurrect a stale draft later.
        let restored = if files_view.is_none() {
            crate::shell::remote_workspace::draft_store::take(&endpoint_id, &root.key())
        } else {
            crate::shell::remote_workspace::draft_store::take(&endpoint_id, &root.key());
            Vec::new()
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let window_width = f32::from(window.bounds().size.width);
        let settings_repo = self.app_state.settings_repo.clone();
        let layout_boot = crate::shell::right_sidebar::SidebarLayoutBoot {
            initial_width: Some(gpui::px(
                crate::scm_layout_settings::load_panel_width(&settings_repo, window_width),
            )),
            settings_repo: Some(settings_repo),
        };
        let built = cx.new(|cx| {
            crate::shell::right_sidebar::RightSidebar::new_remote(
                host,
                name.to_string(),
                root.clone(),
                files_view,
                prior_open,
                layout_boot,
                theme,
                density,
                typography,
                window,
                cx,
            )
        });
        self.right_sidebar_by_project.insert(key.clone(), built.clone());
        self.right_sidebar = Some(built.clone());
        // Lift the entity out of the `read` borrow before `update`-ing it.
        let files_entity = (!restored.is_empty())
            .then(|| built.read(cx).remote_panels().map(|p| p.files.clone()))
            .flatten();
        if let Some(files_entity) = files_entity {
            files_entity.update(cx, |files, cx| files.restore_drafts(restored, window, cx));
        }
    }

    /// Open the remote pairing modal over the shell. Pairing used to swap
    /// the window for a dedicated remote workspace view; with remote
    /// projects mounting in the panes area it is a form like
    /// `AddProjectDialog` — the rail row then tracks connect → connected.
    pub(crate) fn open_remote_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remote_pair_modal
            .update(cx, |modal, cx| modal.open(window, cx));
    }

    /// Build and cache a remote surface's panes — the remote mirror of
    /// `build_project_panes_if_absent`: same `ProjectPanes` entity and pane
    /// group tree, with a `RemoteScope` exec target, and none of the local
    /// persistence plumbing (no persisted layout, no pane buffers, no relay
    /// attach reconcile — all three key off local project ids and the local
    /// terminal daemon). The group mounts empty; the user opens tabs and the
    /// scope routes spawn/attach to the host.
    ///
    /// Cache hits rebind when the host ENTITY was replaced underneath them
    /// (forget-and-re-pair, or dialing a sibling enrollment on the same
    /// endpoint): the cached `RemoteScope`s and chat registrations point at
    /// the dead entity until then.
    pub(crate) fn build_remote_project_panes_if_absent(
        &mut self,
        host: &Entity<RemoteHost>,
        key: &oximux_core::ProjectKey,
        project_path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ProjectPanes> {
        if let Some(panes) = self.project_panes_by_project.get(key) {
            let panes = panes.clone();
            panes.update(cx, |panes, cx| panes.rebind_remote_host(host, cx));
            return panes;
        }
        let entry = host.read(cx).entry().clone();
        let scope = RemoteScope::new(host, entry.endpoint_id);
        let cwd = std::path::PathBuf::from(project_path);
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let cli_runtime = self.cli_runtime.clone();
        let notifier = self.notifier.clone();
        let panes = cx.new(|cx| {
            ProjectPanes::new(
                cwd,
                Some(scope),
                theme,
                density,
                typography,
                cli_runtime,
                notifier,
                window,
                cx,
            )
        });
        self.project_panes_by_project.insert(key.clone(), panes.clone());
        panes
    }

    /// Open a chat tab bound to an existing host session (a rail session-row
    /// click). Sessions carry no cwd in the wire summary. Resolution order:
    ///
    /// 1. A chat already open ANYWHERE on this host wins focus — host
    ///    bindings are one-per-session, so a second `register_chat` in
    ///    another pane would steal the stream and a close would unregister
    ///    the survivor. When it lives under a different mounted surface the
    ///    whole surface activates so the focused tab is visible.
    /// 2. The host's ACTIVE remote surface — the common re-click.
    /// 3. The host's first listed project — the chat lands on the right
    ///    endpoint even when a local project was active.
    /// 4. A session-rooted mount — fresh read-only / session-scoped
    ///    pairings have no project listing, so the session supplies its own
    ///    file/git surface instead of the open being dropped.
    pub(crate) fn open_remote_session(
        &mut self,
        host: Entity<RemoteHost>,
        session_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry = host.read(cx).entry().clone();
        let Some(host_id) = RemoteHosts::host_id(&entry) else {
            return;
        };
        let already_open = self
            .project_panes_by_project
            .iter()
            .filter(|(key, _)| key.host == host_id)
            .find_map(|(key, panes)| {
                panes
                    .update(cx, |panes, cx| {
                        panes.focus_remote_session_tab(session_id, window, cx)
                    })
                    .then(|| key.clone())
            });
        if let Some(key) = already_open {
            if self.active_remote.as_ref().map(|a| &a.key) != Some(&key) {
                self.activate_cached_remote_surface(&host, &key, window, cx);
            }
            return;
        }
        let (title, first_project) = {
            let host = host.read(cx);
            // Replaying a session needs its live stream — a disconnected or
            // mid-reconnect host would mount an empty view instead.
            if host.session().is_none() {
                return;
            }
            let title = host
                .sessions()
                .iter()
                .find(|s| s.session_id == session_id)
                .map(|s| s.title.clone())
                .unwrap_or_default();
            (title, host.projects().first().cloned())
        };
        let key = match remote_session_target(
            self.active_remote
                .as_ref()
                .filter(|a| a.key.host == host_id)
                .map(|a| &a.key),
            first_project.is_some(),
            session_id,
            &host_id,
        ) {
            RemoteSessionTarget::Active(key) => key,
            RemoteSessionTarget::FirstProject => {
                let project = first_project.expect("target implies a listing");
                let key = oximux_core::ProjectKey {
                    host: host_id,
                    project_id: project.path.clone(),
                };
                self.set_active_remote(host.clone(), project, window, cx);
                key
            }
            RemoteSessionTarget::Session(key) => {
                if self.right_sidebar_by_project.contains_key(&key) {
                    self.activate_cached_remote_surface(&host, &key, window, cx);
                } else {
                    self.set_active_remote_session(
                        host.clone(),
                        session_id,
                        &title,
                        window,
                        cx,
                    );
                }
                key
            }
        };
        let Some(panes) = self.project_panes_by_project.get(&key).cloned() else {
            return;
        };
        panes.update(cx, |panes, cx| {
            panes.rebind_remote_host(&host, cx);
            panes.open_remote_session_chat_in_active_group(session_id, &title, window, cx);
        });
    }

    /// Attach a tab to an EXISTING host PTY (a rail terminal-row click).
    /// Resolution order mirrors `open_remote_session`: an already-attached
    /// view anywhere on this host wins focus; else the host's active remote
    /// surface; else the PTY's own cwd mounted as a project surface — the
    /// host reports terminals whose cwd never appeared in `ListProjects`
    /// (a read-only pairing's listing is empty by design). Attaching is
    /// `TermAttach` only: the original PTY replays into the new view, never
    /// a replacement spawn, and read-only pairings attach watch-only.
    pub(crate) fn open_remote_terminal(
        &mut self,
        host: Entity<RemoteHost>,
        pty_id: &str,
        cwd: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry = host.read(cx).entry().clone();
        let Some(host_id) = RemoteHosts::host_id(&entry) else {
            return;
        };
        let already_open = self
            .project_panes_by_project
            .iter()
            .filter(|(key, _)| key.host == host_id)
            .find_map(|(key, panes)| {
                panes
                    .update(cx, |panes, cx| {
                        panes.focus_remote_terminal_tab(pty_id, window, cx)
                    })
                    .then(|| key.clone())
            });
        if let Some(key) = already_open {
            if self.active_remote.as_ref().map(|a| &a.key) != Some(&key) {
                self.activate_cached_remote_surface(&host, &key, window, cx);
            }
            return;
        }
        if host.read(cx).session().is_none() {
            return;
        }
        let key = match self
            .active_remote
            .as_ref()
            .filter(|a| a.key.host == host_id)
        {
            Some(active) => active.key.clone(),
            None => {
                let key = oximux_core::ProjectKey {
                    host: host_id,
                    project_id: cwd.to_string(),
                };
                let project = host
                    .read(cx)
                    .projects()
                    .iter()
                    .find(|p| p.path == cwd)
                    .cloned()
                    .unwrap_or_else(|| oximux_remote_proto::ProjectSummaryWire {
                        name: basename(cwd),
                        path: cwd.to_string(),
                    });
                self.set_active_remote(host.clone(), project, window, cx);
                key
            }
        };
        let Some(panes) = self.project_panes_by_project.get(&key).cloned() else {
            return;
        };
        let label = basename(cwd);
        panes.update(cx, |panes, cx| {
            panes.rebind_remote_host(&host, cx);
            panes.open_remote_terminal_in_active_group(pty_id, &label, window, cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> oximux_core::HostId {
        oximux_core::HostId::Remote([7; 32])
    }

    fn key(host: &oximux_core::HostId, project_id: &str) -> oximux_core::ProjectKey {
        oximux_core::ProjectKey { host: host.clone(), project_id: project_id.to_string() }
    }

    /// The regression: a session-rooted surface reused for a DIFFERENT
    /// session leaves its Explorer/Git panels answering the wrong session's
    /// RPCs — session B must mount its own `session:{B}` surface.
    #[test]
    fn session_target_rejects_another_sessions_root() {
        let host = host();
        let active_a = key(&host, "session:sess-a");
        match remote_session_target(Some(&active_a), false, "sess-b", &host) {
            RemoteSessionTarget::Session(key) => {
                assert_eq!(key.project_id, "session:sess-b");
            }
            other => panic!("session B must mount its own surface, got {other:?}"),
        }
        // The same session's mount IS reused — opening A again re-focuses it.
        match remote_session_target(Some(&active_a), false, "sess-a", &host) {
            RemoteSessionTarget::Active(key) => assert_eq!(key.project_id, "session:sess-a"),
            other => panic!("re-opening session A must reuse its mount, got {other:?}"),
        }
    }

    /// A project surface hosts any session's chat — session B's tab lands
    /// on the mounted project, whose panels answer project-scoped RPCs.
    #[test]
    fn session_target_reuses_project_surfaces() {
        let host = host();
        let project = key(&host, "/work/repo");
        match remote_session_target(Some(&project), true, "sess-b", &host) {
            RemoteSessionTarget::Active(key) => assert_eq!(key.project_id, "/work/repo"),
            other => panic!("a project surface hosts any session, got {other:?}"),
        }
    }

    /// No active surface: the first project wins when the host lists any —
    /// the session-mount fallback is only for listings that never came.
    #[test]
    fn session_target_without_active_surface() {
        let host = host();
        assert!(matches!(
            remote_session_target(None, true, "sess-b", &host),
            RemoteSessionTarget::FirstProject
        ));
        match remote_session_target(None, false, "sess-b", &host) {
            RemoteSessionTarget::Session(key) => {
                assert_eq!(key.project_id, "session:sess-b");
            }
            other => panic!("no listing → the session's own mount, got {other:?}"),
        }
    }
}
