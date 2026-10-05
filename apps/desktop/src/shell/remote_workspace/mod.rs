//! Outbound remote workspace. Server paths never enter the local project registry.
use std::{sync::Arc, collections::HashMap};

use gpui::{AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, Styled, StatefulInteractiveElement, Task, Window, FocusHandle, Focusable, div, px};
use gpui::prelude::FluentBuilder;
use gpui_component::{Disableable, button::{Button, ButtonVariants}, input::{Input, InputState}};
use oximux_remote_proto::{PairingTicket, ProjectSummaryWire, SessionSummary};
use oximux_remote_session::{ConnState, RemoteSession};
use oximux_remote_session::hosts_store::{HostEntry, HostsFile, endpoint_id_hex};
use oximux_settings::{Density, Theme, Typography};
use tokio::sync::mpsc;

mod connection;
mod render;
mod navigator;
mod resources;
mod chat_driver;
mod terminal_driver;
mod terminals;
mod git_rpc;
mod git_view;
mod files_rpc;
mod files_view;
pub(crate) use files_view::RemoteFilesView;
pub(crate) mod restore;
use crate::shell::agent_chat::AgentChatView;

struct ChatTab { view: Entity<AgentChatView>, git: Entity<git_view::RemoteGitView>, files: Entity<files_view::RemoteFilesView>, generation: u64, title: String }

/// Where a remote file/git operation is rooted: a session id (the
/// agent-bound surface available since v1) or a project path (the v29 browse
/// surface — same verbs, no session, no agent spawn on the host).
#[derive(Clone)]
pub(crate) enum Root {
    Session(String),
    // Constructed only by project-browse panes (LeftRail remote mounting lands
    // next) and by the rpc tests — the wire/handlers for it already exist.
    #[allow(dead_code)]
    Project(String),
}

/// Host-file editors parked while their workspace is torn down — keyed by
/// (host endpoint, session) so drafts from one host can never appear on
/// another's identically-named session. Lives on `WorkspaceRoot` across a
/// Back-to-local hop so the drafts survive the workspace entity itself.
pub(crate) type DraftFiles = HashMap<(String, String), Entity<RemoteFilesView>>;

/// A folded chat state or its open/recovery failure. Boxed at the variant site:
/// `ChatThread` is large enough that an inline `Result` would blow up `Update`.
pub(super) type ChatSnapshot = Result<(u64, oximux_agents::thread::ChatThread, bool, Option<oximux_remote_proto::proto::SessionChoices>), String>;

pub(super) enum Update {
    Hosts(Result<HostsFile, String>),
    Enrollment(HostEntry),
    State(ConnState),
    Connected(Arc<RemoteSession>),
    Access(u64, bool, bool),
    Projects(u64, Vec<ProjectSummaryWire>),
    Sessions(u64, Vec<SessionSummary>),
    Terminals(u64, Result<Vec<oximux_remote_proto::messages::TerminalSummary>, String>),
    Created(u64, Result<String, String>),
    ListingError(u64, String),
    ResourceError(u64, resources::Resource, String),
    Chat(u64, String, u64, Box<ChatSnapshot>),
}

