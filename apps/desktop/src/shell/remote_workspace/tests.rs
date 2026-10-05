use super::*;
use gpui::TestAppContext;

#[gpui::test]
async fn invalid_pairing_stays_local_and_never_echoes_the_ticket(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let window = cx.add_window(|window, cx| {
        RemoteWorkspace::with_hosts(Theme::default(), Density::default(), Typography::default(), window, cx, || Ok(HostsFile::default()))
    });
    window.update(cx, |view, window, cx| {
        view.name.update(cx, |input, cx| input.set_value("server", window, cx));
        view.ticket.update(cx, |input, cx| input.set_value("SECRET-INVALID-TICKET", window, cx));
        view.pair(window, cx);
        assert!(!view.error.as_ref().unwrap().contains("SECRET-INVALID-TICKET"));
        assert!(view.host.is_none());
        assert!(view.selected.is_none());
        assert_eq!(view.conn_state(cx), ConnState::Disconnected);
    }).unwrap();
}

#[gpui::test]
fn identical_session_ids_on_different_hosts_never_share_views(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    // connect() registers the transport task; it is never polled.
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let _entered = runtime.enter();
    let window = cx.add_window(|window, cx| RemoteWorkspace::with_hosts(Theme::default(), Density::default(), Typography::default(), window, cx, || Ok(HostsFile::default())));
    window.update(cx, |view, window, cx| {
        view.connect(host("first"), None, cx);
        view.open_chat("same-id".into(), "first chat".into(), window, cx);
        let first = view.chats["same-id"].view.entity_id();
        // A snapshot stamped with a generation the host never issued lands nowhere.
        let mut stale = oximux_agents::thread::ChatThread::new();
        stale.push_user_message("from the other host");
        let mounted = view.host.clone().unwrap();
        mounted.update(cx, |host, cx| {
            host.apply(Update::Chat(host.listing_revision(), "same-id".into(), 999,
                Box::new(Ok((50, stale, false, None)))), cx);
        });
        assert!(view.chats["same-id"].view.read(cx).remote_thread().entries.is_empty());

        view.disconnect(cx);
        assert!(view.chats.is_empty());
        view.connect(host("second"), None, cx);
        view.open_chat("same-id".into(), "second chat".into(), window, cx);
        assert_ne!(view.chats["same-id"].view.entity_id(), first);
        view.close_chat("same-id", window, cx);
        assert!(view.chats.is_empty());
        assert!(view.active_chat.is_none());
    }).unwrap();
}

#[gpui::test]
fn remote_tab_changes_focus_the_visible_chat(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let window = cx.add_window(|window, cx| RemoteWorkspace::with_hosts(Theme::default(), Density::default(), Typography::default(), window, cx, || Ok(HostsFile::default())));
    window.update(cx, |view, window, cx| {
        view.open_chat("first".into(), "first".into(), window, cx);
        view.open_chat("second".into(), "second".into(), window, cx);
        view.activate_chat("first".into(), window, cx);
        assert!(view.chats["first"].view.read(cx).focus_handle(cx).is_focused(window));
        view.close_chat("first", window, cx);
        assert!(view.chats["second"].view.read(cx).focus_handle(cx).is_focused(window));
        view.close_chat("second", window, cx);
        assert!(view.focus.is_focused(window));
    }).unwrap();
}

#[gpui::test]
fn remote_first_use_connect_opens_pairing_at_narrow_and_zoomed_sizes(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let window = cx.add_window(|window, cx| RemoteWorkspace::with_hosts(Theme::default(), Density::default(), Typography::default(), window, cx, || Ok(HostsFile::default())));
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    for (width, height, zoom) in [(1548.0, 900.0, 100), (720.0, 480.0, 100), (720.0, 480.0, 160)] {
        cx.update(|cx| cx.set_global(oximux_settings::Appearance {
            scale: oximux_settings::UiScale::from_percent(zoom), ..Default::default()
        }));
        visual.simulate_resize(gpui::size(px(width), px(height)));
        cx.refresh().unwrap();
        let bounds = visual.debug_bounds("remote-connect").expect("first use has a Connect action");
        assert!(bounds.origin.x >= px(0.0) && bounds.right() <= px(width), "Connect must fit the window");
        assert!(bounds.origin.y >= px(0.0) && bounds.bottom() <= px(height), "Connect must stay visible");
        assert!(visual.debug_bounds("remote-disconnect").is_none(), "no disconnect before connecting");
    }
    let bounds = visual.debug_bounds("remote-connect").unwrap();
    visual.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.refresh().unwrap();
    window.update(cx, |view, _, _| assert!(view.show_pairing)).unwrap();
    let pairing = visual.debug_bounds("remote-pair-submit").expect("pairing has a submit action");
    assert!(pairing.bottom() <= px(480.0), "pairing submit stays reachable at 160% zoom");
}

