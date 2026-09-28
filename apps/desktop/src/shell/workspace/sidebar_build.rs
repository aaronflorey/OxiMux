//! Building a project's right sidebar, and rebuilding it once a plain folder
//! becomes a git repo.
//!
//! Lifted out of `workspace_ops.rs` (over the file-size soft cap). A sidebar
//! is built once per project and cached in `right_sidebar_by_project`; its
//! git-ness is decided by that one `Repository::open`. Nothing else ever
//! re-asks, so a project opened before `git init` would keep its
//! Explorer-only sidebar — no Source Control tab — until the app quit.
//! [`WorkspaceRoot::rebuild_sidebar_after_git_init`] is the re-ask.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{AppContext, Context, Window};

use crate::shell::right_sidebar::has_git_dir;
use crate::shell::right_sidebar::tab::RightTab;
use crate::shell::workspace::workspace_ops::refocus_active_pane;
use crate::workspace_root::WorkspaceRoot;

/// How long to wait before re-opening a repo whose `.git` exists but would
/// not open: the tick that spotted it may have landed while `git init` was
/// still writing it. One retry — a `.git` still rejected after this is
/// broken, and the rebuilt sidebar stays Explorer-only rather than looping.
const GIT_INIT_SETTLE: Duration = Duration::from_millis(500);