pub struct RemoteWorkspace {
    theme: Theme,
    focus: FocusHandle,
    density: Density,
    typography: Typography,
    hosts: HostsFile,
    hosts_loaded: bool,
    pending_restore: Option<restore::Selection>,
    selected: Option<HostEntry>,
    state: ConnState,
    error: Option<String>,
    name: Entity<InputState>,
    ticket: Entity<InputState>,
    show_pairing: bool,
    show_ticket: bool,
    session: Option<Arc<RemoteSession>>,
    projects: Vec<ProjectSummaryWire>,
    resource_states: [resources::LoadState; 3],
    refresh_task: Option<tokio::task::JoinHandle<()>>,
    sidebar_open: bool,
    navigator_open: bool,
    filter: Entity<InputState>,
    _filter_subscription: gpui::Subscription,
    sessions: Vec<SessionSummary>,
    creating: bool,
    access: Option<(bool, bool)>,
    listing_revision: u64,
    epoch: u64,
    tx: mpsc::UnboundedSender<(u64, Update)>,
    connection: Option<connection::ConnectionJob>,
    listings: Option<tokio::task::JoinHandle<()>>,
    creation_task: Option<tokio::task::JoinHandle<()>>,
    chats: HashMap<String, ChatTab>,
    active_chat: Option<String>,
    show_git: bool,
    show_files: bool,
    terminals: Vec<oximux_remote_proto::messages::TerminalSummary>,
    terminal_tabs: HashMap<String, terminals::TerminalTab>,
    active_terminal: Option<String>,
    terminal_driver: Option<terminal_driver::TerminalDriver>,
    next_chat_generation: u64,
    chat_driver: Option<chat_driver::ChatDriver>,
    subscriptions: chat_driver::Subscriptions,
    /// Dirty host-file editors parked by the last teardown, waiting for their
    /// owning session to be opened again on the same host.
    drafts: DraftFiles,
    /// Chat id awaiting the user's save/discard/cancel on close.
    pending_close: Option<String>,
    close_task: Option<Task<()>>,
    _updates: Task<()>,
}

impl RemoteWorkspace {
    pub fn new(theme: Theme, density: Density, typography: Typography,
        window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_hosts(theme, density, typography, window, cx, || {
            oximux_remote_session::hosts_store::config_dir()
                .and_then(|dir| HostsFile::load(&dir)).map_err(|e| e.to_string())
        })
    }

    /// The saved-host book arrives on the update channel like every other remote
    /// read; tests inject it so a developer machine's real hosts file can never
    /// race a simulated click or bounds assertion.
    pub(super) fn with_hosts(theme: Theme, density: Density, typography: Typography,
        window: &mut Window, cx: &mut Context<Self>,
        load: impl FnOnce() -> Result<HostsFile, String> + Send + 'static) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Host name"));
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Find a remote session or terminal…"));
        let _filter_subscription = cx.observe(&filter, |_, _, cx| cx.notify());
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let ticket = cx.new(|cx| InputState::new(window, cx).placeholder("Connection URL or pairing ticket").masked(true));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _updates = cx.spawn(async move |view, cx| {
            while let Some((epoch, update)) = rx.recv().await {
                if view.update(cx, |view, cx| {
                    if epoch == view.epoch { view.apply(update, cx); }
                }).is_err() { break; }
            }
        });
        let load_tx = tx.clone();
        cx.background_executor().spawn(async move {
            let _ = load_tx.send((0, Update::Hosts(load())));
        }).detach();
        Self { theme, focus, density, typography, hosts: HostsFile::default(), hosts_loaded: false, pending_restore: None, selected: None,
            state: ConnState::Disconnected, error: None, name, ticket, show_pairing: false, show_ticket: false,
            session: None, resource_states: std::array::from_fn(|_| resources::LoadState::Loading), refresh_task: None, sidebar_open: true, navigator_open: false, filter, _filter_subscription, projects: Vec::new(), sessions: Vec::new(), creating: false, access: None, listing_revision: 0, epoch: 0, tx,
            connection: None, listings: None, creation_task: None, chats: HashMap::new(), active_chat: None, show_git: false, show_files: false,
            terminals: Vec::new(), terminal_tabs: HashMap::new(), active_terminal: None, terminal_driver: None,
            next_chat_generation: 0, chat_driver: None, subscriptions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            drafts: HashMap::new(), pending_close: None, close_task: None, _updates }
    }

