//! In-memory remote editor. No local paths, watchers, LSP, autosave, or persistence.
use super::*;
use super::files_rpc::{Operation, Reply};
use gpui_component::input::{Editor, EditorState};
use gpui_component::{Selectable, Sizable};
use oximux_remote_proto::files::{DirectoryWire, FileKindWire, TextFileWire, FILES_MIN_VERSION};

struct Buffer {
    loaded: TextFileWire,
    editor: Entity<EditorState>,
    _changes: gpui::Subscription,
}

pub(crate) struct RemoteFilesView {
    root: super::Root,
    theme: Theme,
    density: Density,
    typography: Typography,
    session: Option<Arc<RemoteSession>>,
    read_only: Option<bool>,
    revision: u64,
    busy: bool,
    reading: bool,
    directory: Option<DirectoryWire>,
    buffers: HashMap<String, Buffer>,
    active: Option<String>,
    pending: Option<TextFileWire>,
    confirm_reload: bool,
    notice: Option<String>,
    _task: Option<Task<()>>,
}

impl RemoteFilesView {
    /// `root` is a session id ([`super::Root::Session`], agent-bound) or a
    /// project path ([`super::Root::Project`], the v29 browse surface).
    pub fn new(root: super::Root, theme: Theme, density: Density, typography: Typography) -> Self {
        Self { root, theme, density, typography, session: None, read_only: None, revision: 0,
            busy: false, reading: false, directory: None, buffers: HashMap::new(), active: None, pending: None,
            confirm_reload: false, notice: None, _task: None }
    }

    pub fn bind(&mut self, session: Option<Arc<RemoteSession>>, read_only: Option<bool>, cx: &mut Context<Self>) {
        self.revision += 1;
        if self.busy { self.notice = Some("Connection changed during a file operation. Drafts are retained; reload before retrying a save.".into()); }
        self.busy = false;
        self._task = None;
        self.session = session;
        self.read_only = read_only;
        self.directory = None;
        if self.supported() { self.run(Operation::List { path: "".into(), after: None }, cx); }
        else if self.session.is_some() { self.notice = Some(format!("Update the host to protocol v{} or newer to browse files.", self.min_version())); }
        cx.notify();
    }

    pub fn set_access(&mut self, read_only: bool, cx: &mut Context<Self>) {
        self.read_only = Some(read_only);
        cx.notify();
    }

    /// A project-rooted browser needs the v29 surface; a session-rooted one
    /// the v28 files surface.
    fn min_version(&self) -> u32 {
        match &self.root {
            super::Root::Session(_) => FILES_MIN_VERSION,
            super::Root::Project(_) => oximux_remote_proto::proto::BROWSE_MIN_VERSION,
        }
    }
    fn supported(&self) -> bool {
        let min = self.min_version();
        self.session.as_ref().is_some_and(|session| session.host_protocol_version().is_some_and(|version| version >= min))
    }
    fn writable(&self) -> bool { self.supported() && self.read_only == Some(false) && !self.busy }
    fn dirty(&self, cx: &gpui::App) -> bool {
        self.active.as_ref().and_then(|path| self.buffers.get(path))
            .is_some_and(|buffer| buffer.editor.read(cx).value().as_ref() != buffer.loaded.text)
    }

    /// Every open buffer whose editor content diverged from its host
    /// baseline — the owning workspace counts them before closing a session,
    /// not just the one on screen.
    pub(crate) fn dirty_buffers(&self, cx: &gpui::App) -> usize {
        self.buffers.values().filter(|buffer| *buffer.editor.read(cx).value() != buffer.loaded.text).count()
    }

