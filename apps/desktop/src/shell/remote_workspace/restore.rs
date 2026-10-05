//! Selection metadata only. Agent state always comes from the server.
use super::*;
use serde::{Deserialize, Serialize};
use oximux_storage::SettingsRepo;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Selection {
    name: String,
    endpoint_id: String,
    #[serde(default)]
    enrollment: Option<String>,
    tabs: Vec<(String, String)>,
    active: Option<String>,
    #[serde(default)]
    terminal_tabs: Vec<(String, String)>,
    #[serde(default)]
    active_terminal: Option<String>,
}

impl Selection {
    fn host<'a>(&self, hosts: &'a HostsFile) -> Option<&'a HostEntry> {
        hosts.entries.iter().find(|host| host.endpoint_id.eq_ignore_ascii_case(&self.endpoint_id)
            && match &self.enrollment {
                Some(enrollment) => host.enrollment.as_ref() == Some(enrollment),
                None => host.name == self.name,
            })
    }
}

fn key(window_id: &str) -> String { format!("remote_workspace:{window_id}") }

pub(crate) fn load(repo: &SettingsRepo, window_id: &str) -> Option<Selection> {
    repo.get(&key(window_id)).ok().flatten()
        .filter(|raw| raw.len() <= 64 * 1024)
        .and_then(|raw| serde_json::from_str::<Option<Selection>>(&raw).ok()).flatten()
}

pub(crate) fn save(repo: &SettingsRepo, window_id: &str, view: Option<(&RemoteWorkspace, &gpui::App)>) {
    let selection = view.and_then(|(view, cx)| {
        // Preserve a pending boot restore until the host book has loaded.
        if let Some(saved) = &view.pending_restore { return Some(saved.clone()); }
        if view.conn_state(cx) == ConnState::Disconnected { return None; }
        let host = view.selected.as_ref()?;
        Some(Selection { name: host.name.clone(), endpoint_id: host.endpoint_id.clone(),
            enrollment: host.enrollment.clone(),
            tabs: view.chats.iter().map(|(id, tab)| (id.clone(), tab.title.clone())).collect(),
            active: view.active_chat.clone(),
            terminal_tabs: view.terminal_tabs.iter().filter(|(_, tab)| tab.control.is_live())
                .map(|(id, tab)| (id.clone(), tab.title.clone())).collect(),
            active_terminal: view.active_terminal.clone() })
    });
    let result = serde_json::to_string(&selection).map_err(|e| e.to_string())
        .and_then(|json| {
            if json.len() > 64 * 1024 { return Err("Remote tab metadata exceeds the settings size limit".into()); }
            repo.set(&key(window_id), &json).map_err(|e| e.to_string())
        });
    if let Err(error) = result { tracing::warn!(%error, "Could not save remote tab selection"); }
}

impl RemoteWorkspace {
    pub(crate) fn with_restore(mut self, selection: Selection) -> Self {
        self.pending_restore = Some(selection);
        self
    }

