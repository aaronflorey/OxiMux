//! Per-session remote Git UI. Paths are opaque host data; no local repository.
use super::*;
use super::git_rpc::{Operation, Reply};
use crate::shell::diff_view::DiffView;
use oximux_remote_proto::messages::{GitStatusWire, IndexStatusWire, WorktreeStatusWire};
use gpui_component::{Sizable, scroll::{Scrollbar, ScrollbarMode}};

pub(crate) struct RemoteGitView {
    root: super::Root,
    title: String,
    theme: Theme,
    density: Density,
    typography: Typography,
    session: Option<Arc<RemoteSession>>,
    read_only: Option<bool>,
    revision: u64,
    busy: bool,
    status: Option<GitStatusWire>,
    notice: Option<String>,
    commit: Entity<InputState>,
    clear_commit: Option<String>,
    diff: Entity<DiffView>,
    files_scroll: gpui::ScrollHandle,
    _task: Option<Task<()>>,
}

impl RemoteGitView {
    /// `root` is a session id ([`super::Root::Session`], agent-bound) or a
    /// project path ([`super::Root::Project`], the v29 browse surface).
    pub fn new(root: super::Root, title: String, theme: Theme, density: Density, typography: Typography,
        window: &mut Window, cx: &mut Context<Self>) -> Self {
        let commit = cx.new(|cx| InputState::new(window, cx).placeholder("Commit message"));
        let diff = cx.new(|cx| DiffView::new_remote(theme, density, typography.clone(), cx));
        Self { root, title, theme, density, typography, session: None, read_only: None,
            revision: 0, busy: false, status: None, notice: None, commit, clear_commit: None, diff, files_scroll: gpui::ScrollHandle::new(), _task: None }
    }

    pub fn root(&self) -> super::Root { self.root.clone() }

    pub fn set_title(&mut self, title: String, cx: &mut Context<Self>) {
        if self.title != title { self.title = title; cx.notify(); }
    }

    pub fn bind(&mut self, session: Option<Arc<RemoteSession>>, read_only: Option<bool>, cx: &mut Context<Self>) {
        self.revision += 1;
        if self.busy { self.notice = Some("Connection changed during a Git operation. Refresh before retrying a mutation.".into()); }
        self.busy = false;
        self._task = None;
        self.session = session;
        self.read_only = read_only;
        self.status = None;
        self.diff.update(cx, |diff, cx| diff.set_remote_diffs(Vec::new(), cx));
        if self.supported() { self.run(Operation::Status, cx); }
        else if self.session.is_some() && matches!(self.root, super::Root::Project(_)) {
            self.notice = Some("Update the host to protocol v29 or newer to browse a project's git.".into());
        }
        cx.notify();
    }

    pub fn set_access(&mut self, read_only: bool, cx: &mut Context<Self>) {
        self.read_only = Some(read_only);
        cx.notify();
    }

    /// A project root needs the v29 browse surface; a session root has had
    /// git RPCs since the early versions, so it needs no gate of its own.
    fn supported(&self) -> bool {
        self.session.as_ref().is_some_and(|session| match &self.root {
            super::Root::Session(_) => true,
            super::Root::Project(_) => session.host_protocol_version().is_some_and(|v| v >= oximux_remote_proto::proto::BROWSE_MIN_VERSION),
        })
    }
    fn writable(&self) -> bool { self.supported() && self.read_only == Some(false) && !self.busy }