    fn apply(&mut self, update: Update, cx: &mut Context<Self>) {
        match update {
            Update::Enrollment(host) => {
                if self.selected.as_ref().is_some_and(|selected|
                    selected.name == host.name && selected.endpoint_id.eq_ignore_ascii_case(&host.endpoint_id)) {
                    self.selected = Some(host);
                }
            }
            Update::Hosts(Ok(hosts)) => { self.hosts = hosts; self.hosts_loaded = true; }
            Update::Hosts(Err(error)) => { self.error = Some(error); }
            Update::State(state) => {
                if matches!(state, ConnState::Connecting | ConnState::Connected) { self.error = None; }
                if state != ConnState::Connected {
                    self.chat_driver = None;
                    self.terminal_driver = None;
                    self.bind_terminals();
                    for chat in self.chats.values() {
                        chat.view.update(cx, |view, cx| view.set_remote_connection(None, None, cx));
                        chat.git.update(cx, |view, cx| view.bind(None, None, cx));
                        chat.files.update(cx, |view, cx| view.bind(None, None, cx));
                    }
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
                self.show_pairing = false;
                self.session = Some(session.clone());
                self.listing_revision += 1;
                let revision = self.listing_revision;
                self.terminal_driver = Some(terminal_driver::TerminalDriver::start(session.clone(),
                    self.terminal_tabs.iter().map(|(id, tab)| (id.clone(), tab.control.clone())).collect(),
                    self.tx.clone(), self.epoch, revision));
                self.chat_driver = Some(chat_driver::ChatDriver::start(session.clone(), self.subscriptions.clone(),
                    self.chats.iter().map(|(id, chat)| (id.clone(), chat.generation)).collect(), self.tx.clone(), self.epoch, revision));
                for (id, chat) in &self.chats {
                    let refresh = self.chat_refresh(id);
                    chat.git.update(cx, |view, cx| view.bind(Some(session.clone()), None, cx));
                    chat.files.update(cx, |view, cx| view.bind(Some(session.clone()), None, cx));
                    chat.view.update(cx, |view, cx| { view.set_remote_connection(Some(session.clone()), None, cx); view.set_remote_refresh(refresh); });
                }
                if let Some(task) = self.listings.take() { task.abort(); }
                self.resource_states = std::array::from_fn(|_| resources::LoadState::Loading);
                self.start_listings(session, revision);
            }
            Update::Access(revision, read_only, can_create) if revision == self.listing_revision => {
                self.access = Some((read_only, can_create));
                // The ticket carries no tier, so the entry saved at pairing time
                // guessed `false` — reconcile the book with what the host reports.
                if let Some(selected) = &mut self.selected && selected.read_only != read_only {
                    selected.read_only = read_only;
                    let name = selected.name.clone();
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
                for tab in self.terminal_tabs.values() {
                    tab.control.set_writable(!read_only);
                    tab.view.update(cx, |_, cx| cx.notify());
                }
                for chat in self.chats.values() {
                    chat.view.update(cx, |view, cx| view.set_remote_access(read_only, cx));
                    chat.git.update(cx, |view, cx| view.set_access(read_only, cx));
                    chat.files.update(cx, |view, cx| view.set_access(read_only, cx));
                }
            }
            Update::Chat(revision, id, generation, result) if revision == self.listing_revision => {
                if let Some(chat) = self.chats.get(&id) && chat.generation == generation {
                    chat.view.update(cx, |view, cx| match *result {
                        Ok((seq, thread, supports_steer, choices)) => view.update_remote_thread(seq, thread, supports_steer, choices, cx),
                        Err(error) => view.remote_error(error, cx),
                    });
                }
            }
            Update::Chat(..) => return,
            Update::Terminals(revision, result) if revision == self.listing_revision => match result {
                Ok(terminals) => { self.terminals = terminals; self.resource_states[2] = resources::LoadState::Ready; }
                Err(error) => self.resource_states[2] = resources::LoadState::Failed(error),
            },
            Update::Terminals(..) => return,
            Update::Projects(revision, projects) if revision == self.listing_revision => { self.projects = projects; self.resource_states[0] = resources::LoadState::Ready; },
            Update::Sessions(revision, mut sessions) if revision == self.listing_revision => {
                for session in &mut sessions {
                    // Older hosts use the full ID as a title until the first prompt.
                    if let Some(prefix) = session.title.strip_suffix(&session.session_id)
                        .filter(|prefix| prefix.is_empty() || prefix.ends_with(" · ")) {
                        session.title = format!("{prefix}New session · {}", session.session_id.chars().take(8).collect::<String>());
                    } else if session.title.trim().is_empty() {
                        session.title = format!("New session · {}", session.session_id.chars().take(8).collect::<String>());
                    }
                    if let Some(chat) = self.chats.get_mut(&session.session_id) {
                        chat.title = session.title.clone();
                        chat.git.update(cx, |view, cx| view.set_title(session.title.clone(), cx));
                    }
                }
                self.sessions = sessions;
                self.resource_states[1] = resources::LoadState::Ready;
            },
            Update::ListingError(revision, error) if revision == self.listing_revision => self.error = Some(error),
            Update::ResourceError(revision, resource, error) if revision == self.listing_revision => {
                self.resource_states[resource as usize] = resources::LoadState::Failed(error);
            }
            Update::ResourceError(..) | Update::Access(..) | Update::Projects(..) | Update::Sessions(..) | Update::ListingError(..) => return,
            Update::Created(revision, result) if revision == self.listing_revision => {
                self.creation_task = None;
                self.creating = false;
                if let Err(error) = result { self.error = Some(error); }
            }
            Update::Created(..) => return,
        }
        cx.notify();
    }

    /// Park every dirty host-file editor under its (host, session) key before
    /// the teardown drops the tabs that own them. Navigation — reconnect,
    /// pairing, Back-to-local — must not silently destroy unsaved drafts.
    fn stash_drafts(&mut self, cx: &mut Context<Self>) {
        let Some(endpoint) = self.selected.as_ref().map(|host| host.endpoint_id.clone()) else { return; };
        for (id, chat) in &self.chats {
            if chat.files.read(cx).dirty_buffers(cx) == 0 { continue; }
            self.drafts.insert((endpoint.clone(), id.clone()), chat.files.clone());
        }
    }

    /// `WorkspaceRoot` calls this right before the entity drops (Back to
    /// local): the drafts outlive this workspace and re-enter through
    /// `restore_drafts` on the next mount.
    pub(crate) fn take_drafts(&mut self, cx: &mut Context<Self>) -> DraftFiles {
        self.stash_drafts(cx);
        std::mem::take(&mut self.drafts)
    }

    pub(crate) fn restore_drafts(&mut self, drafts: DraftFiles) { self.drafts.extend(drafts); }

    fn disconnect(&mut self) {
        self.error = None;
        self.pending_restore = None;
        self.pending_close = None;
        self.close_task = None;
        self.epoch += 1;
        self.listing_revision += 1;
        self.connection = None;
        self.chat_driver = None;
        self.terminal_driver = None;
        self.bind_terminals();
        self.terminal_tabs.clear();
        self.terminals.clear();
        self.active_terminal = None;
        self.chats.clear();
        self.active_chat = None;
        self.show_git = false;
        self.show_files = false;
        self.navigator_open = false;
        self.subscriptions = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        if let Some(task) = self.listings.take() { task.abort(); }
        if let Some(task) = self.refresh_task.take() { task.abort(); }
        if let Some(task) = self.creation_task.take() { task.abort(); }
        self.session = None;
        self.access = None;
        self.state = ConnState::Disconnected;
        self.creating = false;
    }

    fn connect(&mut self, host: HostEntry, ticket: Option<PairingTicket>, cx: &mut Context<Self>) {
        self.stash_drafts(cx);
        self.disconnect();
        self.error = None;
        self.chats.clear();
        self.active_chat = None;
        self.subscriptions = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        self.projects.clear();
        self.sessions.clear();
        self.selected = Some(host.clone());
        self.show_pairing = false;
        self.state = ConnState::Connecting;
        self.connection = Some(connection::start(host, ticket, self.epoch, self.tx.clone()));
        cx.notify();
    }

    fn pair(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_string();
        let parsed = PairingTicket::parse(self.ticket.read(cx).unmask_value().as_ref());
        if name.is_empty() { self.error = Some("Enter a host name.".into()); cx.notify(); return; }
        match parsed {
            Ok(ticket) => {
                // Drop the bearer credential from the input immediately after submission.
                self.ticket.update(cx, |input, cx| input.set_value("", window, cx));
                let host = HostEntry { name, endpoint_id: endpoint_id_hex(&ticket.endpoint_id),
                    enrollment: None, read_only: false, protocol_version: None };
                self.connect(host, Some(ticket), cx);
                self.focus_active(window, cx);
            }
            Err(_) => { self.error = Some("Invalid pairing ticket. Paste the full connection URL or mint a fresh ticket on the host with `oximux pair-new`.".into()); cx.notify(); }
        }
    }

    fn show_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stash_drafts(cx);
        self.disconnect();
        self.show_pairing = true;
        self.show_ticket = false;
        self.name.update(cx, |input, cx| input.set_value("", window, cx));
        self.ticket.update(cx, |input, cx| { input.set_value("", window, cx); input.set_masked(true, window, cx); });
        self.name.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn cancel_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_pairing = false;
        self.error = None;
        self.ticket.update(cx, |input, cx| input.set_value("", window, cx));
        self.focus_active(window, cx);
        cx.notify();
    }

    fn chat_refresh(&self, id: &str) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let tx = self.chat_driver.as_ref()?.tx.clone();
        let id = id.to_string();
        Some(Arc::new(move || { let _ = tx.send(chat_driver::Command::Refresh(id.clone())); }))
    }

