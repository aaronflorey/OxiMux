//! Outbound remote workspace. Server paths never enter the local project registry.
//!
//! The transport/connection/stream-recovery state lives on
//! [`crate::shell::remote_host::RemoteHost`] — this view is the transitional
//! presentation shell (navigator, session rail, pairing form) while remote
//! projects move into `ProjectPanes`. Views it mounts register with the host
//! entity, which rebinds them on every reconnect.
use std::{sync::Arc, collections::HashMap};

use gpui::{App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, Subscription, Task, Window, div, px};
use gpui::prelude::FluentBuilder;
use gpui_component::{Disableable, button::{Button, ButtonVariants}, input::{Input, InputState}};
use oximux_remote_proto::{PairingTicket, SessionSummary};
use oximux_remote_session::{ConnState, RemoteSession};
use oximux_remote_session::hosts_store::{HostEntry, HostsFile, endpoint_id_hex};
use oximux_settings::{Density, Theme, Typography};

pub(crate) mod connection;
mod render;
mod navigator;
pub(crate) mod resources;
pub(crate) mod chat_driver;
pub(crate) mod terminal_driver;
mod terminals;
pub(crate) mod git_rpc;
pub(crate) mod git_view;
pub(crate) mod files_rpc;
pub(crate) mod files_view;
pub(crate) use files_view::RemoteFilesView;
pub(crate) mod restore;
use crate::shell::agent_chat::AgentChatView;
use crate::shell::remote_host::{RemoteHost, RemoteHostEvent};

struct ChatTab { view: Entity<AgentChatView>, git: Entity<git_view::RemoteGitView>, files: Entity<files_view::RemoteFilesView>, title: String, order: u64 }

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

impl Root {
    /// Stable draft-parking key: `(endpoint, key)` identifies one host
    /// file surface — a session's files or a browsed project's — so parked
    /// buffers re-enter the right view after teardown.
    pub(crate) fn key(&self) -> String {
        match self {
            Self::Session(id) => format!("session:{id}"),
            Self::Project(path) => format!("project:{path}"),
        }
    }
}

/// Host-file editors parked while their workspace is torn down — keyed by
/// (host endpoint, [`Root::key`]) so drafts from one host or one surface can
/// never appear on another's identically-named session or project. Lives on
/// `WorkspaceRoot` across a Back-to-local hop so the drafts survive the
/// workspace entity itself.
pub(crate) type DraftFiles = HashMap<(String, String), Entity<RemoteFilesView>>;

/// A folded chat state or its open/recovery failure. Boxed at the variant site:
/// `ChatThread` is large enough that an inline `Result` would blow up `Update`.
pub(crate) type ChatSnapshot = Result<(u64, oximux_agents::thread::ChatThread, bool, Option<oximux_remote_proto::proto::SessionChoices>), String>;