    pub(super) fn restore_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.hosts_loaded { return; }
        let Some(saved) = self.pending_restore.as_ref() else { return; };
        // Bound enrollment references survive aliases but never endpoint or
        // signing-key replacement. Legacy records require their original alias.
        let Some(host) = saved.host(&self.hosts).cloned() else {
            self.error = Some("The saved host enrollment changed or was removed. Select or pair a host to continue.".into());
            return;
        };
        // Keep unavailable metadata recoverable through autosave and restart.
        // Only consume it once the original enrollment can own these tabs.
        let saved = self.pending_restore.take().expect("validated pending restore");
        self.connect(host, None, cx);
        for (id, title) in saved.tabs { self.open_chat(id, title, window, cx); }
        if saved.active.as_ref().is_some_and(|id| self.chats.contains_key(id)) {
            self.active_chat = saved.active;
        }
        for (id, title) in saved.terminal_tabs { self.open_terminal(id, title, window, cx); }
        self.active_terminal = saved.active_terminal.filter(|id| self.terminal_tabs.contains_key(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_is_window_scoped_and_contains_no_transcript() {
        let repo = SettingsRepo::new(oximux_storage::open_memory().unwrap());
        let selected = Selection { name: "server".into(), endpoint_id: "endpoint".into(), enrollment: None,
            tabs: vec![("id".into(), "title".into())], active: Some("id".into()), terminal_tabs: Vec::new(), active_terminal: None };
        repo.set(&key("first"), &serde_json::to_string(&Some(selected)).unwrap()).unwrap();
        assert!(load(&repo, "second").is_none());
        let restored = load(&repo, "first").unwrap();
        assert_eq!(restored.tabs, vec![("id".into(), "title".into())]);
        save(&repo, "first", None);
        assert!(load(&repo, "first").is_none(), "selecting Local clears restoration");
    }

    #[test]
    fn bound_restore_survives_alias_change_but_not_key_or_endpoint_change() {
        let selection = Selection { name: "old".into(), endpoint_id: "endpoint".into(),
            enrollment: Some("binding".into()), tabs: Vec::new(), active: None,
            terminal_tabs: Vec::new(), active_terminal: None };
        let mut hosts = HostsFile::default();
        hosts.entries.push(HostEntry { name: "new".into(), endpoint_id: "endpoint".into(),
            enrollment: Some("binding".into()), read_only: false, protocol_version: None });
        assert_eq!(selection.host(&hosts).unwrap().name, "new");
        hosts.entries[0].enrollment = Some("another-binding".into());
        assert!(selection.host(&hosts).is_none());
        hosts.entries[0].enrollment = Some("binding".into());
        hosts.entries[0].endpoint_id = "replacement".into();
        assert!(selection.host(&hosts).is_none());
    }

    #[gpui::test]
    fn changed_enrollment_cannot_restore_tabs(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
        let window = cx.add_window(|window, cx| RemoteWorkspace::new(
            Theme::default(), Density::default(), Typography::default(), window, cx));
        window.update(cx, |view, window, cx| {
            view.hosts_loaded = true;
            view.hosts.entries.push(HostEntry { name: "server".into(), endpoint_id: "replacement".into(),
                enrollment: None, read_only: false, protocol_version: None });
            view.pending_restore = Some(Selection { name: "server".into(), endpoint_id: "original".into(), enrollment: None,
                tabs: vec![("same-id".into(), "saved".into())], active: Some("same-id".into()), terminal_tabs: Vec::new(), active_terminal: None });
            view.restore_tabs(window, cx);
            assert!(view.chats.is_empty());
            assert!(view.host.is_none());
            assert!(view.error.is_some());
            let repo = SettingsRepo::new(oximux_storage::open_memory().unwrap());
            save(&repo, "window", Some((view, cx)));
            let recovered = load(&repo, "window").expect("unavailable host metadata survives autosave");
            assert_eq!(recovered.endpoint_id, "original");
            assert_eq!(recovered.tabs, vec![("same-id".into(), "saved".into())]);
        }).unwrap();
    }

    #[gpui::test]
    fn failed_host_book_load_preserves_pending_restore(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
        let window = cx.add_window(|window, cx| RemoteWorkspace::new(
            Theme::default(), Density::default(), Typography::default(), window, cx));
        window.update(cx, |view, _, cx| {
            view.pending_restore = Some(Selection { name: "server".into(), endpoint_id: "original".into(),
                enrollment: None, tabs: vec![("id".into(), "saved".into())], active: Some("id".into()),
                terminal_tabs: Vec::new(), active_terminal: None });
            view.apply_book(Err("could not read host book".into()), cx);
            let repo = SettingsRepo::new(oximux_storage::open_memory().unwrap());
            save(&repo, "window", Some((view, cx)));
            assert_eq!(load(&repo, "window").unwrap().tabs, vec![("id".into(), "saved".into())]);
            assert!(view.host.is_none());
        }).unwrap();
    }

    #[gpui::test]
    fn restored_tabs_are_empty_remote_views_until_server_snapshot(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
        // The task is registered but never polled: no dial, identity IO, or server process.
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let _entered = runtime.enter();
        let window = cx.add_window(|window, cx| RemoteWorkspace::new(
            Theme::default(), Density::default(), Typography::default(), window, cx));
        window.update(cx, |view, window, cx| {
            view.hosts_loaded = true;
            view.hosts.entries.push(HostEntry { name: "server".into(), endpoint_id: "original".into(),
                enrollment: None, read_only: false, protocol_version: None });
            view.pending_restore = Some(Selection { name: "server".into(), endpoint_id: "original".into(), enrollment: None,
                tabs: vec![("same-id".into(), "saved".into())], active: Some("same-id".into()), terminal_tabs: Vec::new(), active_terminal: None });
            view.restore_tabs(window, cx);
            assert_eq!(view.active_chat.as_deref(), Some("same-id"));
            assert!(view.chats["same-id"].view.read(cx).remote_thread().entries.is_empty());
            assert!(view.host_session(cx).is_none());
            assert!(view.host_access(cx).is_none());
            view.disconnect(cx);
        }).unwrap();
    }
}