    fn run(&mut self, operation: Operation, cx: &mut Context<Self>) {
        if self.busy || !self.supported() || (operation.mutates() && !self.writable()) { return; }
        let Some(session) = self.session.clone() else { return; };
        let root = self.root.clone();
        let revision = self.revision;
        let mutating = operation.mutates();
        let submitted = match &operation { Operation::Commit(message) => Some(message.clone()), _ => None };
        self.busy = true;
        self.notice = None;
        let (tx, rx) = tokio::sync::oneshot::channel();
        // Keep an in-flight RPC alive even if its view closes: cancellation on
        // the shared ordered transport would invalidate all other remote tabs.
        tokio::spawn(async move { let _ = tx.send(super::git_rpc::execute(&session, &root, operation).await); });
        self._task = Some(cx.spawn(async move |view, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("Git operation interrupted. Refresh before retrying a mutation.".into()));
            let _ = view.update(cx, |view, cx| view.finish(revision, mutating, submitted, result, cx));
        }));
        cx.notify();
    }

    fn finish(&mut self, revision: u64, mutating: bool, submitted: Option<String>, result: Result<Reply, String>, cx: &mut Context<Self>) {
        if self.revision != revision { return; }
        self.busy = false;
        match result {
            Ok(Reply::Status(status)) => self.status = Some(status),
            Ok(Reply::Diff(files)) => self.diff.update(cx, |diff, cx| diff.set_remote_diffs(files, cx)),
            Ok(Reply::Mutated { sha, status }) => {
                self.diff.update(cx, |diff, cx| diff.set_remote_diffs(Vec::new(), cx));
                if let Some(sha) = sha {
                    self.clear_commit = submitted;
                    self.notice = Some(format!("Committed {}", sha.chars().take(8).collect::<String>()));
                }
                match status {
                    Ok(status) => self.status = Some(status),
                    Err(error) => { self.status = None; self.notice = Some(format!("Git operation succeeded; status refresh failed: {error}")); }
                }
            }
            // A failed read (status refresh, diff preview) leaves the
            // last-known state usable; a failed mutation may have partially
            // applied, so the cached status can no longer describe the repo.
            Err(error) => { self.notice = Some(error); if mutating { self.status = None; } }
        }
        cx.notify();
    }

    fn files(&self, staged: bool, cx: &mut Context<Self>) -> gpui::Div {
        let mut list = div().flex().flex_col().gap(px(self.density.gap_inline));
        let Some(status) = &self.status else { return list; };
        let mut count = 0;
        for (i, file) in status.files.iter().enumerate() {
            let has_staged = !matches!(file.index, IndexStatusWire::Unmodified | IndexStatusWire::Untracked | IndexStatusWire::Ignored);
            let has_unstaged = !matches!(file.worktree, WorktreeStatusWire::Unmodified | WorktreeStatusWire::Ignored);
            if !(if staged { has_staged } else { has_unstaged }) { continue; }
            if count == 0 { list = list.child(if staged { "Staged changes" } else { "Unstaged changes" }); }
            count += 1;
            let path = file.path.clone();
            let change_path = path.clone();
            let untracked = matches!(file.worktree, WorktreeStatusWire::Untracked);
            list = list.child(div().flex().items_center().gap(px(self.density.gap_inline))
                .child(Button::new((if staged { "remote-staged-diff" } else { "remote-worktree-diff" }, i))
                    .label(path.clone()).ghost().disabled(self.busy || self.session.is_none())
                    .on_click(cx.listener(move |view, _, _, cx| view.run(Operation::Diff { path: path.clone(), staged, untracked }, cx))))
                .child(Button::new((if staged { "remote-unstage" } else { "remote-stage" }, i))
                    .label(if staged { "Unstage" } else { "Stage" }).ghost().disabled(!self.writable())
                    .on_click(cx.listener(move |view, _, _, cx| view.run(if staged { Operation::Unstage(change_path.clone()) } else { Operation::Stage(change_path.clone()) }, cx)))));
        }
        list
    }
}

impl Render for RemoteGitView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        if let Some(submitted) = self.clear_commit.take() && self.commit.read(cx).value().as_ref() == submitted {
            self.commit.update(cx, |input, cx| input.set_value("", window, cx));
        }
        let message = self.commit.read(cx).value().to_string();
        let staged = self.status.as_ref().is_some_and(|status| status.files.iter().any(|file|
            !matches!(file.index, IndexStatusWire::Unmodified | IndexStatusWire::Untracked | IndexStatusWire::Ignored)));
        let branch = self.status.as_ref().map(|s| format!("{} · ↑{} ↓{}", s.branch.as_deref().unwrap_or("Detached HEAD"), s.ahead, s.behind));
        let d = self.density;
        div().flex().flex_col().h_full().w_full().max_w(px(d.scale(480.0))).min_w_0().min_h_0().border_l_1().border_color(self.theme.border_inactive)
            .child(div().flex_none().p(px(d.pad_panel))
                .flex().flex_col().gap(px(d.gap_inline)).child(div().truncate().child(format!("Git · {}", self.title)))
                .when_some(branch, |row, branch| row.child(div().truncate().child(branch)))
                .child(div().flex().child(Button::new("refresh-remote-git").small().label(if self.busy { "Working…" } else { "Refresh" }).ghost()
                    .disabled(self.busy || self.session.is_none()).on_click(cx.listener(|view, _, _, cx| view.run(Operation::Status, cx)))))
                .when(self.read_only != Some(false), |row| row.child(if self.read_only == Some(true) { "Read-only" } else { "Waiting for host access" }))
                .when_some(self.notice.clone(), |row, notice| row.child(notice)))
            .when(self.status.as_ref().is_some_and(|status| !status.files.is_empty()), |body| body.child(div().relative().min_h_0().h(px(d.scale(160.0)))
                .child(div().id("remote-git-status").h_full().overflow_y_scroll().track_scroll(&self.files_scroll)
                    .p(px(d.pad_panel)).flex().flex_col().gap(px(d.gap_inline)).child(self.files(true, cx)).child(self.files(false, cx)))
                .child(Scrollbar::vertical(&self.files_scroll).mode(ScrollbarMode::Always))))
            .child(div().flex_none().p(px(d.pad_panel)).flex().flex_col().gap(px(d.gap_inline))
                .child(Input::new(&self.commit).disabled(!self.writable()))
                .child(Button::new("commit-remote-staged").debug_selector(|| "remote-git-commit".into()).label("Commit staged changes")
                    .disabled(!self.writable() || !staged || message.trim().is_empty())
                    .on_click(cx.listener(move |view, _, _, cx| view.run(Operation::Commit(message.clone()), cx)))))
            .child(div().flex_1().min_h(px(0.0)).child(self.diff.clone()))
    }
}

#[cfg(test)]
mod tests;