    /// One save operation per dirty buffer, in stable path order. Kept for
    /// the remote-files dirty-close path (its takeover-era caller went with
    /// the takeover shell).
    #[allow(dead_code)]
    pub(super) fn dirty_saves(&self, cx: &gpui::App) -> Vec<Operation> {
        let mut saves: Vec<_> = self.buffers.values().filter_map(|buffer| {
            let text = buffer.editor.read(cx).value().to_string();
            (text != buffer.loaded.text).then(|| Operation::Save {
                path: buffer.loaded.path.clone(), text, version: buffer.loaded.version.clone() })
        }).collect();
        saves.sort_by(|a, b| match (a, b) {
            (Operation::Save { path: a, .. }, Operation::Save { path: b, .. }) => a.cmp(b),
            _ => std::cmp::Ordering::Equal,
        });
        saves
    }

    /// See `dirty_saves` — error surfacing for the deferred save-all path.
    #[allow(dead_code)]
    pub(super) fn set_notice(&mut self, notice: Option<String>) { self.notice = notice; }

    /// The same root this view's RPCs use — so a caller (e.g. save-all on
    /// close) issues operations against the right surface instead of
    /// re-deriving the view's addressing mode.
    pub(crate) fn root(&self) -> super::Root { self.root.clone() }

    /// Serialize every dirty buffer for the durable draft store — the
    /// baseline text + version travel too, so a rehydrated buffer diffs,
    /// saves and version-checks exactly like this one would have.
    pub(crate) fn capture_drafts(
        &self,
        endpoint: &str,
        root_key: &str,
        cx: &gpui::App,
    ) -> Vec<super::draft_store::DraftEntry> {
        self.buffers.values().filter_map(|buffer| {
            let draft = buffer.editor.read(cx).value().to_string();
            (draft != buffer.loaded.text).then(|| super::draft_store::DraftEntry {
                endpoint: endpoint.to_string(),
                root: root_key.to_string(),
                path: buffer.loaded.path.clone(),
                base_text: buffer.loaded.text.clone(),
                base_version: buffer.loaded.version.clone(),
                draft,
            })
        }).collect()
    }

    /// Rehydrate stored drafts as dirty buffers — each editor holds the
    /// saved draft over the saved host baseline, so dirty state, Save and
    /// version-conflict detection behave as if the window never closed.
    /// Called only when no parked view was reclaimed (the live entity
    /// always carries fresher state than the store).
    pub(crate) fn restore_drafts(
        &mut self,
        entries: Vec<super::draft_store::DraftEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for entry in entries {
            let editor = cx.new(|cx| {
                let mut editor = EditorState::new(window, cx);
                editor.set_value(entry.draft, window, cx);
                editor
            });
            let changes = cx.observe(&editor, |_, _, cx| cx.notify());
            if self.active.is_none() {
                self.active = Some(entry.path.clone());
            }
            self.buffers.insert(entry.path.clone(), Buffer {
                loaded: TextFileWire {
                    path: entry.path,
                    text: entry.base_text,
                    version: entry.base_version,
                },
                editor,
                _changes: changes,
            });
        }
        cx.notify();
    }

