//! The shell's remote fleet, lifted to `WorkspaceRoot`.
//!
//! `RemoteWorkspace` owned one host per window and dropped the book on every
//! navigation — which is exactly the shape the shell-parity contract rules
//! out. The coordinator owns the saved-hosts book and one `RemoteHost`
//! entity per connected endpoint so the same shell renders local work and
//! several remote hosts side by side: the left rail reads the fleet here,
//! and every pane mounting a remote surface resolves its endpoint through
//! this entity.
//!
//! Transport, authorization, and stream recovery stay on `RemoteHost`; this
//! file tracks the entities, keeps the book fresh, and forwards each host's
//! book events as one change stream for the rail.

use std::collections::HashMap;

use gpui::{AppContext, Context, Entity, EventEmitter, Subscription, Task};
use oximux_core::HostId;
use oximux_remote_proto::PairingTicket;
use oximux_remote_session::hosts_store::{HostEntry, HostsFile, parse_endpoint_id};

use super::remote_host::{RemoteHost, RemoteHostEvent};

/// Something about the fleet changed — book rows or a host's state. The rail
/// re-reads the snapshot on each one.
pub(crate) struct RemoteHostsEvent;
impl EventEmitter<RemoteHostsEvent> for RemoteHosts {}

pub(crate) struct RemoteHosts {
    book: HostsFile,
    book_loaded: bool,
    book_error: Option<String>,
    /// endpoint_id (lowercase hex) → entity. Entities persist across
    /// disconnects: a redial reuses the entity's view bindings, and dropping
    /// the entity is what finally severs the connection.
    hosts: HashMap<String, Entity<RemoteHost>>,
    subscriptions: Vec<Subscription>,
    /// The hosts.toml directory — captured once so `remove` writes back to
    /// the same file the load read, and tests can inject a tempdir instead
    /// of the real config path.
    dir: Option<std::path::PathBuf>,
    _book_load: Task<()>,
}

