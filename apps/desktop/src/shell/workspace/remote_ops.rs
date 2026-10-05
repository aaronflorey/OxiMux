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
        if let Some(outgoing_sidebar) = self.right_sidebar.as_ref() {
            outgoing_sidebar.read(cx).set_polling_focused(false);
        }
        self.right_sidebar = None;
        self.active_project = None;
        self.active_workspace_id = None;
        self.active_remote = Some(RemoteActive {
            key: key.clone(),
            host: host.downgrade(),
            name: project.name.clone(),
            path: project.path.clone(),
        });
        let panes = self.build_remote_project_panes_if_absent(&host, &key, &project.path, window, cx);
        self._project_panes_observer = Some(cx.observe(&panes, |_, _, cx| cx.notify()));
        self.hide_inactive_project_terminals(&key, cx);
        defer_focus_active(window, cx, panes);
        cx.notify();
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
}
