//! Real dispatcher coverage: session ACLs apply before any filesystem access.
use std::sync::Arc;
use oximux_agents::session_registry::{SessionMeta, SessionRegistry};
use oximux_agents::thread::StubConnection;
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot, registration_proof};
use oximux_remote_proto::{Transport, Request, Response, RpcError};
use oximux_remote_proto::messages::RegisterReq;
use oximux_remote_proto::testing::duplex_pair;

const SECRET: [u8; 16] = [0x22; 16];
const NOW: u64 = 1_700_000_000;
fn clock() -> u64 { NOW }
async fn call(client: &dyn Transport, request: Request) -> Response {
    client.send(request.to_bytes().unwrap()).await.unwrap();
    Response::from_bytes(&client.recv().await.unwrap().unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_use_host_root_and_recheck_scope_read_only_and_revocation() {
    for read_only in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("host.txt"), "host contents").unwrap();
        let registry = Arc::new(SessionRegistry::new());
        registry.register("s".into(), Arc::new(StubConnection::default()))
            .set_meta(SessionMeta { cwd: Some(root.path().into()), ..Default::default() });
        let auth = Arc::new(AuthStore::new());
        auth.set_pairing(PairingSlot::new(SECRET, Some("s".into()), false).with_read_only(read_only));
        let dispatcher = Dispatcher::new(registry, auth.clone()).with_clock(clock);
        let (client, server) = duplex_pair();
        let script = async {
            let read = || Request::ReadTextFile { session_id: "s".into(), path: "host.txt".into() };
            assert_eq!(call(&client, read()).await, Response::Error(RpcError::Unauthorized));
            let pubkey = [0x33; 32];
            let registration = RegisterReq { app_pubkey: pubkey, device_name: "desktop".into(),
                proof: registration_proof(&SECRET, &pubkey, NOW), timestamp_secs: NOW, session_id: Some("s".into()) };
            assert!(matches!(call(&client, Request::Register(registration)).await, Response::Registered { .. }));
            let Response::Directory(list) = call(&client, Request::ListDirectory {
                session_id: "s".into(), path: "".into(), after: None,
            }).await else { panic!("directory listing"); };
            assert_eq!(list.entries[0].name, "host.txt");
            let Response::TextFile(doc) = call(&client, read()).await else { panic!("host file"); };
            assert_eq!(doc.text, "host contents");
            assert_eq!(doc.path, "host.txt");
            let write = Request::WriteTextFile { session_id: "s".into(), path: doc.path.clone(),
                text: "edited".into(), version: doc.version.clone() };
            let reply = call(&client, write.clone()).await;
            if read_only { assert_eq!(reply, Response::Error(RpcError::Unauthorized)); }
            else {
                assert!(matches!(reply, Response::TextFile(_)));
                assert!(matches!(call(&client, write).await, Response::Error(RpcError::BadRequest(_))));
            }
            for request in [
                Request::ReadTextFile { session_id: "other".into(), path: "host.txt".into() },
                Request::ListDirectory { session_id: "other".into(), path: "".into(), after: None },
                Request::WriteTextFile { session_id: "other".into(), path: "host.txt".into(), text: "x".into(), version: doc.version },
            ] { assert_eq!(call(&client, request).await, Response::Error(RpcError::Unauthorized)); }
            auth.revoke(&pubkey);
            assert_eq!(call(&client, read()).await, Response::Error(RpcError::Unauthorized));
            drop(client);
        };
        tokio::join!(dispatcher.serve(&server), script);
        assert_eq!(std::fs::read_to_string(root.path().join("host.txt")).unwrap(), if read_only { "host contents" } else { "edited" });
    }
}