    fn open(&mut self, path: String, cx: &mut Context<Self>) {
        if self.busy { return; }
        self.confirm_reload = false;
        if self.buffers.contains_key(&path) { self.active = Some(path); cx.notify(); }
        else { self.run(Operation::Read(path), cx); }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if !self.writable() || !self.dirty(cx) { return; }
        let Some(buffer) = self.active.as_ref().and_then(|path| self.buffers.get(path)) else { return; };
        self.run(Operation::Save { path: buffer.loaded.path.clone(), text: buffer.editor.read(cx).value().to_string(), version: buffer.loaded.version.clone() }, cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.busy || !self.supported() { return; }
        if self.dirty(cx) && !self.confirm_reload { self.confirm_reload = true; cx.notify(); return; }
        self.confirm_reload = false;
        if let Some(path) = self.active.clone() { self.run(Operation::Read(path), cx); }
    }

    fn run(&mut self, operation: Operation, cx: &mut Context<Self>) {
        if self.busy || !self.supported() || (operation.mutates() && !self.writable()) { return; }
        let Some(session) = self.session.clone() else { return; };
        let append = matches!(&operation, Operation::List { after: Some(_), .. });
        let revision = self.revision;
        let root = self.root.clone();
        self.reading = matches!(&operation, Operation::Read(_));
        self.busy = true;
        self.notice = None;
        let (tx, rx) = tokio::sync::oneshot::channel();
        // An ordered RPC must finish even when a panel closes, or it would poison
        // the shared transport used by the other tabs.
        tokio::spawn(async move { let _ = tx.send(super::files_rpc::execute(&session, &root, operation).await); });
        self._task = Some(cx.spawn(async move |view, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("File operation interrupted; your draft is retained.".into()));
            let _ = view.update(cx, |view, cx| view.finish(revision, append, result, cx));
        }));
        cx.notify();
    }

    fn finish(&mut self, revision: u64, append: bool, result: Result<Reply, String>, cx: &mut Context<Self>) {
        if revision != self.revision { return; }
        self.busy = false;
        self.notice = None;
        match result {
            Ok(Reply::Directory(mut page)) => {
                if append && let Some(old) = self.directory.take().filter(|old| old.path == page.path) {
                    let mut entries = old.entries;
                    entries.extend(page.entries);
                    page.entries = entries;
                }
                self.directory = Some(page);
            }
            Ok(Reply::Loaded(doc)) => self.pending = Some(doc),
            Ok(Reply::Saved(doc)) => {
                // The draft may have advanced while this save was in flight.
                // Advance only its confirmed host baseline, never its buffer.
                if let Some(buffer) = self.buffers.get_mut(&doc.path) { buffer.loaded = doc; }
                self.notice = Some("Saved on host".into());
            }
            Err(error) => self.notice = Some(error),
        }
        cx.notify();
    }

    fn install_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = self.pending.take() else { return; };
        let editor = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx);
            editor.set_value(doc.text.clone(), window, cx);
            editor
        });
        let changes = cx.observe(&editor, |_, _, cx| cx.notify());
        self.active = Some(doc.path.clone());
        self.buffers.insert(doc.path.clone(), Buffer { loaded: doc, editor, _changes: changes });
    }

    /// Test helper: plant a dirty buffer without RPC — the dirty-close
    /// regression tests plant these.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn plant_buffer(&mut self, path: &str, loaded: &str, draft: &str,
        window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx);
            editor.set_value(draft.to_string(), window, cx);
            editor
        });
        let changes = cx.observe(&editor, |_, _, cx| cx.notify());
        self.active = Some(path.to_string());
        self.buffers.insert(path.to_string(), Buffer {
            loaded: TextFileWire { path: path.into(), text: loaded.into(), version: "v0".into() },
            editor, _changes: changes });
    }
}