    fn open_chat(&mut self, id: String, title: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.chats.contains_key(&id) {
            self.next_chat_generation += 1;
            let generation = self.next_chat_generation;
            let session = self.session.clone();
            let read_only = self.access.map(|(read_only, _)| read_only);
            let refresh = self.chat_refresh(&id);
            let view = cx.new(|cx| {
                let mut view = AgentChatView::new_remote(id.clone(), self.theme, self.density, self.typography.clone(), window, cx);
                view.set_remote_connection(session.clone(), read_only, cx);
                view.set_remote_refresh(refresh);
                view
            });
            let git = cx.new(|cx| {
                let mut git = git_view::RemoteGitView::new(Root::Session(id.clone()), title.clone(), self.theme, self.density, self.typography.clone(), window, cx);
                git.bind(session.clone(), read_only, cx);
                git
            });
            // A parked editor for this same (host, session) returns with its
            // drafts intact; `bind` re-points it at the live session.
            let key = self.selected.as_ref().map(|host| (host.endpoint_id.clone(), id.clone()));
            let files = key.and_then(|key| self.drafts.remove(&key)).inspect(|files| {
                files.update(cx, |files, cx| files.bind(session.clone(), read_only, cx));
            }).unwrap_or_else(|| cx.new(|cx| {
                let mut files = files_view::RemoteFilesView::new(Root::Session(id.clone()), self.theme, self.density, self.typography.clone());
                files.bind(session, read_only, cx);
                files
            }));
            self.chats.insert(id.clone(), ChatTab { view, git, files, generation, title });
            if let Some(driver) = &self.chat_driver { let _ = driver.tx.send(chat_driver::Command::Open(id.clone(), generation)); }
        }
        self.activate_chat(id, window, cx);
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.active_terminal.as_ref().and_then(|id| self.terminal_tabs.get(id))
            .map(|tab| tab.view.read(cx).focus_handle(cx))
            .or_else(|| self.active_chat.as_ref().filter(|_| !self.show_git && !self.show_files).and_then(|id| self.chats.get(id))
                .map(|chat| chat.view.read(cx).focus_handle(cx)))
            .unwrap_or_else(|| self.focus.clone());
        focus.focus(window, cx);
    }