impl RemoteHosts {
    /// Load the book off the UI thread (it holds lock+fs I/O), then surface it.
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let dir = oximux_remote_session::hosts_store::config_dir();
        Self::with_dir(dir.ok(), cx)
    }

    /// Same boot, but reading (and later writing) a caller-chosen directory —
    /// tests pass a tempdir so the book load and `remove`'s write never touch
    /// the real `hosts.toml`.
    fn with_dir(dir: Option<std::path::PathBuf>, cx: &mut Context<Self>) -> Self {
        let load_dir = dir.clone();
        let _book_load = cx.spawn(async move |this, cx| {
            let result = match &load_dir {
                Some(dir) => {
                    let dir = dir.clone();
                    cx.background_executor()
                        .spawn(async move { HostsFile::load(&dir).map_err(|e| e.to_string()) })
                        .await
                }
                None => Err("Could not resolve the hosts directory".to_string()),
            };
            let _ = this.update(cx, |this, cx| this.apply_book(result, cx));
        });
        Self {
            book: HostsFile::default(),
            book_loaded: false,
            book_error: None,
            hosts: HashMap::new(),
            subscriptions: Vec::new(),
            dir,
            _book_load,
        }
    }

    /// Test-only ctor: preloaded book, no async load, writes confined to
    /// `dir`.
    #[cfg(test)]
    fn for_test(book: HostsFile, dir: std::path::PathBuf) -> Self {
        Self {
            book,
            book_loaded: true,
            book_error: None,
            hosts: HashMap::new(),
            subscriptions: Vec::new(),
            dir: Some(dir),
            _book_load: Task::ready(()),
        }
    }

    fn apply_book(&mut self, result: Result<HostsFile, String>, cx: &mut Context<Self>) {
        match result {
            Ok(book) => {
                self.book = book;
                self.book_error = None;
            }
            Err(error) => self.book_error = Some(error),
        }
        self.book_loaded = true;
        cx.emit(RemoteHostsEvent);
        cx.notify();
    }

    /// Saved hosts — paired or not. Rows that have never answered still show
    /// so the rail can offer a connect button.
    pub(crate) fn book(&self) -> &[HostEntry] { &self.book.entries }
    pub(crate) fn book_loaded(&self) -> bool { self.book_loaded }
    pub(crate) fn book_error(&self) -> Option<&str> { self.book_error.as_deref() }

    /// The live entity for one endpoint, when it exists.
    pub(crate) fn host(&self, endpoint_id: &str) -> Option<Entity<RemoteHost>> {
        self.hosts.get(&endpoint_id.to_lowercase()).cloned()
    }

    /// The `HostId` a pane key stores for this endpoint.
    pub(crate) fn host_id(entry: &HostEntry) -> Option<HostId> {
        parse_endpoint_id(&entry.endpoint_id).ok().map(HostId::Remote)
    }

    /// Every connected (or connecting) host, for the rail's remote section.
    pub(crate) fn connected(&self) -> impl Iterator<Item = (&String, &Entity<RemoteHost>)> {
        self.hosts.iter()
    }

    /// Bring up (or reuse) the entity for an entry and dial. `ticket` is set
    /// only on the pairing call; reconnects pass `None` like the workspace did.
    pub(crate) fn connect(
        &mut self,
        entry: HostEntry,
        ticket: Option<PairingTicket>,
        cx: &mut Context<Self>,
    ) -> Entity<RemoteHost> {
        let key = entry.endpoint_id.to_lowercase();
        // Reuse the endpoint's entity only when it carries THIS enrollment —
        // the book can hold several per endpoint, and a stored entity redials
        // the entry it was built with, so a different saved row needs a fresh
        // entity (its `Drop` severs the old dial).
        let host = match self.hosts.get(&key) {
            Some(host) if host.read(cx).entry().name == entry.name => host.clone(),
            _ => {
                let host = cx.new(|cx| RemoteHost::new(entry, cx));
                self.subscriptions.push(cx.subscribe(
                    &host,
                    |this, _host, event, cx| this.on_host_event(event, cx),
                ));
                // Host notify (conn state, projects, sessions) has no
                // RemoteHostEvent — relay it so the rail repaints.
                self.subscriptions.push(cx.observe(&host, |_this, _host, cx| {
                    cx.emit(RemoteHostsEvent);
                    cx.notify();
                }));
                self.hosts.insert(key, host.clone());
                host
            }
        };
        host.update(cx, |host, cx| host.connect(ticket, cx));
        host
    }

    /// Reconnect a saved book row: look up its entry and dial. Rows are
    /// keyed by name+endpoint — two enrollments can point at the same
    /// endpoint, and a first-match lookup would dial the wrong device's
    /// key (and tier). No-op for unknown rows — a rail row referencing a
    /// removed host can't fire it anyway (`remove` already flushed it).
    pub(crate) fn connect_saved(
        &mut self,
        name: &str,
        endpoint_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .book
            .entries
            .iter()
            .find(|e| e.endpoint_id.eq_ignore_ascii_case(endpoint_id) && e.name == name)
            .cloned()
        else {
            return;
        };
        self.connect(entry, None, cx);
    }

    /// Dial down one endpoint; the entity stays so a reconnect keeps its
    /// bindings. `remote_hosts` never drops entities on its own — the rail's
    /// forget path is the only caller that also removes the book row.
    pub(crate) fn disconnect(&mut self, endpoint_id: &str, cx: &mut Context<Self>) {
        let key = endpoint_id.to_lowercase();
        if let Some(host) = self.hosts.get(&key) {
            host.update(cx, |host, cx| host.disconnect(cx));
        }
    }

    /// Forget one saved row: drop its book entry and, when it was the
    /// enrollment the live entity dialed (or the endpoint's last row), the
    /// entity too (its `Drop` disconnects). Other enrollments on the same
    /// endpoint keep their rows and any live connection — a sibling's
    /// forget button must not sever them.
    pub(crate) fn remove(&mut self, name: &str, endpoint_id: &str, cx: &mut Context<Self>) {
        let key = endpoint_id.to_lowercase();
        let last_for_endpoint = !self
            .book
            .entries
            .iter()
            .any(|e| e.endpoint_id.eq_ignore_ascii_case(&key) && e.name != name);
        let entity_is_this_row = self
            .hosts
            .get(&key)
            .map(|h| h.read(cx).entry().name == name)
            .unwrap_or(false);
        if last_for_endpoint || entity_is_this_row {
            self.hosts.remove(&key);
        }
        let (key_owned, name_owned) = (key.clone(), name.to_string());
        let dir = self.dir.clone();
        cx.background_executor()
            .spawn(async move {
                let Some(dir) = dir else {
                    return Err("Could not resolve the hosts directory".to_string());
                };
                HostsFile::update(&dir, |hosts| {
                    hosts.entries.retain(|entry| {
                        !(entry.endpoint_id.eq_ignore_ascii_case(&key_owned)
                            && entry.name == name_owned)
                    });
                    Ok(())
                })
                .map_err(|e| e.to_string())
            })
            .detach();
        // Drop the row from the in-memory book immediately — a failed save is
        // reported by the next load, not by pretending the row still exists.
        self.book
            .entries
            .retain(|entry| !(entry.endpoint_id.eq_ignore_ascii_case(&key) && entry.name == name));
        cx.emit(RemoteHostsEvent);
        cx.notify();
    }

    /// Pair a new host and dial it. Builds the book entry the workspace used
    /// to build inline; the enrollment refresh that lands on `Connected`
    /// reconciles it on disk.
    pub(crate) fn pair(
        &mut self,
        name: String,
        ticket: PairingTicket,
        cx: &mut Context<Self>,
    ) -> Entity<RemoteHost> {
        let entry = HostEntry {
            name,
            endpoint_id: oximux_remote_session::hosts_store::endpoint_id_hex(&ticket.endpoint_id),
            enrollment: None,
            read_only: false,
            protocol_version: None,
        };
        self.connect(entry, Some(ticket), cx)
    }

    fn on_host_event(&mut self, event: &RemoteHostEvent, cx: &mut Context<Self>) {
        match event {
            RemoteHostEvent::Book(result) => {
                self.apply_book(result.clone(), cx);
            }
            RemoteHostEvent::Entry(entry) => {
                // The host refreshed its own entry (name/endpoint moved) —
                // reconcile the in-memory row so the rail reads the same
                // label the book will hold next load. Match by name too:
                // the book can hold several enrollments for one endpoint
                // and only the dialed one refreshed.
                let key = entry.endpoint_id.to_lowercase();
                for row in self.book.entries.iter_mut() {
                    if row.endpoint_id.eq_ignore_ascii_case(&key) && row.name == entry.name {
                        *row = entry.clone();
                    }
                }
                cx.emit(RemoteHostsEvent);
                cx.notify();
            }
        }
    }
}

