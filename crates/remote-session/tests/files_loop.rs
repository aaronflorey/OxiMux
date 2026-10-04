//! The actual client API crosses the host wire boundary without laptop file I/O.
use std::sync::Arc;
use oximux_agents::session_registry::{SessionMeta, SessionRegistry};
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot};
use oximux_remote_proto::{PairingTicket, testing::duplex_pair};
use oximux_remote_session::{RemoteSession, ClientSigner};

#[tokio::test]
async fn files_client_lists_reads_and_version_checks_host_saves() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.txt"), "host original").unwrap();
    let registry = Arc::new(SessionRegistry::new());
    registry.register("s".into(), Arc::new(StubConnection::default()))
        .set_meta(SessionMeta { cwd: Some(root.path().into()), ..Default::default() });
    let auth = Arc::new(AuthStore::new());
    let secret = [3; 16];
    auth.set_pairing(PairingSlot::new(secret, None, false));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(|| 1_700_000_000);
    let (transport, server) = duplex_pair();
    let client = RemoteSession::new(Arc::new(transport), ClientSigner::from_seed(&[7; 32]));
    let pump = client.take_pump().unwrap();
    let script = async move {
        // An unknown host version must fail before sending any new wire variant.
        assert!(client.read_text_file("s", "file.txt").await.is_err());
        client.pair(&PairingTicket { endpoint_id: [0; 32], handshake_secret: secret, session_id: None }, "desktop", 1_700_000_000).await.unwrap();
        let listing = client.list_directory("s", "", None).await.unwrap();
        assert_eq!(listing.entries[0].name, "file.txt");
        let doc = client.read_text_file("s", "file.txt").await.unwrap();
        assert_eq!(doc.text, "host original");
        let saved = client.write_text_file("s", &doc.path, "host edited", &doc.version).await.unwrap();
        assert_eq!(saved.text, "host edited");
        assert!(client.write_text_file("s", &doc.path, "stale", &doc.version).await.is_err());
        assert!(client.read_text_file("s", "../outside").await.is_err());
        drop(client);
    };
    let (_, result, ()) = futures::future::join3(dispatcher.serve(&server), pump.run(), script).await;
    result.unwrap();
    assert_eq!(std::fs::read_to_string(root.path().join("file.txt")).unwrap(), "host edited");
}

#[tokio::test]
async fn v27_host_never_receives_file_variants() {
    use oximux_remote_proto::{Transport, Request, Response, messages::HelloAckWire};
    let (transport, host) = duplex_pair();
    let client = RemoteSession::new(Arc::new(transport), ClientSigner::from_seed(&[7; 32]));
    let pump = client.take_pump().unwrap();
    let serve = async {
        assert!(matches!(Request::from_bytes(&host.recv().await.unwrap().unwrap()).unwrap(), Request::Hello(_)));
        host.send(Response::HelloAck(HelloAckWire { protocol_version: 27, min_compatible: 1 }).to_bytes().unwrap()).await.unwrap();
        assert!(matches!(Request::from_bytes(&host.recv().await.unwrap().unwrap()).unwrap(), Request::Register(_)));
        host.send(Response::Registered { session_token: "token".into() }.to_bytes().unwrap()).await.unwrap();
        assert!(host.recv().await.unwrap().is_none(), "no unsupported file RPC sent");
    };
    let script = async move {
        client.pair(&PairingTicket { endpoint_id: [0; 32], handshake_secret: [3; 16], session_id: None }, "desktop", 1_700_000_000).await.unwrap();
        assert_eq!(client.host_protocol_version(), Some(27));
        assert!(client.list_directory("s", "", None).await.is_err());
        assert!(client.read_text_file("s", "file").await.is_err());
        assert!(client.write_text_file("s", "file", "text", "v1").await.is_err());
    };
    let (_, result, ()) = futures::future::join3(serve, pump.run(), script).await;
    result.unwrap();
}
