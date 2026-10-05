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

/// The v29 browse surface: reads at full scope, writes through the
/// session-creation capability — and nothing reaches an agent, because no
/// session is ever looked up (the registry has none to begin with).
#[tokio::test]
async fn project_browse_reads_and_writes_at_full_scope_and_refuses_confined_peers() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("file"), "original").unwrap();
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::new(AuthStore::new()));
    let peer = Peer::local(LocalScope::Full);
    let project = temp.path().to_str().unwrap().to_string();
    let Response::TextFile(doc) = dispatcher.browse_files(&peer, project.clone(), BrowseOp::ReadTextFile {
        path: "file".into(),
    }).await else { panic!("browse read"); };
    assert_eq!(doc.text, "original");
    let Response::Directory(dir) = dispatcher.browse_files(&peer, project.clone(), BrowseOp::ListDirectory {
        path: "".into(), after: None,
    }).await else { panic!("browse list"); };
    assert!(dir.entries.iter().any(|entry| entry.name == "file"));
    let Response::TextFile(_) = dispatcher.browse_files(&peer, project.clone(), BrowseOp::WriteTextFile {
        path: "file".into(), text: "next".into(), version: doc.version.clone(),
    }).await else { panic!("browse write"); };
    assert_eq!(std::fs::read_to_string(temp.path().join("file")).unwrap(), "next");
    // An out-of-project path is contained exactly as the session surface does.
    assert!(matches!(dispatcher.browse_files(&peer, project.clone(), BrowseOp::ReadTextFile {
        path: "../sibling".into(),
    }).await, Response::Error(RpcError::BadRequest(_))));
    // A confined agent may not browse at all: the gate is full scope, and the
    // write gate is the session-creation capability it also lacks.
    let confined = Peer::local(LocalScope::Session("sess-1".into()));
    assert_eq!(dispatcher.browse_files(&confined, project.clone(), BrowseOp::ReadTextFile {
        path: "file".into(),
    }).await, Response::Error(RpcError::Unauthorized));
    assert_eq!(dispatcher.browse_files(&confined, project, BrowseOp::WriteTextFile {
        path: "file".into(), text: "x".into(), version: "v".into(),
    }).await, Response::Error(RpcError::Unauthorized));
}