impl Drop for RemoteHosts {
    fn drop(&mut self) {
        // Dropping the last entity handle disconnects each host; the
        // coordinator never outlives its WorkspaceRoot.
        self.hosts.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_remote_session::ConnState;

    fn entry(name: &str, endpoint: &str) -> HostEntry {
        HostEntry {
            name: name.into(),
            endpoint_id: endpoint.into(),
            enrollment: None,
            read_only: false,
            protocol_version: None,
        }
    }

    fn book(entries: Vec<HostEntry>) -> HostsFile {
        HostsFile { entries, ..HostsFile::default() }
    }

    /// A saved book row connects through `connect_saved`: the lookup is
    /// case-insensitive, the entity goes into the fleet dialing, and unknown
    /// endpoints are a no-op rather than a phantom host.
    #[gpui::test]
    fn connect_saved_dials_a_book_row(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let _entered = runtime.enter();
        cx.update(|cx| {
            let dir = std::env::temp_dir();
            let hosts =
                cx.new(|_cx| RemoteHosts::for_test(book(vec![entry("alpha", "AB12")]), dir));
            hosts.update(cx, |hosts, cx| {
                hosts.connect_saved("nope", "ab12", cx);
                assert!(hosts.hosts.is_empty(), "unknown endpoint must not spawn an entity");
                hosts.connect_saved("alpha", "ab12", cx);
                let host = hosts.host("AB12").expect("connect_saved creates the entity");
                assert!(matches!(host.read(cx).state(), ConnState::Connecting));
                assert_eq!(host.read(cx).entry().name, "alpha");
                // A second connect reuses the entity — no duplicate fleet rows.
                hosts.connect_saved("alpha", "AB12", cx);
                assert_eq!(hosts.hosts.len(), 1);
            });
        });
    }

    /// `remove` drops the live entity AND the book row, and its hosts.toml
    /// write is confined to the injected dir — never the user's real file.
    #[gpui::test]
    fn remove_drops_entity_and_book_row(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let _entered = runtime.enter();
        cx.update(|cx| {
            let dir = std::env::temp_dir();
            let hosts = cx.new(|_cx| {
                RemoteHosts::for_test(
                    book(vec![entry("alpha", "AB12"), entry("beta", "CD34")]),
                    dir,
                )
            });
            hosts.update(cx, |hosts, cx| {
                hosts.connect_saved("alpha", "AB12", cx);
                hosts.remove("alpha", "ab12", cx);
                assert!(hosts.host("AB12").is_none());
                assert_eq!(hosts.book().len(), 1);
                assert_eq!(hosts.book()[0].endpoint_id, "CD34");
            });
        });
    }

    /// Two saved enrollments may point at the same endpoint (pairing twice,
    /// or a writable + read-only device). Each row must keep its own name,
    /// dial its own enrollment, and forget only itself — the endpoint is
    /// not the row's identity.
    #[gpui::test]
    fn same_endpoint_enrollments_stay_distinct(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let _entered = runtime.enter();
        cx.update(|cx| {
            let dir = std::env::temp_dir();
            let hosts = cx.new(|_cx| {
                RemoteHosts::for_test(
                    book(vec![entry("alpha", "AB12"), entry("beta", "ab12")]),
                    dir,
                )
            });
            hosts.update(cx, |hosts, cx| {
                // Connecting alpha, then a fresh entry carrying the same
                // endpoint: the beta row must dial beta's enrollment, not
                // reuse alpha's entity.
                hosts.connect_saved("alpha", "AB12", cx);
                assert_eq!(hosts.host("ab12").unwrap().read(cx).entry().name, "alpha");
                hosts.connect_saved("beta", "AB12", cx);
                assert_eq!(hosts.host("ab12").unwrap().read(cx).entry().name, "beta");
                assert_eq!(hosts.hosts.len(), 1, "one entity per endpoint");

                // The host reporting its refreshed enrollment (the dialed
                // beta entry) must not clobber alpha's book row.
                let mut refreshed = entry("beta", "AB12");
                refreshed.read_only = true;
                hosts.on_host_event(&RemoteHostEvent::Entry(refreshed), cx);
                assert_eq!(hosts.book()[0].name, "alpha");
                assert!(!hosts.book()[0].read_only);
                assert_eq!(hosts.book()[1].name, "beta");
                assert!(hosts.book()[1].read_only);

                // Forgetting alpha keeps beta's row and its live entity.
                hosts.remove("alpha", "AB12", cx);
                assert_eq!(hosts.book().len(), 1);
                assert_eq!(hosts.book()[0].name, "beta");
                assert!(hosts.host("ab12").is_some(), "beta's connection survives");

                // Forgetting the endpoint's last row drops the entity too.
                hosts.remove("beta", "AB12", cx);
                assert!(hosts.book().is_empty());
                assert!(hosts.host("ab12").is_none());
            });
        });
    }
}