impl WorkspaceRoot {
    /// Build `project_id`'s sidebar against `project_root`, cache it, and
    /// show it if that project is still the active one when the build lands.
    ///
    /// `replacing` is the tab of the sidebar this build replaces in place
    /// (the `git init` rebuild): the new one opens on that tab and focus is
    /// left alone, since no user action asked for either to move. `None` is
    /// a project activation — default tab, focus back to the active pane.
    pub(crate) fn build_right_sidebar(
        &mut self,
        project_id: String,
        project_root: PathBuf,
        replacing: Option<RightTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |weak, cx| {
            // Repo presence is optional now — Repository::open may fail for
            // non-git folders. Build the sidebar in either mode: with git
            // (Source Control + Explorer + Search) or without (Explorer +
            // Search only). The Explorer + Search tabs always work from
            // `root_path` regardless of git status.
            let mut opened = oximux_git::Repository::open(&project_root).await;
            if opened.is_err() && replacing.is_some() && has_git_dir(&project_root) {
                cx.background_executor().timer(GIT_INIT_SETTLE).await;
                opened = oximux_git::Repository::open(&project_root).await;
            }
            let repo = match opened {
                Ok(r) => Some(r),
                Err(err) => {
                    tracing::info!(
                        ?err,
                        path = %project_root.display(),
                        "non-git project; building file-explorer-only sidebar"
                    );
                    None
                }
            };
            let _ = weak.update_in(cx, |this, window, cx| {
                let theme = this.theme;
                let density = this.density;
                let typography = this.typography.clone();
                // Carry the previous sidebar's open/collapsed state across
                // the rebuild — the right column must stay where the user
                // left it, not snap back open on every project switch. No
                // sidebar yet = first activation of this window, which starts
                // collapsed (the "default-collapsed on app boot" behavior).
                let prior_open = this
                    .right_sidebar
                    .as_ref()
                    .map(|s| s.read(cx).open)
                    .unwrap_or(false);
                let weak = cx.weak_entity();
                let on_open =
                    crate::workspace_root::WorkspaceRoot::build_on_open_file_callback(weak.clone());
                let on_open_diff = repo.as_ref().map(|r| {
                    crate::workspace_root::WorkspaceRoot::build_on_open_diff_callback(
                        weak.clone(),
                        r.clone(),
                    )
                });
                let on_query =
                    crate::workspace_root::WorkspaceRoot::build_on_query_active_path_callback(weak);
                let worktree_settings_repo =
                    Some(this.app_state.worktree_settings_repo.clone());
                // Phase 13: load persisted panel width clamped against
                // the current window so a too-large persisted value
                // can't overflow a newly-smaller window. The settings
                // repo is shared app-wide via the same DB handle.
                let window_width = f32::from(window.bounds().size.width);
                let settings_repo = this.app_state.settings_repo.clone();
                let initial_width = gpui::px(
                    crate::scm_layout_settings::load_panel_width(&settings_repo, window_width),
                );
                let layout_boot = crate::shell::right_sidebar::SidebarLayoutBoot {
                    initial_width: Some(initial_width),
                    settings_repo: Some(settings_repo),
                };
                let built = cx.new(|cx| {
                    crate::shell::right_sidebar::RightSidebar::new(
                        repo,
                        project_root.clone(),
                        prior_open,
                        Some(on_open),
                        on_open_diff,
                        Some(on_query),
                        worktree_settings_repo,
                        layout_boot,
                        theme,
                        density,
                        typography,
                        window,
                        cx,
                    )
                });
                // Cache the freshly built sidebar so a later switch back to
                // this project reuses it (fast path in `set_active_project`)
                // instead of rebuilding from scratch.
                let ports_panel = this.ports_panel.clone();
                let simulator_panel = this.simulator.panel(cx);
                built.update(cx, |s, cx| {
                    s.set_ports_panel(ports_panel, cx);
                    s.set_simulator_panel(simulator_panel, cx);
                    if let Some(tab) = replacing {
                        s.select_tab(tab, cx);
                    }
                });
                this.right_sidebar_by_project
                    .insert(project_id.clone(), built.clone());
                // The user may have switched projects while the repo opened;
                // the cache entry above serves them when they come back, but
                // the visible sidebar must stay the active project's.
                if this.active_project.as_ref().map(|p| p.id.as_str()) != Some(project_id.as_str())
                {
                    built.read(cx).set_polling_focused(false);
                    return;
                }
                this.right_sidebar = Some(built);
                // The rebuild minted fresh SCM panel entities — re-point
                // every source-control event subscription at them, or the
                // "View all" / commit / branch / discard / stash actions
                // would silently stop firing after a project switch.
                this.rewire_scm_subscriptions(window, cx);
                // RT-3: forward the new project to any open Tasks tab.
                let active_proj = this.active_project.clone();
                this.refresh_tasks_tab_for_active_project(active_proj, cx);
                // Re-focus the active pane after the right_sidebar
                // rebuild — the rebuild's `cx.notify` triggers a
                // repaint that can land focus on a freshly-mounted
                // sub-element of the sidebar (FileExplorer, etc.)
                // instead of the user's last-active terminal/editor.
                // Mirrors the "open project → cursor in last
                // working terminal" behavior; also keeps the chrome
                // toggle buttons routable since their actions need a
                // focused element inside the workspace_root subtree.
                if replacing.is_none() {
                    refocus_active_pane(this, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Rebuild the active project's sidebar if it was built for a plain
    /// folder that has since been `git init`-ed, so the Source Control tab
    /// appears without a restart.
    ///
    /// Cheap when there is nothing to do: a git-backed sidebar answers
    /// without touching the disk, and a plain-folder one costs a single
    /// `.git` stat. Called from the periodic diff tick; activation has its
    /// own check on the cached sidebar in `set_active_project`.
    pub(crate) fn rebuild_sidebar_after_git_init(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(project), Some(sidebar)) =
            (self.active_project.clone(), self.right_sidebar.clone())
        else {
            return;
        };
        // Right after a switch, the visible sidebar is still the previous
        // project's until the new one's build lands — never judge it
        // against the new project's root.
        if self.right_sidebar_by_project.get(&project.id) != Some(&sidebar) {
            return;
        }
        let project_root = PathBuf::from(&project.root_path);
        if !sidebar.read(cx).awaits_git_init() || !has_git_dir(&project_root) {
            return;
        }
        tracing::info!(
            project_id = %project.id,
            "project became a git repo; rebuilding its sidebar"
        );
        let tab = sidebar.update(cx, |s, _| {
            s.mark_rebuild_for_new_repo_started();
            s.active_tab
        });
        self.right_sidebar_by_project.remove(&project.id);
        self.build_right_sidebar(project.id, project_root, Some(tab), window, cx);
    }
}