#[gpui::test]
fn remote_chat_keeps_shortcuts_after_the_navigator_closes(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        cx.bind_keys([gpui::KeyBinding::new("cmd-p", crate::actions::OpenQuickOpen, None)]);
    });
    let window = cx.add_window(|window, cx| RemoteWorkspace::with_hosts(Theme::default(), Density::default(), Typography::default(), window, cx, || Ok(HostsFile::default())));
    let (transport, _host) = oximux_remote_proto::testing::duplex_pair();
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let _entered = runtime.enter();
    window.update(cx, |view, window, cx| {
        let host = cx.new(|cx| RemoteHost::new(host("server"), cx));
        view.mount_host(host.clone(), cx);
        host.update(cx, |host, cx| {
            host.apply(Update::Connected(Arc::new(RemoteSession::new(Arc::new(transport),
                oximux_remote_session::ClientSigner::from_seed(&[3; 32])))), cx);
            host.apply(Update::Sessions(host.listing_revision(), vec![SessionSummary {
                session_id: "s".into(), title: "Chat".into(), model: None, last_seq: 0, awaiting_permission: false }]), cx);
        });
        view.open_chat("s".into(), "Chat".into(), window, cx);
        cx.notify();
    }).unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    cx.refresh().unwrap();
    visual.simulate_keystrokes("cmd-p");
    window.update(cx, |view, _, _| assert!(view.navigator_open, "Cmd+P must bubble through the mounted chat")).unwrap();
    cx.refresh().unwrap();
    let result = visual.debug_bounds("remote-search-chat-0").expect("navigator must offer the session");
    visual.simulate_click(result.center(), gpui::Modifiers::none());
    window.update(cx, |view, _, _| assert!(!view.navigator_open, "selecting a session closes the navigator")).unwrap();
    cx.refresh().unwrap();
    visual.simulate_keystrokes("cmd-p");
    window.update(cx, |view, _, _| assert!(view.navigator_open, "closing navigation must preserve Cmd+P dispatch")).unwrap();
}

#[gpui::test]
fn remote_pairing_clears_old_fields_and_restores_focus(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let window = cx.add_window(|window, cx| RemoteWorkspace::with_hosts(Theme::default(), Density::default(), Typography::default(), window, cx, || Ok(HostsFile::default())));
    window.update(cx, |view, window, cx| {
        view.name.update(cx, |input, cx| input.set_value("Previous host", window, cx));
        view.error = Some("Previous error".into());
        view.show_pairing(window, cx);
        assert_eq!(view.conn_state(cx), ConnState::Disconnected);
        assert!(view.name.read(cx).value().is_empty());
        assert!(view.error.is_none());
        view.error = Some("Invalid pairing ticket".into());
        view.cancel_pairing(window, cx);
        assert!(!view.show_pairing);
        assert!(view.error.is_none());
        assert!(view.focus.is_focused(window));
    }).unwrap();
}

fn host(endpoint: &str) -> HostEntry {
    HostEntry { name: endpoint.into(), endpoint_id: endpoint.into(), enrollment: None, read_only: false, protocol_version: None }
}

#[gpui::test]
fn navigation_parks_dirty_drafts_and_reopening_restores_them_per_host(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let window = cx.add_window(|window, cx| RemoteWorkspace::with_hosts(Theme::default(), Density::default(), Typography::default(), window, cx, || Ok(HostsFile::default())));
    window.update(cx, |view, window, cx| {
        view.selected = Some(host("ep-1"));
        view.open_chat("s1".into(), "chat".into(), window, cx);
        let files = view.chats["s1"].files.clone();
        files.update(cx, |files, cx| {
            files.plant_buffer("a.txt", "saved", "edited draft", window, cx);
            files.plant_buffer("b.txt", "clean", "clean", window, cx);
        });
        // The dirty buffer is NOT the one on screen — a.txt was planted first
        // so b.txt owns `active`; the count must be buffer-wide anyway.
        assert_eq!(files.read(cx).dirty_buffers(cx), 1);

        // The workspace-entity teardown path parks drafts for their host.
        let parked = view.take_drafts(cx);
        assert_eq!(parked.len(), 1);
        view.disconnect(cx);
        assert!(view.chats.is_empty());
        assert_eq!(parked.get(&("ep-1".into(), "session:s1".into())).unwrap().entity_id(), files.entity_id());

        // A different host with the same session id must not inherit them.
        view.selected = Some(host("ep-2"));
        view.open_chat("s1".into(), "other chat".into(), window, cx);
        assert_ne!(view.chats["s1"].files.entity_id(), files.entity_id());
        assert_eq!(view.chats["s1"].files.read(cx).dirty_buffers(cx), 0);
        view.stash_drafts(cx);
        view.disconnect(cx);

        // Reopening the original session on the original host returns the
        // exact editor — the draft is still there and still dirty.
        view.restore_drafts(parked);
        view.selected = Some(host("ep-1"));
        view.open_chat("s1".into(), "chat".into(), window, cx);
        assert_eq!(view.chats["s1"].files.entity_id(), files.entity_id());
        assert_eq!(view.chats["s1"].files.read(cx).dirty_buffers(cx), 1);
        let saves = view.chats["s1"].files.read(cx).dirty_saves(cx);
        assert_eq!(saves.len(), 1);
    }).unwrap();
}

