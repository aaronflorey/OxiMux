//! Diff-tab family: `PaneGroup` extension impl for every tab that renders a
//! `DiffView` — working-tree, commit, branch, stash, combined, and turn
//! diffs — plus the shared mount (`push_diff_tab`).
//!
//! Lifted out of `tabs.rs`, which sits at the 3000-LOC hard cap `xtask
//! file-size-lint` enforces. Local diffs bind a `Repository` at the group's
//! cwd; the remote path is guarded at each opener (host paths are not on
//! this machine — remote review mounts a repo-less `DiffView` via
//! `remote_tabs.rs` instead).

use super::*;

impl PaneGroup {
    /// Open a read-only diff tab for `path` (staged-vs-HEAD when
    /// `staged=true`, worktree-vs-index otherwise). Idempotent: if a
    /// diff tab for the same (path, staged) pair already exists in this
    /// group, it's activated rather than duplicated.
    ///
    /// Constructs a fresh `DiffView` entity bound to `repo` and kicks
    /// off the patch fetch via `DiffView::load`. The DiffView is then
    /// mounted as `PaneContent::Diff`.
    pub fn open_or_activate_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        path: PathBuf,
        staged: bool,
        untracked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Remote panes only mount host-backed surfaces — local-disk-backed
        // openers (editors, git diffs, local tasks/automations, restores)
        // are unreachable by design; guard so a stray dispatch is a no-op
        // rather than reading the host path on this machine.
        if self.remote.is_some() {
            return self.tabs.len();
        }