    fn activate_chat(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.active_chat = Some(id);
        self.active_terminal = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn close_chat(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        // Closing a session that still owns unsaved host-file drafts needs
        // the same save/discard/cancel the files panel applies on reload —
        // including buffers that are not on screen right now.
        if let Some(chat) = self.chats.get(id)
            && self.pending_close.as_deref() != Some(id)
            && chat.files.read(cx).dirty_buffers(cx) != 0 {
            self.pending_close = Some(id.to_string());
            cx.notify();
            return;
        }
        self.force_close_chat(id, Some(window), cx);
    }

    /// `window` is `None` when the close completes inside a `cx.spawn`
    /// continuation (no `Window` there) — focus then stays where the confirm
    /// strip already had it.
    fn force_close_chat(&mut self, id: &str, window: Option<&mut Window>, cx: &mut Context<Self>) {
        self.pending_close = None;
        self.chats.remove(id);
        if let Some(driver) = &self.chat_driver { let _ = driver.tx.send(chat_driver::Command::Close(id.into())); }
        if self.active_chat.as_deref() == Some(id) {
            self.active_chat = self.chats.keys().next().cloned();
            if let Some(window) = window { self.focus_active(window, cx); }
        }
        cx.notify();
    }

    fn discard_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.pending_close.take() { self.force_close_chat(&id, Some(window), cx); }
    }

