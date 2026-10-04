use super::*;
use crate::auth::{AuthStore, LocalScope};
use crate::catalog::{DormantSession, DormantTranscript, DormantChoices, SessionCatalog};
use oximux_agents::session_registry::SessionRegistry;
use std::sync::Arc;

struct Catalog { root: std::path::PathBuf }
#[async_trait::async_trait]
impl SessionCatalog for Catalog {
    fn dormant(&self) -> Vec<DormantSession> {
        vec![DormantSession { session_id: "dormant".into(), cwd: Some(self.root.clone()), title: None, model: None }]
    }
    fn transcript(&self, _: &str) -> Option<DormantTranscript> { None }
    fn choices(&self, _: &str) -> Option<DormantChoices> { None }
    async fn open(&self, _: &str) -> Result<(), String> { panic!("file access must never start an agent") }
}

#[tokio::test]
async fn dormant_file_access_does_not_build_agents_and_concurrent_saves_conflict() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("file"), "original").unwrap();
    let registry = Arc::new(SessionRegistry::new());
    let dispatcher = Dispatcher::new(registry.clone(), Arc::new(AuthStore::new()))
        .with_catalog(Arc::new(Catalog { root: temp.path().into() }));
    let peer = Peer::local(LocalScope::Full);
    let Response::TextFile(doc) = dispatcher.file_request(&peer, Request::ReadTextFile {
        session_id: "dormant".into(), path: "file".into(),
    }).await else { panic!("read dormant file"); };
    let save = |text: &str| Request::WriteTextFile { session_id: "dormant".into(), path: "file".into(),
        text: text.into(), version: doc.version.clone() };
    let (a, b) = tokio::join!(dispatcher.file_request(&peer, save("a")), dispatcher.file_request(&peer, save("b")));
    assert_eq!([&a, &b].iter().filter(|response| matches!(response, Response::TextFile(_))).count(), 1);
    assert_eq!([&a, &b].iter().filter(|response| matches!(response, Response::Error(RpcError::BadRequest(_)))).count(), 1);
    assert!(registry.get("dormant").is_none());
    assert_eq!(dispatcher.file_request(&Peer::local(LocalScope::Session("other".into())), Request::ReadTextFile {
        session_id: "dormant".into(), path: "file".into(),
    }).await, Response::Error(RpcError::Unauthorized));
}
