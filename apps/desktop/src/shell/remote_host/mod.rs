//! One `RemoteHost` per connected endpoint.
//!
//! Owns the whole per-connection surface — the `maintain_connection` job, the
//! `RemoteSession`, the access envelope, the chat/terminal stream-recovery
//! drivers, the resource listings, and every view bound to the host — so a
//! remote-bound view behaves the same whether it lives in `RemoteWorkspace`
//! (the current surface) or in a `ProjectPanes` group alongside local tabs.
//!
//! Views register with the host when they mount; on every reconnect the host
//! rebinds them through the same `bind`/`set_remote_connection` calls the
//! workspace issued by hand before. Registration uses weak entities: a pane
//! that drops unregisters itself the next rebind pass without ceremony.
//!
//! The host is a state entity, not a widget — nothing here renders.
use std::{collections::HashMap, sync::Arc};

use gpui::{Context, Entity, EventEmitter, Task, WeakEntity};
use oximux_agents::SharedBackend;
use oximux_pty::remote_backend::{RemoteTerminalBackend, RemoteTerminalControl};
use oximux_remote_proto::{PairingTicket, ProjectSummaryWire, SessionSummary};
use oximux_remote_proto::messages::TerminalSummary;
use oximux_remote_session::{ConnState, RemoteSession};
use oximux_remote_session::hosts_store::{HostEntry, HostsFile};
use tokio::sync::mpsc;

use crate::shell::agent_chat::AgentChatView;
use crate::shell::remote_workspace::{
    Update, chat_driver, connection, files_view, git_view, resources,
    terminal_driver,
};

/// One remote-bound chat tab. `generation` stale-checks late snapshots, the
/// same contract the workspace's `ChatTab` had.
struct ChatBinding {
    view: WeakEntity<AgentChatView>,
    generation: u64,
    title: String,
}

/// Emitted for listeners that own the saved-host book (`RemoteHosts`, and the
/// transitional `RemoteWorkspace` navigator). The entity's own `cx.notify`
/// covers ordinary state changes; these exist because the book itself lives
/// off-entity until the coordinator lands.
pub(crate) enum RemoteHostEvent {
    /// The pairing save wrote the book (or an `Access` reply reconciled a
    /// row's `read_only` guess with the server's word).
    Book(Result<HostsFile, String>),
    /// The enrollment on disk refreshed this host's entry (name, key).
    Entry(HostEntry),
}
impl EventEmitter<RemoteHostEvent> for RemoteHost {}

pub(crate) struct RemoteHost {
    entry: HostEntry,
    state: ConnState,
    error: Option<String>,
    session: Option<Arc<RemoteSession>>,
    /// `(read_only, can_create)` — `None` until the host answers `ClientAccess`.
    access: Option<(bool, bool)>,
    connection: Option<connection::ConnectionJob>,
    projects: Vec<ProjectSummaryWire>,
    sessions: Vec<SessionSummary>,
    terminals: Vec<TerminalSummary>,
    resource_states: [resources::LoadState; 3],
    refresh_task: Option<tokio::task::JoinHandle<()>>,
    listings: Option<tokio::task::JoinHandle<()>>,
    creating: bool,
    creation_task: Option<tokio::task::JoinHandle<()>>,
    listing_revision: u64,
    epoch: u64,
    tx: mpsc::UnboundedSender<(u64, Update)>,
    chat_driver: Option<chat_driver::ChatDriver>,
    terminal_driver: Option<terminal_driver::TerminalDriver>,
    subscriptions: chat_driver::Subscriptions,
    next_chat_generation: u64,
    chats: HashMap<String, ChatBinding>,
    file_views: Vec<WeakEntity<files_view::RemoteFilesView>>,
    git_views: Vec<WeakEntity<git_view::RemoteGitView>>,
    /// One attachment control per host PTY this window drives. A view drop
    /// sends `Detach` through the backend; the control prunes on rebind.
    terminal_controls: HashMap<String, RemoteTerminalControl>,
    _updates: Task<()>,
}