    fn save_all_close(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.pending_close.clone() else { return; };
        let Some(chat) = self.chats.get(&id) else { self.pending_close = None; return; };
        let saves = chat.files.read(cx).dirty_saves(cx);
        let root = chat.files.read(cx).root();
        let Some(session) = self.session.clone() else { self.pending_close = None; return; };
        self.pending_close = None;
        // Sequential saves: the user confirmed once, so every dirty buffer
        // — active or not — must reach the host before the tab drops.
        self.close_task = Some(cx.spawn(async move |view, cx| {
            let mut result = Ok(());
            for operation in saves {
                if let Err(error) = files_rpc::execute(&session, &root, operation).await {
                    result = Err((id.clone(), error));
                    break;
                }
            }
            let _ = view.update(cx, |view, cx| view.finish_save_all(&id, result, cx));
        }));
    }

    fn finish_save_all(&mut self, id: &str, result: Result<(), (String, String)>, cx: &mut Context<Self>) {
        match result {
            Ok(()) => self.force_close_chat(id, None, cx),
            Err((failed, error)) => {
                // A failed save keeps the tab open; the error lands where the
                // user's draft is still sitting.
                if let Some(chat) = self.chats.get(&failed) {
                    chat.files.update(cx, |files, _| files.set_notice(Some(error)));
                }
            }
        }
        cx.notify();
    }



    fn create_session(&mut self, path: String, cx: &mut Context<Self>) {
        if self.creating || !self.access.is_some_and(|(_, can_create)| can_create) { return; }
        let Some(session) = self.session.clone() else { return; };
        self.creating = true;
        self.error = None;
        let tx = self.tx.clone();
        let epoch = self.epoch;
        let revision = self.listing_revision;
        self.creation_task = Some(tokio::spawn(async move {
            // The path is opaque host data: only the server touches its filesystem.
            let result = session.create_session(&path, None).await.map_err(|error| error.to_string());
            let _ = tx.send((epoch, Update::Created(revision, result)));
        }));
        cx.notify();
    }
}

impl Drop for RemoteWorkspace {
    fn drop(&mut self) { self.disconnect(); }
}

impl Focusable for RemoteWorkspace {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle { self.focus.clone() }
}


#[cfg(test)]
mod tests;