        // Already-open (same path AND same staged flag) → activate.
        // `untracked` is not part of the tab key because a file can't be
        // both tracked and untracked at the same time; whichever variant
        // opened first wins until the user closes the tab.
        if let Some(idx) = self.tabs.iter().position(|t| {
            matches!(
                &t.kind,
                PaneGroupTabKind::Diff { path: p, staged: s } if p == &path && *s == staged
            )
        }) {
            self.set_active(idx, window, cx);
            return idx;
        }
        // New diff tab path. DiffView::new takes (repo, theme, density,
        // typography, cx). Then load(path, staged, untracked, cx) kicks
        // off the async fetch — the view paints a "Loading…" state until
        // the patch arrives.
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let path_for_load = path.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load(path_for_load, staged, untracked, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let label = {
            let leaf = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("diff")
                .to_string();
            // Suffix tells the user which side they're looking at. Kept
            // short so narrow tab strips don't truncate the filename.
            let suffix = if staged { " · staged" } else { " · diff" };
            SharedString::from(format!("{leaf}{suffix}"))
        };
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::Diff { path, staged },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open or activate a commit-detail tab. Dedup key is the full
    /// SHA — clicking the same commit row twice activates the
    /// existing tab. `short_oid` and `subject` are display-only and
    /// land in the tab label.
    pub fn open_or_activate_commit_tab(
        &mut self,
        repo: oximux_git::Repository,
        sha: String,
        short_oid: String,
        subject: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Remote panes only mount host-backed surfaces — local-disk-backed
        // openers (editors, git diffs, local tasks/automations, restores)
        // are unreachable by design; guard so a stray dispatch is a no-op
        // rather than reading the host path on this machine.
        if self.remote.is_some() {
            return self.tabs.len();
        }

        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| matches!(&t.kind, PaneGroupTabKind::Commit { sha: s } if s == &sha))
        {
            self.set_active(idx, window, cx);
            return idx;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let sha_for_load = sha.clone();
        let short_for_load = short_oid.clone();
        let subject_for_load = subject.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load_commit(sha_for_load, short_for_load, subject_for_load, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        // Tab label: short SHA + truncated subject. The tab strip
        // truncates anything long, so we keep the subject readable up
        // to a sane bound rather than trying to fit the entire commit
        // message.
        let label = {
            let subject_trim: String = subject.chars().take(50).collect();
            let suffix = if subject.chars().count() > 50 {
                "…"
            } else {
                ""
            };
            SharedString::from(format!("{short_oid}: {subject_trim}{suffix}"))
        };
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::Commit { sha },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open or activate a read-only range-diff tab for one file from the
    /// "Committed on Branch" section. Dedup key is the path. `base`/`head`
    /// are the `merge_base`/`HEAD` OIDs the section was computed against;
    /// the `DiffView` loads `diff_for_range(base, head, path)`.
    pub fn open_or_activate_branch_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        base: String,
        head: String,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Remote panes only mount host-backed surfaces — local-disk-backed
        // openers (editors, git diffs, local tasks/automations, restores)
        // are unreachable by design; guard so a stray dispatch is a no-op
        // rather than reading the host path on this machine.
        if self.remote.is_some() {
            return self.tabs.len();
        }

        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| matches!(&t.kind, PaneGroupTabKind::BranchFile { path: p } if p == &path))
        {
            self.set_active(idx, window, cx);
            return idx;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let leaf = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("diff")
            .to_string();
        let title = leaf.clone();
        let path_for_load = path.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load_range(base, head, path_for_load, title, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let label = SharedString::from(format!("{leaf} · branch"));
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::BranchFile { path },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open or activate the all-files tab for one stash — "Open All Changes".
    ///
    /// Dedup key is the stash's sha, in its own [`PaneGroupTabKind::StashAll`]
    /// variant rather than [`PaneGroupTabKind::Commit`]: a stash sha is a
    /// valid commit sha, so sharing the key would let the two collide, and
    /// they do not show the same files — this one includes the untracked `^3`
    /// set.
    ///
    /// `stash_label` names the stash in the tab title. Unlike a commit tab
    /// there is no short subject to fall back on, so an unlabelled stash gets
    /// its short sha instead of an empty chip.
    pub fn open_or_activate_stash_all_tab(
        &mut self,
        repo: oximux_git::Repository,
        sha: String,
        stash_label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Remote panes only mount host-backed surfaces — local-disk-backed
        // openers (editors, git diffs, local tasks/automations, restores)
        // are unreachable by design; guard so a stray dispatch is a no-op
        // rather than reading the host path on this machine.
        if self.remote.is_some() {
            return self.tabs.len();
        }

        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| matches!(&t.kind, PaneGroupTabKind::StashAll { sha: s } if s == &sha))
        {
            self.set_active(idx, window, cx);
            return idx;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let short_oid: String = sha.chars().take(7).collect();
        let subject = if stash_label.trim().is_empty() {
            short_oid.clone()
        } else {
            stash_label.trim().to_string()
        };
        let sha_for_load = sha.clone();
        let short_for_load = short_oid.clone();
        let subject_for_load = subject.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load_stash(sha_for_load, short_for_load, subject_for_load, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        // "Stash · <message>" so the strip says what kind of thing this is —
        // a bare message is indistinguishable from a commit tab. Truncated on
        // the same bound as a commit tab's subject.
        let label = {
            let trimmed: String = subject.chars().take(50).collect();
            let suffix = if subject.chars().count() > 50 {
                "…"
            } else {
                ""
            };
            SharedString::from(format!("Stash · {trimmed}{suffix}"))
        };
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::StashAll { sha },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open or activate a read-only diff tab for one file inside a stash.
    /// Dedup key is `(sha, path)` — see [`PaneGroupTabKind::StashFile`].
    ///
    /// `base` is the diff's left side and may be **empty**, which `DiffView`
    /// reads as "no base at all": that is how an untracked file, which lives
    /// in the parentless `<sha>^3` and is absent from the stash commit's own
    /// tree, renders as the whole-file addition it is. The caller picks the
    /// pair from `StashFileOrigin`; nothing here re-derives it.
    ///
    /// `stash_label` names the stash in the tab title, because a bare file
    /// name gives the user no way to tell two stashes' copies apart once both
    /// tabs are open.
    #[allow(clippy::too_many_arguments)]
    pub fn open_or_activate_stash_file_tab(
        &mut self,
        repo: oximux_git::Repository,
        sha: String,
        base: String,
        head: String,
        path: PathBuf,
        stash_label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Remote panes only mount host-backed surfaces — local-disk-backed
        // openers (editors, git diffs, local tasks/automations, restores)
        // are unreachable by design; guard so a stray dispatch is a no-op
        // rather than reading the host path on this machine.
        if self.remote.is_some() {
            return self.tabs.len();
        }

        if let Some(idx) = self.tabs.iter().position(|t| {
            matches!(&t.kind, PaneGroupTabKind::StashFile { sha: s, path: p } if s == &sha && p == &path)
        }) {
            self.set_active(idx, window, cx);
            return idx;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let leaf = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("diff")
            .to_string();
        let title = leaf.clone();
        let path_for_load = path.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load_range(base, head, path_for_load, title, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let label = SharedString::from(format!("{leaf} · {stash_label}"));
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::StashFile { sha, path },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open or activate a combined multi-file diff tab for `scope`. Dedup
    /// key is the scope title ("All Changes" / "Staged Changes" /
    /// "Untracked" / "Branch Diff") so re-clicking the same "View all" CTA
    /// reactivates the existing tab. The new `DiffView` loads via
    /// `load_combined` — the same multi-file render path commit/branch tabs
    /// use, with per-file-group staging routing.
    pub fn open_or_activate_combined_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        scope: oximux_core::CombinedDiffScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        // Remote panes only mount host-backed surfaces — local-disk-backed
        // openers (editors, git diffs, local tasks/automations, restores)
        // are unreachable by design; guard so a stray dispatch is a no-op
        // rather than reading the host path on this machine.
        if self.remote.is_some() {
            return self.tabs.len();
        }

        // Dedup by `tab_key` (range-aware for Branch) so switching branches
        // opens a fresh tab; the display label stays the shorter `title`.
        let scope_key = SharedString::from(scope.tab_key());
        let label = SharedString::from(scope.title());
        if let Some(idx) = self.tabs.iter().position(|t| {
            matches!(&t.kind, PaneGroupTabKind::CombinedDiff { scope_key: k } if k == &scope_key)
        }) {
            self.set_active(idx, window, cx);
            return idx;
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let scope_for_load = scope.clone();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load_combined(scope_for_load, cx);
            v
        });
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let tab = PaneGroupTab {
            label,
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::CombinedDiff { scope_key },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }

    /// Open or activate a tab showing a diff the CALLER already has — an agent
    /// turn's accumulated diff, from the chat's turn-end Review.
    ///
    /// Deliberately the same tab machinery and the same `DiffView` as
    /// `open_or_activate_combined_diff_tab`; only the load differs
    /// (`load_virtual`, no repo fetch). A `Repository` is still required because
    /// `DiffView` uses it for the things that remain repo-relative even for a
    /// virtual diff — opening a file in the editor, review notes.
    pub fn open_or_activate_turn_diff_tab(
        &mut self,
        repo: oximux_git::Repository,
        key: &str,
        diff: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let scope = oximux_core::CombinedDiffScope::TurnDiff { key: key.to_string() };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let diff_owned = diff.to_string();
        let view = cx.new(|cx| {
            let mut v =
                crate::shell::diff_view::DiffView::new(repo, theme, density, typography, cx);
            v.load_virtual(scope.clone(), &diff_owned, cx);
            v
        });
        self.push_diff_tab(view, scope, window, cx)
    }

    /// Dedupe + mount shared by the local and remote turn-diff openers: one
    /// tab per `scope`, activated if it already exists.
    pub(super) fn push_diff_tab(
        &mut self,
        view: Entity<crate::shell::diff_view::DiffView>,
        scope: oximux_core::CombinedDiffScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let scope_key = SharedString::from(scope.tab_key());
        if let Some(idx) = self.tabs.iter().position(|t| {
            matches!(&t.kind, PaneGroupTabKind::CombinedDiff { scope_key: k } if k == &scope_key)
        }) {
            self.set_active(idx, window, cx);
            return idx;
        }
        let opener = cx.weak_entity();
        view.update(cx, |v, _| v.set_opener(opener));
        let observer = Some(cx.observe(&view, |_this, _v, cx| cx.notify()));
        let tab = PaneGroupTab {
            label: SharedString::from(scope.title()),
            content: PaneContent::Diff(view),
            kind: PaneGroupTabKind::CombinedDiff { scope_key },
            color: None,
            custom_title: None,
            pinned: false,
            is_preview: false,
            external_mutation: None,
            restore_rank: None,
            _observer: observer,
            _status_task: None,
        };
        self.tabs.push(tab);
        let new_idx = self.tabs.len() - 1;
        self.tab_order.push(new_idx);
        self.active = new_idx;
        self.bump_mru(new_idx);
        self.focus_active(window, cx);
        self.pin_tab_strip_to_end();
        cx.notify();
        new_idx
    }
}