impl RemoteHost {
    pub(crate) fn new(entry: HostEntry, cx: &mut Context<Self>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _updates = cx.spawn(async move |host, cx| {
            while let Some((epoch, update)) = rx.recv().await {
                if host
                    .update(cx, |host, cx| {
                        if epoch == host.epoch {
                            host.apply(update, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            entry,
            state: ConnState::Disconnected,
            error: None,
            session: None,
            access: None,
            connection: None,
            projects: Vec::new(),
            sessions: Vec::new(),
            terminals: Vec::new(),
            resource_states: std::array::from_fn(|_| resources::LoadState::Loading),
            refresh_task: None,
            listings: None,
            creating: false,
            creation_task: None,
            listing_revision: 0,
            epoch: 0,
            tx,
            chat_driver: None,
            terminal_driver: None,
            subscriptions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            next_chat_generation: 0,
            chats: HashMap::new(),
            file_views: Vec::new(),
            git_views: Vec::new(),
            terminal_controls: HashMap::new(),
            _updates,
        }
    }

    /// Coordinator-facing (pane mounting lands next): the host survives
    /// disconnects, so callers need its entry to key views and restore.
    #[allow(dead_code)]
    pub(crate) fn entry(&self) -> &HostEntry { &self.entry }
    pub(crate) fn state(&self) -> &ConnState { &self.state }
    pub(crate) fn error(&self) -> Option<&str> { self.error.as_deref() }
    pub(crate) fn session(&self) -> Option<Arc<RemoteSession>> { self.session.clone() }
    pub(crate) fn access(&self) -> Option<(bool, bool)> { self.access }
    pub(crate) fn projects(&self) -> &[ProjectSummaryWire] { &self.projects }
    pub(crate) fn sessions(&self) -> &[SessionSummary] { &self.sessions }
    pub(crate) fn terminals(&self) -> &[TerminalSummary] { &self.terminals }
    pub(crate) fn resource_states(&self) -> &[resources::LoadState; 3] { &self.resource_states }
    pub(crate) fn creating(&self) -> bool { self.creating }
    pub(crate) fn refreshing(&self) -> bool {
        self.refresh_task.as_ref().is_some_and(|task| !task.is_finished())
    }
    pub(crate) fn chat_title(&self, id: &str) -> Option<&str> {
        self.chats.get(id).map(|binding| binding.title.as_str())
    }
    /// Stale-reply discriminator for callers that send through the channel —
    /// only the tests stamp updates against it today.
    #[cfg(test)]
    pub(crate) fn listing_revision(&self) -> u64 { self.listing_revision }

    /// Point the connection job at the endpoint (with a pairing ticket the
    /// first time). Same `maintain_connection` machinery as before — the
    /// epoch stamp on every update keeps a stale job from talking to a new
    /// connection.
    pub(crate) fn connect(&mut self, ticket: Option<PairingTicket>, cx: &mut Context<Self>) {
        self.epoch += 1;
        self.listing_revision += 1;
        self.error = None;
        self.session = None;
        self.access = None;
        self.creating = false;
        if let Some(task) = self.creation_task.take() { task.abort(); }
        if let Some(task) = self.listings.take() { task.abort(); }
        if let Some(task) = self.refresh_task.take() { task.abort(); }
        self.chat_driver = None;
        self.terminal_driver = None;
        self.subscriptions = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        self.unbind_views(cx);
        self.projects.clear();
        self.sessions.clear();
        self.terminals.clear();
        self.state = ConnState::Connecting;
        self.connection = Some(connection::start(self.entry.clone(), ticket, self.epoch, self.tx.clone()));
        cx.notify();
    }

    /// Drop the connection job and every registration-side binding. Views stay
    /// mounted and get rebound on the next connect — detach is per-view (a
    /// terminal's `Drop` sends `Detach` through its own backend).
    #[allow(dead_code)] // used by the coordinator once panes mount hosts
    pub(crate) fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.epoch += 1;
        self.listing_revision += 1;
        self.connection = None;
        self.chat_driver = None;
        self.terminal_driver = None;
        self.subscriptions = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        if let Some(task) = self.listings.take() { task.abort(); }
        if let Some(task) = self.refresh_task.take() { task.abort(); }
        if let Some(task) = self.creation_task.take() { task.abort(); }
        self.creating = false;
        self.session = None;
        self.access = None;
        self.state = ConnState::Disconnected;
        self.unbind_views(cx);
        cx.notify();
    }

    /// Drop the connection job and every registration-side binding while
    /// keeping the entity — used on explicit disconnect.
    fn unbind_views(&mut self, cx: &mut Context<Self>) {
        for binding in self.chats.values() {
            if let Some(view) = binding.view.upgrade() {
                view.update(cx, |view, cx| view.set_remote_connection(None, None, cx));
            }
        }
        self.file_views.retain(|view| {
            if let Some(view) = view.upgrade() {
                view.update(cx, |view, cx| view.bind(None, None, cx));
                true
            } else {
                false
            }
        });
        self.git_views.retain(|view| {
            if let Some(view) = view.upgrade() {
                view.update(cx, |view, cx| view.bind(None, None, cx));
                true
            } else {
                false
            }
        });
        for control in self.terminal_controls.values() {
            control.bind(None, false);
        }
    }

    /// Register a remote-bound chat view for `id`. Idempotent per id: a
    /// re-register (same tab re-mounted after a pane teardown) bumps the
    /// generation so frames for the old entity can't land on the new one.
    pub(crate) fn register_chat(
        &mut self,
        id: String,
        title: String,
        view: &Entity<AgentChatView>,
        cx: &mut Context<Self>,
    ) {
        self.next_chat_generation += 1;
        let generation = self.next_chat_generation;
        let refresh = self.chat_driver.as_ref().map(|driver| {
            let tx = driver.tx.clone();
            let id = id.clone();
            Arc::new(move || { let _ = tx.send(chat_driver::Command::Refresh(id.clone())); })
                as Arc<dyn Fn() + Send + Sync>
        });
        let read_only = self.access.map(|(read_only, _)| read_only);
        let session = self.session.clone();
        view.update(cx, |view, cx| {
            view.set_remote_connection(session, read_only, cx);
            view.set_remote_refresh(refresh);
        });
        self.chats.insert(
            id.clone(),
            ChatBinding { view: view.downgrade(), generation, title },
        );
        if let Some(driver) = &self.chat_driver {
            let _ = driver.tx.send(chat_driver::Command::Open(id, generation));
        }
    }

    /// Drop a chat binding without touching the view — used when the pane
    /// closes its tab. The driver's `Close` unsubscribes cleanly; the server
    /// session itself is the host's, not this window's.
    pub(crate) fn unregister_chat(&mut self, id: &str) {
        self.chats.remove(id);
        if let Some(driver) = &self.chat_driver {
            let _ = driver.tx.send(chat_driver::Command::Close(id.into()));
        }
    }

    /// Register a file browser/editor surface rooted wherever the view is
    /// rooted (session or project — the view owns its `Root`).
    pub(crate) fn register_files(
        &mut self,
        view: &Entity<files_view::RemoteFilesView>,
        cx: &mut Context<Self>,
    ) {
        let session = self.session.clone();
        let read_only = self.access.map(|(read_only, _)| read_only);
        view.update(cx, |view, cx| view.bind(session, read_only, cx));
        self.file_views.push(view.downgrade());
    }

    pub(crate) fn register_git(
        &mut self,
        view: &Entity<git_view::RemoteGitView>,
        cx: &mut Context<Self>,
    ) {
        let session = self.session.clone();
        let read_only = self.access.map(|(read_only, _)| read_only);
        view.update(cx, |view, cx| view.bind(session, read_only, cx));
        self.git_views.push(view.downgrade());
    }

    /// Attach this window to a host PTY. Returns the shared backend a
    /// `TerminalView::mount` needs plus the control the caller should retain
    /// (its drop detaches). One live attachment per `pty_id` per connection —
    /// the driver's control map is keyed by PTY, so a second view of the same
    /// terminal gets `None` and the caller focuses the existing tab instead.
    pub(crate) fn attach_terminal(
        &mut self,
        pty_id: &str,
    ) -> Option<(SharedBackend, RemoteTerminalControl)> {
        if self.terminal_controls.get(pty_id).is_some_and(|control| control.is_live()) {
            return None;
        }
        let (backend, control) = RemoteTerminalBackend::new();
        let shared: SharedBackend = Arc::new(std::sync::Mutex::new(Box::new(backend)));
        if let Some(driver) = &self.terminal_driver {
            let feed = control.bind(
                Some(terminal_driver::sender(driver.tx.clone(), pty_id.to_string())),
                self.access.is_some_and(|(read_only, _)| !read_only),
            );
            let _ = driver.tx.send(terminal_driver::Command::Open(pty_id.to_string(), feed));
        }
        self.terminal_controls.insert(pty_id.to_string(), control.clone());
        Some((shared, control))
    }

    /// Fire `CreateSession` on the host; the answer arrives on the returned
    /// oneshot so the caller can open its chat tab with the real id. The
    /// server's sessions-push stream publishes the new row by itself.
    pub(crate) fn create_session(
        &mut self,
        path: String,
    ) -> Option<tokio::sync::oneshot::Receiver<Result<String, String>>> {
        if self.creating || !self.access.is_some_and(|(_, can_create)| can_create) {
            return None;
        }
        let session = self.session.clone()?;
        self.creating = true;
        let (done, rx) = tokio::sync::oneshot::channel();
        let tx = self.tx.clone();
        let epoch = self.epoch;
        let revision = self.listing_revision;
        self.creation_task = Some(tokio::spawn(async move {
            // The path is opaque host data: only the server touches its filesystem.
            let result = session.create_session(&path, None).await.map_err(|error| error.to_string());
            let _ = tx.send((epoch, Update::Created(revision, result.clone())));
            let _ = done.send(result);
        }));
        Some(rx)
    }

    /// Fire `TermSpawn` on the host; the new pty id arrives on the returned
    /// oneshot. Gated on `can_create` — the wire carries the session-creation
    /// tier, not terminal visibility. The terminal listing refreshes itself so
    /// the new row appears without a manual reload.
    pub(crate) fn spawn_terminal(
        &mut self,
        cwd: String,
        cols: u16,
        rows: u16,
    ) -> Option<tokio::sync::oneshot::Receiver<Result<String, String>>> {
        if !self.access.is_some_and(|(_, can_create)| can_create) {
            return None;
        }
        let session = self.session.clone()?;
        let (done, rx) = tokio::sync::oneshot::channel();
        let driver_tx = self.terminal_driver.as_ref().map(|driver| driver.tx.clone());
        tokio::spawn(async move {
            let result = session.term_spawn(&cwd, cols, rows).await.map_err(|error| error.to_string());
            if result.is_ok() && let Some(tx) = driver_tx {
                let _ = tx.send(terminal_driver::Command::Refresh);
            }
            let _ = done.send(result);
        });
        Some(rx)
    }

    /// Manual refresh — the same visible outcomes as the listings pump: access,
    /// projects, sessions; terminals go through the terminal driver.
    pub(crate) fn refresh_resources(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else { return; };
        if self.refresh_task.as_ref().is_some_and(|task| !task.is_finished()) { return; }
        self.resource_states = std::array::from_fn(|_| resources::LoadState::Loading);
        self.error = None;
        if let Some(driver) = &self.terminal_driver {
            let _ = driver.tx.send(terminal_driver::Command::Refresh);
        }
        let tx = self.tx.clone();
        let epoch = self.epoch;
        let revision = self.listing_revision;
        self.refresh_task = Some(tokio::spawn(async move {
            let access = publish_access(&session, &tx, epoch, revision).await;
            if !access.is_some_and(|(read_only, _)| read_only) {
                publish_projects(&session, &tx, epoch, revision).await;
            }
            // Subscribe again also repairs a previously failed initial subscription.
            match session.subscribe_sessions().await {
                Ok(sessions) => { let _ = tx.send((epoch, Update::Sessions(revision, sessions))); }
                Err(error) => {
                    let _ = tx.send((epoch, Update::ResourceError(revision, resources::Resource::Sessions, error.to_string())));
                }
            }
        }));
        cx.notify();
    }

    /// `pub(crate)` for the transport-semantics tests: they drive updates
    /// directly rather than waiting on the entity's drain task.
    pub(crate) fn apply(&mut self, update: Update, cx: &mut Context<Self>) {
        match update {
            Update::Enrollment(entry) => {
                if entry.name == self.entry.name
                    && entry.endpoint_id.eq_ignore_ascii_case(&self.entry.endpoint_id)
                {
                    self.entry = entry.clone();
                    cx.emit(RemoteHostEvent::Entry(entry));
                }
            }
            Update::Hosts(saved) => cx.emit(RemoteHostEvent::Book(saved)),
            Update::State(state) => {
                if matches!(state, ConnState::Connecting | ConnState::Connected) {
                    self.error = None;
                }
                if state != ConnState::Connected {
                    self.chat_driver = None;
                    self.terminal_driver = None;
                    self.unbind_views(cx);
                    self.session = None;
                    self.access = None;
                    self.creating = false;
                    if let Some(task) = self.creation_task.take() { task.abort(); }
                    self.listing_revision += 1;
                    if let Some(task) = self.listings.take() { task.abort(); }
                    if let Some(task) = self.refresh_task.take() { task.abort(); }
                }
                self.state = state;
            }
            Update::Connected(session) => {
                self.error = None;
                self.session = Some(session.clone());
                self.listing_revision += 1;
                let revision = self.listing_revision;
                self.terminal_driver = Some(terminal_driver::TerminalDriver::start(
                    session.clone(),
                    self.terminal_controls.iter().map(|(id, control)| (id.clone(), control.clone())).collect(),
                    self.tx.clone(),
                    self.epoch,
                    revision,
                ));
                self.chat_driver = Some(chat_driver::ChatDriver::start(
                    session.clone(),
                    self.subscriptions.clone(),
                    self.chats.iter().map(|(id, binding)| (id.clone(), binding.generation)).collect(),
                    self.tx.clone(),
                    self.epoch,
                    revision,
                ));
                self.rebind_views(cx);
                if let Some(task) = self.listings.take() { task.abort(); }
                self.resource_states = std::array::from_fn(|_| resources::LoadState::Loading);
                self.start_listings(session, revision);
            }
            Update::Access(revision, read_only, can_create) if revision == self.listing_revision => {
                self.access = Some((read_only, can_create));
                // The ticket carries no tier, so the entry saved at pairing time
                // guessed `false` — reconcile the book with what the host reports.
                if self.entry.read_only != read_only {
                    self.entry.read_only = read_only;
                    let name = self.entry.name.clone();
                    let (epoch, tx) = (self.epoch, self.tx.clone());
                    cx.background_executor().spawn(async move {
                        let saved = (|| {
                            let dir = oximux_remote_session::hosts_store::config_dir()?;
                            let hosts = HostsFile::update(&dir, |hosts| {
                                if let Some(entry) = hosts.entries.iter_mut().find(|entry| entry.name == name) {
                                    entry.read_only = read_only;
                                }
                                Ok(())
                            })?;
                            Ok::<_, oximux_remote_session::StoreError>(hosts)
                        })().map_err(|e| e.to_string());
                        let _ = tx.send((epoch, Update::Hosts(saved)));
                    }).detach();
                }
                for control in self.terminal_controls.values() {
                    control.set_writable(!read_only);
                }
                self.propagate_access(read_only, cx);
            }
            Update::Chat(revision, id, generation, result) if revision == self.listing_revision => {
                if let Some(binding) = self.chats.get(&id)
                    && binding.generation == generation
                    && let Some(view) = binding.view.upgrade()
                {
                    view.update(cx, |view, cx| match *result {
                        Ok((seq, thread, supports_steer, choices)) => {
                            view.update_remote_thread(seq, thread, supports_steer, choices, cx)
                        }
                        Err(error) => view.remote_error(error, cx),
                    });
                }
            }
            Update::Chat(..) => return,
            Update::Terminals(revision, result) if revision == self.listing_revision => match result {
                Ok(terminals) => {
                    self.terminals = terminals;
                    self.resource_states[2] = resources::LoadState::Ready;
                }
                Err(error) => self.resource_states[2] = resources::LoadState::Failed(error),
            },
            Update::Terminals(..) => return,
            Update::Projects(revision, projects) if revision == self.listing_revision => {
                self.projects = projects;
                self.resource_states[0] = resources::LoadState::Ready;
            }
            Update::Sessions(revision, mut sessions) if revision == self.listing_revision => {
                for session in &mut sessions {
                    // Older hosts use the full ID as a title until the first prompt.
                    if let Some(prefix) = session.title.strip_suffix(&session.session_id)
                        .filter(|prefix| prefix.is_empty() || prefix.ends_with(" · "))
                    {
                        session.title = format!("{prefix}New session · {}", session.session_id.chars().take(8).collect::<String>());
                    } else if session.title.trim().is_empty() {
                        session.title = format!("New session · {}", session.session_id.chars().take(8).collect::<String>());
                    }
                    if let Some(binding) = self.chats.get_mut(&session.session_id) {
                        binding.title = session.title.clone();
                    }
                }
                self.sync_git_titles(&sessions, cx);
                self.sessions = sessions;
                self.resource_states[1] = resources::LoadState::Ready;
            }
            Update::ListingError(revision, error) if revision == self.listing_revision => {
                self.error = Some(error);
            }
            Update::ResourceError(revision, resource, error) if revision == self.listing_revision => {
                self.resource_states[resource as usize] = resources::LoadState::Failed(error);
            }
            Update::ResourceError(..)
            | Update::Access(..)
            | Update::Projects(..)
            | Update::Sessions(..)
            | Update::ListingError(..) => return,
            Update::Created(revision, result) if revision == self.listing_revision => {
                self.creation_task = None;
                self.creating = false;
                if let Err(error) = result {
                    self.error = Some(error);
                }
            }
            Update::Created(..) => return,
        }
        cx.notify();
    }

    /// Rebind every registered view to `self.session`. Runs on `Connected`
    /// only — `Access` changes take the narrower `propagate_access` path.
    fn rebind_views(&mut self, cx: &mut Context<Self>) {
        let session = self.session.clone();
        let read_only = self.access.map(|(read_only, _)| read_only);
        let generations: HashMap<String, u64> = self
            .chats
            .iter()
            .map(|(id, binding)| (id.clone(), binding.generation))
            .collect();
        self.chats.retain(|id, binding| {
            let Some(generation) = generations.get(id).copied() else { return false };
            let refresh = self.chat_driver.as_ref().map(|driver| {
                let tx = driver.tx.clone();
                let id = id.clone();
                Arc::new(move || { let _ = tx.send(chat_driver::Command::Refresh(id.clone())); })
                    as Arc<dyn Fn() + Send + Sync>
            });
            if let Some(view) = binding.view.upgrade() {
                view.update(cx, |view, cx| {
                    view.set_remote_connection(session.clone(), read_only, cx);
                    view.set_remote_refresh(refresh);
                });
                binding.generation = generation;
                true
            } else {
                false
            }
        });
        self.file_views.retain(|view| {
            if let Some(view) = view.upgrade() {
                view.update(cx, |view, cx| view.bind(session.clone(), read_only, cx));
                true
            } else {
                false
            }
        });
        self.git_views.retain(|view| {
            if let Some(view) = view.upgrade() {
                view.update(cx, |view, cx| view.bind(session.clone(), read_only, cx));
                true
            } else {
                false
            }
        });
        self.terminal_controls.retain(|_, control| control.is_live());
    }

    fn propagate_access(&self, read_only: bool, cx: &mut Context<Self>) {
        for binding in self.chats.values() {
            if let Some(view) = binding.view.upgrade() {
                view.update(cx, |view, cx| view.set_remote_access(read_only, cx));
            }
        }
        for view in &self.file_views {
            if let Some(view) = view.upgrade() {
                view.update(cx, |view, cx| view.set_access(read_only, cx));
            }
        }
        for view in &self.git_views {
            if let Some(view) = view.upgrade() {
                view.update(cx, |view, cx| view.set_access(read_only, cx));
            }
        }
    }

    /// Session-rooted git views title themselves after the session row; a
    /// project-rooted view keeps the title it was mounted with.
    fn sync_git_titles(&self, sessions: &[SessionSummary], cx: &mut Context<Self>) {
        for view in self.git_views.iter().filter_map(|view| view.upgrade()) {
            let root = view.read(cx).root();
            if let crate::shell::remote_workspace::Root::Session(id) = root
                && let Some(session) = sessions.iter().find(|session| session.session_id == id)
            {
                view.update(cx, |view, cx| view.set_title(session.title.clone(), cx));
            }
        }
    }

    fn start_listings(&mut self, session: Arc<RemoteSession>, revision: u64) {
        let tx = self.tx.clone();
        let epoch = self.epoch;
        self.listings = Some(tokio::spawn(async move {
            use futures::StreamExt;
            let mut changes = session.take_sessions().expect("one listing task per connection");
            let access = publish_access(&session, &tx, epoch, revision).await;
            if !access.is_some_and(|(read_only, _)| read_only) {
                publish_projects(&session, &tx, epoch, revision).await;
            }
            match session.subscribe_sessions().await {
                Ok(sessions) => { let _ = tx.send((epoch, Update::Sessions(revision, sessions))); }
                Err(error) => {
                    let _ = tx.send((epoch, Update::ResourceError(revision, resources::Resource::Sessions, error.to_string())));
                }
            }
            while let Some(sessions) = changes.next().await {
                let _ = tx.send((epoch, Update::Sessions(revision, sessions)));
            }
        }));
    }
}

#[cfg(test)]
mod tests;

impl Drop for RemoteHost {
    fn drop(&mut self) {
        if let Some(task) = self.listings.take() { task.abort(); }
        if let Some(task) = self.refresh_task.take() { task.abort(); }
        if let Some(task) = self.creation_task.take() { task.abort(); }
        self.connection = None; // ConnectionJob::Drop stops the maintain loop.
    }
}

async fn publish_projects(
    session: &RemoteSession,
    tx: &mpsc::UnboundedSender<(u64, Update)>,
    epoch: u64,
    revision: u64,
) {
    let update = match session.list_projects().await {
        Ok(projects) => Update::Projects(revision, projects),
        Err(error) => Update::ResourceError(revision, resources::Resource::Projects, error.to_string()),
    };
    let _ = tx.send((epoch, update));
}

async fn publish_access(
    session: &RemoteSession,
    tx: &mpsc::UnboundedSender<(u64, Update)>,
    epoch: u64,
    revision: u64,
) -> Option<(bool, bool)> {
    let access = if session.host_protocol_version().is_some_and(|version| version >= 27) {
        session.client_access().await.map_err(|error| error.to_string())
    } else {
        Err("Update the host to protocol v27 or newer to verify desktop access.".into())
    };
    let update = match &access {
        Ok((read_only, can_create)) => Update::Access(revision, *read_only, *can_create),
        Err(error) => Update::ListingError(revision, format!("Cannot verify host access: {error}")),
    };
    let _ = tx.send((epoch, update));
    access.ok()
}

