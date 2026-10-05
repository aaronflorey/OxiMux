use super::*;
use gpui::{AppContext, TestAppContext};

fn entry() -> HostEntry {
    HostEntry { name: "server".into(), endpoint_id: "ep".into(), enrollment: None,
        read_only: false, protocol_version: None }
}

#[gpui::test]
async fn disconnected_host_rejects_old_connection_updates(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let host = cx.update(|cx| cx.new(|cx| RemoteHost::new(entry(), cx)));
    let stale_epoch = host.update(cx, |host, cx| {
        let epoch = host.epoch;
        host.disconnect(cx);
        epoch
    });
    host.update(cx, |host, _| {
        let stale = ProjectSummaryWire { name: "stale".into(), path: "/server/stale".into() };
        let current = ProjectSummaryWire { name: "current".into(), path: "/server/current".into() };
        host.tx.send((stale_epoch, Update::Projects(host.listing_revision, vec![stale]))).unwrap();
        host.tx.send((host.epoch, Update::Projects(host.listing_revision, vec![current]))).unwrap();
    });
    cx.run_until_parked();
    host.update(cx, |host, _| {
        assert_eq!(host.projects.len(), 1);
        assert_eq!(host.projects[0].name, "current");
        assert!(host.session.is_none());
        assert!(host.connection.is_none());
    });
}

#[gpui::test]
fn failed_remote_creation_releases_without_a_phantom_session(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let host = cx.update(|cx| cx.new(|cx| RemoteHost::new(entry(), cx)));
    host.update(cx, |host, cx| {
        host.creating = true;
        host.apply(Update::Created(host.listing_revision, Err("server refused creation".into())), cx);
        assert!(!host.creating);
        assert!(host.sessions.is_empty());
        assert_eq!(host.error.as_deref(), Some("server refused creation"));
    });
}

#[gpui::test]
fn reconnect_rejects_old_snapshots_and_rpc_results(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let host = cx.update(|cx| cx.new(|cx| RemoteHost::new(entry(), cx)));
    host.update(cx, |host, cx| {
        let old_revision = host.listing_revision;
        host.creating = true;
        host.apply(Update::State(ConnState::Connecting), cx);
        assert!(!host.creating);
        host.apply(Update::Projects(old_revision, vec![ProjectSummaryWire {
            name: "stale".into(), path: "/server/stale".into(),
        }]), cx);
        host.apply(Update::Created(old_revision, Err("stale RPC error".into())), cx);
        assert!(host.projects.is_empty());
        assert!(host.error.is_none());
    });
}

#[gpui::test]
fn access_is_authoritative_and_invalidated_on_disconnect(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let host = cx.update(|cx| cx.new(|cx| RemoteHost::new(entry(), cx)));
    host.update(cx, |host, cx| {
        assert!(host.access.is_none(), "saved hints cannot enable mutations");
        host.listing_revision = 3;
        host.apply(Update::Access(2, false, true), cx);
        assert!(host.access.is_none(), "stale connection reply ignored");
        host.apply(Update::Access(3, true, false), cx);
        assert_eq!(host.access, Some((true, false)));
        host.disconnect(cx);
        assert!(host.access.is_none());
    });
}

#[gpui::test]
fn resource_failure_recovery_and_stale_replies_are_distinct(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let host = cx.update(|cx| cx.new(|cx| RemoteHost::new(entry(), cx)));
    host.update(cx, |host, cx| {
        let revision = host.listing_revision;
        assert!(host.resource_states.iter().all(|state| matches!(state, resources::LoadState::Loading)));
        host.apply(Update::ResourceError(revision, resources::Resource::Projects, "denied".into()), cx);
        host.apply(Update::ResourceError(revision, resources::Resource::Sessions, "offline".into()), cx);
        host.apply(Update::Terminals(revision, Err("unavailable".into())), cx);
        assert!(host.resource_states.iter().all(|state| matches!(state, resources::LoadState::Failed(_))));
        host.apply(Update::Projects(revision, vec![]), cx);
        host.apply(Update::Sessions(revision, vec![]), cx);
        host.apply(Update::Terminals(revision, Ok(vec![])), cx);
        assert!(host.resource_states.iter().all(|state| matches!(state, resources::LoadState::Ready)));
        host.apply(Update::State(ConnState::Connecting), cx);
        host.apply(Update::ResourceError(revision, resources::Resource::Projects, "stale".into()), cx);
        assert!(matches!(host.resource_states[0], resources::LoadState::Ready), "old errors cannot overwrite a new connection");
    });
}

#[gpui::test]
fn connecting_state_clears_the_prior_error(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let host = cx.update(|cx| cx.new(|cx| RemoteHost::new(entry(), cx)));
    host.update(cx, |host, cx| {
        host.apply(Update::Created(host.listing_revision, Err("prior failure".into())), cx);
        assert!(host.error.is_some());
        host.apply(Update::State(ConnState::Connecting), cx);
        assert!(host.error.is_none());
    });
}