pub(crate) enum Update {
    Hosts(Result<HostsFile, String>),
    Enrollment(HostEntry),
    State(ConnState),
    Connected(Arc<RemoteSession>),
    Access(u64, bool, bool),
    Projects(u64, Vec<oximux_remote_proto::ProjectSummaryWire>),
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
    /// The per-endpoint transport entity; `None` while disconnected. All
    /// session/access/listing state the UI reads lives on it.
    host: Option<Entity<RemoteHost>>,
    _host_subscriptions: Vec<Subscription>,
    error: Option<String>,
    name: Entity<InputState>,
    ticket: Entity<InputState>,
    show_pairing: bool,
    show_ticket: bool,
    sidebar_open: bool,
    navigator_open: bool,
    filter: Entity<InputState>,
    _filter_subscription: gpui::Subscription,
    chats: HashMap<String, ChatTab>,
    active_chat: Option<String>,
    show_git: bool,
    show_files: bool,
    terminal_tabs: HashMap<String, terminals::TerminalTab>,
    active_terminal: Option<String>,
    /// Insertion order for the tab strip — the generation counter moved to
    /// the host with the subscription; the UI only needs a stable open order.
    next_tab_order: u64,
    /// Dirty host-file editors parked by the last teardown, waiting for their
    /// owning surface to be opened again on the same host.
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
        // The book load is the only async the workspace itself still drives —
        // every transport update flows through `RemoteHost`.
        let _updates = cx.spawn(async move |view, cx| {
            let result = cx.background_executor().spawn(async move { load() }).await;
            let _ = view.update(cx, |view, cx| view.apply_book(result, cx));
        });
        Self { theme, focus, density, typography, hosts: HostsFile::default(), hosts_loaded: false,
            pending_restore: None, selected: None, host: None, _host_subscriptions: Vec::new(),
            error: None, name, ticket, show_pairing: false, show_ticket: false,
            sidebar_open: true, navigator_open: false, filter, _filter_subscription,
            chats: HashMap::new(), active_chat: None, show_git: false, show_files: false,
            terminal_tabs: HashMap::new(), active_terminal: None,
            next_tab_order: 0,
            drafts: HashMap::new(), pending_close: None, close_task: None, _updates }
    }

    /// The book is the only async the workspace itself still drives — the
    /// host entity's channel carries every transport update.
    fn apply_book(&mut self, result: Result<HostsFile, String>, cx: &mut Context<Self>) {
        match result {
            Ok(hosts) => { self.hosts = hosts; self.hosts_loaded = true; }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    // ---- Host-state shims: render code reads through the host entity. ----

    fn conn_state(&self, cx: &App) -> ConnState {
        self.host.as_ref().map(|host| host.read(cx).state().clone()).unwrap_or(ConnState::Disconnected)
    }
    fn host_session(&self, cx: &App) -> Option<Arc<RemoteSession>> {
        self.host.as_ref().and_then(|host| host.read(cx).session())
    }
    fn host_access(&self, cx: &App) -> Option<(bool, bool)> {
        self.host.as_ref().and_then(|host| host.read(cx).access())
    }
    fn host_projects(&self, cx: &App) -> Vec<oximux_remote_proto::ProjectSummaryWire> {
        self.host.as_ref().map(|host| host.read(cx).projects().to_vec()).unwrap_or_default()
    }
    fn host_sessions(&self, cx: &App) -> Vec<SessionSummary> {
        self.host.as_ref().map(|host| host.read(cx).sessions().to_vec()).unwrap_or_default()
    }
    fn host_terminals(&self, cx: &App) -> Vec<oximux_remote_proto::messages::TerminalSummary> {
        self.host.as_ref().map(|host| host.read(cx).terminals().to_vec()).unwrap_or_default()
    }
    fn host_resource_states(&self, cx: &App) -> [resources::LoadState; 3] {
        self.host.as_ref().map(|host| host.read(cx).resource_states().clone())
            .unwrap_or_else(|| std::array::from_fn(|_| resources::LoadState::Loading))
    }
    fn host_creating(&self, cx: &App) -> bool {
        self.host.as_ref().is_some_and(|host| host.read(cx).creating())
    }
    fn host_refreshing(&self, cx: &App) -> bool {
        self.host.as_ref().is_some_and(|host| host.read(cx).refreshing())
    }
    fn host_error(&self, cx: &App) -> Option<String> {
        self.host.as_ref().and_then(|host| host.read(cx).error().map(str::to_string))
    }
    fn display_error(&self, cx: &App) -> Option<String> {
        self.error.clone().or_else(|| self.host_error(cx))
    }

    /// Park every dirty host-file editor under its (host, surface) key before
    /// the teardown drops the tabs that own them. Navigation — reconnect,
    /// pairing, Back-to-local — must not silently destroy unsaved drafts.
    fn stash_drafts(&mut self, cx: &mut Context<Self>) {
        let Some(endpoint) = self.selected.as_ref().map(|host| host.endpoint_id.clone()) else { return; };
        for (id, chat) in &self.chats {
            if chat.files.read(cx).dirty_buffers(cx) == 0 { continue; }
            self.drafts.insert((endpoint.clone(), Root::Session(id.clone()).key()), chat.files.clone());
        }
    }

    /// `WorkspaceRoot` calls this right before the entity drops (Back to
    /// local): the drafts outlive this workspace and re-enter through
    /// `restore_drafts` on the next mount. Their takeover-era caller went
    /// away with the takeover — pane-mounted remote editors reclaim them in
    /// the editor slice.
    #[allow(dead_code)]
    pub(crate) fn take_drafts(&mut self, cx: &mut Context<Self>) -> DraftFiles {
        self.stash_drafts(cx);
        std::mem::take(&mut self.drafts)
    }

    /// See `take_drafts`.
    #[allow(dead_code)]
    pub(crate) fn restore_drafts(&mut self, drafts: DraftFiles) { self.drafts.extend(drafts); }

    /// Also park dirty buffers the workspace currently shows — project-rooted
    /// views register their drafts the same way via `Root::key`.
    #[allow(dead_code)]
    pub(crate) fn stash_view_drafts(&mut self, view: Entity<RemoteFilesView>, cx: &mut Context<Self>) {
        let Some(endpoint) = self.selected.as_ref().map(|host| host.endpoint_id.clone()) else { return; };
        if view.read(cx).dirty_buffers(cx) != 0 {
            self.drafts.insert((endpoint, view.read(cx).root().key()), view);
        }
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.pending_restore = None;
        self.pending_close = None;
        self.close_task = None;
        // Dropping the entity aborts its connection job and drivers; the
        // registered views lose their sender along with it.
        self.host = None;
        self._host_subscriptions.clear();
        self.terminal_tabs.clear();
        self.active_terminal = None;
        self.chats.clear();
        self.active_chat = None;
        self.show_git = false;
        self.show_files = false;
        self.navigator_open = false;
        cx.notify();
    }

    fn mount_host(&mut self, host: Entity<RemoteHost>, cx: &mut Context<Self>) {
        let state_observer = cx.observe(&host, |view, _host, cx| {
            view.sync_titles(cx);
            cx.notify();
        });
        let events = cx.subscribe(&host, |view, _host, event, _cx| match event {
            RemoteHostEvent::Book(result) => match result {
                Ok(hosts) => { view.hosts = hosts.clone(); view.hosts_loaded = true; }
                Err(error) => view.error = Some(error.clone()),
            },
            RemoteHostEvent::Entry(entry) => {
                if view.selected.as_ref().is_some_and(|selected|
                    selected.name == entry.name && selected.endpoint_id.eq_ignore_ascii_case(&entry.endpoint_id)) {
                    view.selected = Some(entry.clone());
                }
            }
        });
        self._host_subscriptions = vec![state_observer, events];
        self.host = Some(host);
    }

    /// Chat-tab titles track the host's sessions listing (the same normalizing
    /// `New session · …` fallback the server rows get).
    fn sync_titles(&mut self, cx: &mut Context<Self>) {
        let Some(host) = &self.host else { return; };
        for (id, chat) in &mut self.chats {
            if let Some(title) = host.read(cx).chat_title(id) {
                chat.title = title.to_string();
            }
        }
    }

    fn connect(&mut self, host: HostEntry, ticket: Option<PairingTicket>, cx: &mut Context<Self>) {
        self.stash_drafts(cx);
        self.disconnect(cx);
        self.error = None;
        self.chats.clear();
        self.active_chat = None;
        self.selected = Some(host.clone());
        self.show_pairing = false;
        let entity = cx.new(|cx| RemoteHost::new(host, cx));
        entity.update(cx, |host, cx| host.connect(ticket, cx));
        self.mount_host(entity, cx);
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
        self.disconnect(cx);
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

    fn open_chat(&mut self, id: String, title: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.chats.contains_key(&id) {
            let view = cx.new(|cx| {
                AgentChatView::new_remote(id.clone(), self.theme, self.density, self.typography.clone(), window, cx)
            });
            let git = cx.new(|cx| {
                git_view::RemoteGitView::new(Root::Session(id.clone()), title.clone(), self.theme, self.density, self.typography.clone(), window, cx)
            });
            // A parked editor for this same (host, surface) returns with its
            // drafts intact; the host's bind re-points it at the live session.
            let key = self.selected.as_ref().map(|host| (host.endpoint_id.clone(), Root::Session(id.clone()).key()));
            let files = key.and_then(|key| self.drafts.remove(&key))
                .unwrap_or_else(|| cx.new(|_cx| {
                    files_view::RemoteFilesView::new(Root::Session(id.clone()), self.theme, self.density, self.typography.clone())
                }));
            if let Some(host) = &self.host {
                host.update(cx, |host, cx| {
                    host.register_chat(id.clone(), title.clone(), &view, cx);
                    host.register_git(&git, cx);
                    host.register_files(&files, cx);
                });
            }
            self.next_tab_order += 1;
            self.chats.insert(id.clone(), ChatTab { view, git, files, title, order: self.next_tab_order });
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
        if let Some(host) = &self.host {
            host.update(cx, |host, _| host.unregister_chat(id));
        }
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
        let Some(session) = self.host_session(cx) else { self.pending_close = None; return; };
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
        let Some(host) = &self.host else { return; };
        let rx = host.update(cx, |host, _| host.create_session(path));
        if let Some(rx) = rx {
            // The result also lands on the host's channel as Update::Created;
            // the receiver only keeps the spawn alive and is dropped on close.
            cx.spawn(async move |_, _| { let _ = rx.await; }).detach();
        }
        cx.notify();
    }

    fn refresh_resources(&mut self, cx: &mut Context<Self>) {
        if let Some(host) = &self.host {
            host.update(cx, |host, cx| host.refresh_resources(cx));
        }
    }
}

impl Drop for RemoteWorkspace {
    fn drop(&mut self) { self.host = None; }
}

impl Focusable for RemoteWorkspace {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle { self.focus.clone() }
}

#[cfg(test)]
mod tests;