#[gpui::test]
fn closing_a_session_with_dirty_drafts_asks_before_dropping_them(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let window = cx.add_window(|window, cx| RemoteWorkspace::with_hosts(Theme::default(), Density::default(), Typography::default(), window, cx, || Ok(HostsFile::default())));
    window.update(cx, |view, window, cx| {
        view.open_chat("clean".into(), "clean".into(), window, cx);
        view.chats["clean"].files.update(cx, |files, cx| files.plant_buffer("b.txt", "same", "same", window, cx));
        view.open_chat("dirty".into(), "dirty".into(), window, cx);
        view.chats["dirty"].files.update(cx, |files, cx| files.plant_buffer("a.txt", "saved", "edited", window, cx));

        view.close_chat("clean", window, cx);
        assert!(!view.chats.contains_key("clean"));
        assert!(view.pending_close.is_none(), "clean sessions close without asking");

        view.close_chat("dirty", window, cx);
        assert_eq!(view.pending_close.as_deref(), Some("dirty"));
        assert!(view.chats.contains_key("dirty"), "the tab waits for the user's choice");

        view.pending_close = None;
        view.close_chat("dirty", window, cx);
        assert_eq!(view.pending_close.as_deref(), Some("dirty"), "cancelling re-arms the check");

        view.discard_close(window, cx);
        assert!(!view.chats.contains_key("dirty"));
        assert!(view.pending_close.is_none());
    }).unwrap();
}

#[gpui::test]
fn remote_tools_fit_without_a_resize_and_session_titles_follow_updates(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let window = cx.add_window(|window, cx| RemoteWorkspace::with_hosts(Theme::default(), Density::default(), Typography::default(), window, cx, || Ok(HostsFile::default())));
    let (transport, _host) = oximux_remote_proto::testing::duplex_pair();
    let session = Arc::new(RemoteSession::new(Arc::new(transport), oximux_remote_session::ClientSigner::from_seed(&[3; 32])));
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let _entered = runtime.enter();
    window.update(cx, |view, window, cx| {
        let host = cx.new(|cx| RemoteHost::new(host("server"), cx));
        view.mount_host(host.clone(), cx);
        host.update(cx, |host, cx| host.apply(Update::Connected(session), cx));
        view.open_chat("01234567-89ab-cdef".into(), "A long session title ".repeat(20), window, cx);
        cx.notify();
    }).unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    for (width, height, zoom) in [(1548.0, 900.0, 100), (720.0, 480.0, 160)] {
        cx.update(|cx| cx.set_global(oximux_settings::Appearance {
            scale: oximux_settings::UiScale::from_percent(zoom), ..Default::default()
        }));
        visual.simulate_resize(gpui::size(px(width), px(height)));
        for (git, files) in [(false, false), (true, false), (false, true)] {
            window.update(cx, |view, _, cx| { view.show_git = git; view.show_files = files; cx.notify(); }).unwrap();
            cx.refresh().unwrap();
            for selector in ["remote-git-toggle", "remote-files-toggle"] {
                let bounds = visual.debug_bounds(selector).expect("tool must be painted on the initial frame");
                assert!(bounds.size.width > px(0.0) && bounds.size.height > px(0.0));
                assert!(bounds.right() <= px(width) && bounds.bottom() <= px(height), "tools must fit the visible window");
            }
        }
    }
    let host = window.update(cx, |view, _, _| view.host.clone().unwrap()).unwrap();
    cx.update(|cx| host.update(cx, |host, cx| {
        host.apply(Update::Sessions(host.listing_revision(), vec![SessionSummary {
            session_id: "01234567-89ab-cdef".into(), title: "01234567-89ab-cdef".into(),
            model: None, last_seq: 0, awaiting_permission: false,
        }]), cx);
    }));
    cx.run_until_parked();
    window.update(cx, |view, _, cx| {
        assert_eq!(host.read(cx).sessions()[0].title, "New session · 01234567");
        assert_eq!(view.chats["01234567-89ab-cdef"].title, host.read(cx).sessions()[0].title);
    }).unwrap();
    cx.update(|cx| host.update(cx, |host, cx| {
        host.apply(Update::Sessions(host.listing_revision(), vec![SessionSummary {
            session_id: "01234567-89ab-cdef".into(), title: "Fix the tests".into(),
            model: None, last_seq: 1, awaiting_permission: false,
        }]), cx);
    }));
    cx.run_until_parked();
    window.update(cx, |view, _, _| {
        assert_eq!(view.chats["01234567-89ab-cdef"].title, "Fix the tests");
    }).unwrap();
}