impl Render for RemoteFilesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        self.install_pending(window, cx);
        let d = self.density;
        let theme = self.theme;
        let current = self.directory.as_ref().map(|directory| directory.path.clone()).unwrap_or_default();
        let parent = current.rsplit_once('/').map(|(parent, _)| parent.to_owned()).unwrap_or_default();
        let mut listing = div().id("remote-files-list").w(px(d.scale(200.0))).max_w(gpui::relative(0.4)).min_w_0().overflow_y_scroll()
            .flex().flex_col().gap(px(d.gap_inline));
        listing = listing.child(div().child(if current.is_empty() { "Session files".to_string() } else { current.clone() }))
            .child(div().flex().flex_none().gap(px(d.gap_inline))
                .child(Button::new("remote-files-up").small().label("Up").ghost().disabled(self.busy || current.is_empty() || !self.supported())
                    .on_click(cx.listener(move |view, _, _, cx| view.run(Operation::List { path: parent.clone(), after: None }, cx))))
                .child(Button::new("remote-files-refresh").debug_selector(|| "remote-files-refresh".into()).small().label("Refresh").ghost().disabled(self.busy || !self.supported())
                    .on_click(cx.listener(move |view, _, _, cx| view.run(Operation::List { path: current.clone(), after: None }, cx)))));
        if let Some(directory) = &self.directory {
            for (i, entry) in directory.entries.iter().enumerate() {
                let path = if directory.path.is_empty() { entry.name.clone() } else { format!("{}/{}", directory.path, entry.name) };
                let is_directory = entry.kind == FileKindWire::Directory;
                listing = listing.child(Button::new(("remote-file-entry", i))
                    .label(format!("{}{}", if is_directory { "▸ " } else { "" }, entry.name)).ghost()
                    .selected(self.active.as_ref() == Some(&path))
                    .disabled(self.busy || !self.supported() || entry.kind == FileKindWire::Unsupported)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if is_directory { view.run(Operation::List { path: path.clone(), after: None }, cx); }
                        else { view.open(path.clone(), cx); }
                    })));
            }
            if let Some(after) = directory.next.clone() {
                let path = directory.path.clone();
                listing = listing.child(Button::new("remote-files-more").label("Load more").ghost().disabled(self.busy || !self.supported())
                    .on_click(cx.listener(move |view, _, _, cx| view.run(Operation::List { path: path.clone(), after: Some(after.clone()) }, cx))));
            }
        }
        // Buffers remain reachable when navigating away from their directory.
        for (i, path) in self.buffers.keys().filter(|path| {
            !self.directory.as_ref().is_some_and(|directory| directory.entries.iter().any(|entry|
                **path == if directory.path.is_empty() { entry.name.clone() } else { format!("{}/{}", directory.path, entry.name) }))
        }).enumerate() {
            let path = path.clone();
            listing = listing.child(Button::new(("remote-open-file", i)).label(format!("Open: {path}")).ghost().selected(self.active.as_ref() == Some(&path)).disabled(self.busy)
                .on_click(cx.listener(move |view, _, _, cx| view.open(path.clone(), cx))));
        }
        let dirty = self.dirty(cx);
        let buffer = self.active.as_ref().and_then(|path| self.buffers.get(path)).map(|buffer| buffer.editor.clone());
        div().w_full().max_w(px(d.scale(650.0))).min_w_0().border_l_1().border_color(theme.border_inactive).h_full().flex().flex_col().min_h(px(0.0)).p(px(d.pad_panel))
            .bg(theme.bg_base).text_color(theme.fg_base).text_size(px(self.typography.t_body_md))
            .child(div().flex().items_center().gap(px(d.gap_inline)).child("Host files")
                .child(Button::new("save-remote-file").label("Save").disabled(!self.writable() || !dirty)
                    .on_click(cx.listener(|view, _, _, cx| view.save(cx))))
                .child(Button::new("reload-remote-file").label("Reload").ghost().disabled(self.busy || !self.supported() || self.active.is_none())
                    .on_click(cx.listener(|view, _, _, cx| view.reload(cx))))
                .when(dirty, |row| row.child("Unsaved")))
            .when_some(self.notice.clone(), |body, notice| body.child(notice))
            .when(self.confirm_reload, |body| body.child(div().child("Discard this file's unsaved changes and reload from the host?")
                .child(Button::new("confirm-remote-reload").label("Discard and reload").on_click(cx.listener(|view, _, _, cx| view.reload(cx))))
                .child(Button::new("cancel-remote-reload").label("Keep draft").ghost().on_click(cx.listener(|view, _, _, cx| { view.confirm_reload = false; cx.notify(); })))))
            .child(div().flex().flex_1().min_h(px(0.0)).child(listing)
                .child(div().flex().flex_col().flex_1().min_w_0()
                    .when_some(self.active.clone(), |body, path| body.child(path))
                    .when_some(buffer, |body, editor| body.child(Editor::new(&editor).h_full().readonly(self.read_only != Some(false) || (self.busy && self.reading))))))
    }
}

#[cfg(test)]
mod tests;
