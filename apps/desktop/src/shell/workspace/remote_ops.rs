//! Remote-project activation ops on `WorkspaceRoot`.
//!
//! Lifted out of `workspace_ops.rs`, which sits at the 3000-LOC hard cap
//! `xtask file-size-lint` enforces. These are the remote mirrors of the
//! local project ops there: `set_active_remote` mounts a host's project in
//! the same panes area, `open_remote_pairing` hosts the pairing modal, and
//! `build_remote_project_panes_if_absent` caches the `ProjectPanes` entity
//! per `ProjectKey` — none of which touch the local-only plumbing (DB,
//! persisted layouts, pane buffers, terminal-daemon reconcile).

use gpui::{AppContext, Context, Entity, Window};

use crate::shell::project_panes::ProjectPanes;
use crate::shell::remote_host::RemoteHost;
use crate::shell::remote_hosts::RemoteHosts;
use crate::shell::remote_scope::RemoteScope;
use crate::shell::workspace::workspace_ops::defer_focus_active;
use crate::workspace_root::{RemoteActive, WorkspaceRoot};

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
        let key = oximux_core::ProjectKey {
            host: host_id,
            project_id: project.path.clone(),
        };
        tracing::info!(endpoint = %entry.endpoint_id, path = %project.path, "active remote project set");
        self.mark_rail_dirty(cx);
        // Sidebar swap mirrors the local fast path: pause the outgoing
        // sidebar's poller, park a remote editor's dirty buffers if the
        // outgoing sidebar was remote, then reuse the cached remote sidebar
        // for this (host, project) or build one.
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
            name: project.name.clone(),
            path: project.path.clone(),
        });
        if let Some(cached) = self.right_sidebar_by_project.get(&key).cloned() {
            cached.update(cx, |s, _| s.open = prior_open);
            self.right_sidebar = Some(cached);
        } else {
            self.install_remote_sidebar(&host, &key, &project, prior_open, window, cx);
        }
        let panes = self.build_remote_project_panes_if_absent(&host, &key, &project.path, window, cx);
        self._project_panes_observer = Some(cx.observe(&panes, |_, _, cx| cx.notify()));
        self.hide_inactive_project_terminals(&key, cx);
        defer_focus_active(window, cx, panes);
        cx.notify();
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

    /// Build, cache, and mount the remote project's right sidebar. A parked
    /// editor for this `(host, project)` — dirty buffers that survived a
    /// previous sidebar swap — is reclaimed instead of minting a fresh
    /// browser.
    fn install_remote_sidebar(
        &mut self,
        host: &Entity<RemoteHost>,
        key: &oximux_core::ProjectKey,
        project: &oximux_remote_proto::ProjectSummaryWire,
        prior_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let endpoint_id = host.read(cx).entry().endpoint_id.to_lowercase();
        let draft_key = (
            endpoint_id,
            crate::shell::remote_workspace::Root::Project(project.path.clone()).key(),
        );
        let files_view = self.remote_drafts.remove(&draft_key);
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
                project.name.clone(),
                project.path.clone(),
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
        self.right_sidebar = Some(built);
    }

    /// Open the remote pairing modal over the shell. Pairing used to swap
    /// the window for a dedicated remote workspace view; with remote
    /// projects mounting in the panes area it is a form like
    /// `AddProjectDialog` — the rail row then tracks connect → connected.
    pub(crate) fn open_remote_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remote_pair_modal
            .update(cx, |modal, cx| modal.open(window, cx));
    }

    /// Build and cache a remote project's panes — the remote mirror of
    /// `build_project_panes_if_absent`: same `ProjectPanes` entity and pane
    /// group tree, with a `RemoteScope` exec target, and none of the local
    /// persistence plumbing (no persisted layout, no pane buffers, no relay
    /// attach reconcile — all three key off local project ids and the local
    /// terminal daemon). The group mounts empty; the user opens tabs and the
    /// scope routes spawn/attach to the host.
    pub(crate) fn build_remote_project_panes_if_absent(
        &mut self,
        host: &Entity<RemoteHost>,
        key: &oximux_core::ProjectKey,
        project_path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ProjectPanes> {
        if let Some(panes) = self.project_panes_by_project.get(key) {
            return panes.clone();
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
    /// click). Sessions carry no cwd in the wire summary, so the tab mounts
    /// in the host's active remote project — or, when a different host or a
    /// local project is active, the host's first listed project is mounted
    /// first so the chat still lands on the right endpoint.
    pub(crate) fn open_remote_session(
        &mut self,
        host: Entity<RemoteHost>,
        session_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (entry, title, first_project) = {
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
            (
                host.entry().clone(),
                title,
                host.projects().first().cloned(),
            )
        };
        let Some(host_id) = RemoteHosts::host_id(&entry) else {
            return;
        };
        let key = match self
            .active_remote
            .as_ref()
            .filter(|a| a.key.host == host_id)
        {
            Some(active) => active.key.clone(),
            None => {
                let Some(project) = first_project else {
                    return;
                };
                let key = oximux_core::ProjectKey {
                    host: host_id,
                    project_id: project.path.clone(),
                };
                self.set_active_remote(host, project, window, cx);
                key
            }
        };
        let Some(panes) = self.project_panes_by_project.get(&key).cloned() else {
            return;
        };
        panes.update(cx, |panes, cx| {
            panes.open_remote_session_chat_in_active_group(session_id, &title, window, cx);
        });
    }
}
