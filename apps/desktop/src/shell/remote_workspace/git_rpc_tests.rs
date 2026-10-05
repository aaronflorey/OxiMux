use super::*;
use std::{path::Path, process::Command, sync::Arc};
use oximux_agents::{session_registry::{SessionRegistry, SessionMeta}, thread::StubConnection};
use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot};
use oximux_remote_proto::{PairingTicket, testing::duplex_pair};
use oximux_remote_session::ClientSigner;
use oximux_remote_proto::messages::{IndexStatusWire, WorktreeStatusWire};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git").args(args).current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

async fn exercise(read_only: bool) {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init"]);
    git(root.path(), &["config", "user.name", "desktop-test"]);
    git(root.path(), &["config", "user.email", "desktop@example.test"]);
    git(root.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.path().join("tracked.txt"), "before\n").unwrap();
    git(root.path(), &["add", "tracked.txt"]);
    git(root.path(), &["commit", "-m", "seed"]);
    std::fs::write(root.path().join("tracked.txt"), "after\n").unwrap();
    std::fs::write(root.path().join("new.txt"), "new content\n").unwrap();
    let registry = Arc::new(SessionRegistry::new());
    registry.register("remote-session".into(), Arc::new(StubConnection::default()))
        .set_meta(SessionMeta { cwd: Some(root.path().into()), ..Default::default() });
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new([2; 16], None, false).with_read_only(read_only));
    let dispatcher = Dispatcher::new(registry, auth).with_clock(|| 123);
    let (client, server) = duplex_pair();
    let host = tokio::spawn(async move { dispatcher.serve(&server).await; });
    let client = RemoteSession::new(Arc::new(client), ClientSigner::from_seed(&[7; 32]));
    let pump = tokio::spawn(client.take_pump().unwrap().run());
    client.pair(&PairingTicket { endpoint_id: [0; 32], handshake_secret: [2; 16], session_id: None }, "desktop", 123).await.unwrap();
    assert_eq!(client.client_access().await.unwrap().0, read_only);
    let addr = Root::Session("remote-session".into());
    let Reply::Status(status) = execute(&client, &addr, Operation::Status).await.unwrap() else { panic!("status") };
    assert!(status.files.iter().any(|f| f.path == "tracked.txt" && f.worktree == WorktreeStatusWire::Modified));
    for (path, untracked) in [("tracked.txt", false), ("new.txt", true)] {
        let Reply::Diff(files) = execute(&client, &addr, Operation::Diff { path: path.into(), staged: false, untracked }).await.unwrap() else { panic!("diff") };
        assert_eq!(files[0].path, Path::new(path));
        assert!(files[0].hunks.iter().flat_map(|h| &h.lines).any(|l| l.kind == DiffLineKind::Added));
    }
    assert!(execute(&client, &Root::Session("missing".into()), Operation::Status).await.is_err());
    assert!(execute(&client, &addr, Operation::Diff { path: "../outside.txt".into(), staged: false, untracked: true }).await.is_err());
    if read_only {
        for operation in [Operation::Stage("new.txt".into()), Operation::Unstage("tracked.txt".into()), Operation::Commit("refused".into())] {
            assert!(execute(&client, &addr, operation).await.is_err(), "host must enforce read-only access");
        }
        assert!(git(root.path(), &["diff", "--cached", "--name-only"]).is_empty());
        assert_eq!(git(root.path(), &["rev-list", "--count", "HEAD"]).trim(), "1");
    } else {
        let Reply::Mutated { sha: None, status: Ok(status) } = execute(&client, &addr, Operation::Stage("new.txt".into())).await.unwrap() else { panic!("stage") };
        assert!(status.files.iter().any(|f| f.path == "new.txt" && f.index == IndexStatusWire::Added));
        let Reply::Diff(files) = execute(&client, &addr, Operation::Diff { path: "new.txt".into(), staged: true, untracked: false }).await.unwrap() else { panic!("staged diff") };
        assert_eq!(files[0].status, DiffStatus::Added);
        let Reply::Mutated { status: Ok(status), .. } = execute(&client, &addr, Operation::Unstage("new.txt".into())).await.unwrap() else { panic!("unstage") };
        assert!(status.files.iter().any(|f| f.path == "new.txt" && f.worktree == WorktreeStatusWire::Untracked));
        execute(&client, &addr, Operation::Stage("new.txt".into())).await.unwrap();
        let Reply::Mutated { sha: Some(sha), status: Ok(status) } = execute(&client, &addr, Operation::Commit("desktop commit".into())).await.unwrap() else { panic!("commit") };
        assert_eq!(sha, git(root.path(), &["rev-parse", "HEAD"]).trim());
        assert_eq!(git(root.path(), &["log", "-1", "--format=%s"]).trim(), "desktop commit");
        assert!(!status.files.iter().any(|f| f.path == "new.txt"));
        assert!(status.files.iter().any(|f| f.path == "tracked.txt" && f.worktree == WorktreeStatusWire::Modified), "unstaged changes are not committed");
        assert!(execute(&client, &addr, Operation::Commit("empty index".into())).await.is_err());
    }
    drop(client);
    pump.await.unwrap().unwrap();
    host.await.unwrap();
}

#[tokio::test]
async fn desktop_git_uses_server_repository_and_commits_only_staged_changes() { exercise(false).await; }
#[tokio::test]
async fn desktop_git_surfaces_read_only_refusals_without_mutating() { exercise(true).await; }

/// The v29 browse surface, end to end: the repo is named by project path, the
/// registry never gains a session, and no agent is ever built. Reads ride the
/// full-scope browse gate (a read-only device still gets them); writes ride
/// the session-creation capability the same device lacks.
async fn exercise_browse(read_only: bool) {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init"]);
    git(dir.path(), &["config", "user.name", "desktop-test"]);
    git(dir.path(), &["config", "user.email", "desktop@example.test"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("tracked.txt"), "before\n").unwrap();
    git(dir.path(), &["add", "tracked.txt"]);
    git(dir.path(), &["commit", "-m", "seed"]);
    std::fs::write(dir.path().join("new.txt"), "new content\n").unwrap();
    // Deliberately empty: a browse op must not materialize a session or an agent.
    let registry = Arc::new(SessionRegistry::new());
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new([2; 16], None, false).with_read_only(read_only));
    let dispatcher = Dispatcher::new(registry.clone(), auth).with_clock(|| 123);
    let (client, server) = duplex_pair();
    let host = tokio::spawn(async move { dispatcher.serve(&server).await; });
    let client = RemoteSession::new(Arc::new(client), ClientSigner::from_seed(&[7; 32]));
    let pump = tokio::spawn(client.take_pump().unwrap().run());
    client.pair(&PairingTicket { endpoint_id: [0; 32], handshake_secret: [2; 16], session_id: None }, "desktop", 123).await.unwrap();
    let root = Root::Project(dir.path().to_str().unwrap().to_string());
    let Reply::Status(status) = execute(&client, &root, Operation::Status).await.unwrap() else { panic!("browse status") };
    assert!(status.files.iter().any(|f| f.path == "new.txt" && f.worktree == WorktreeStatusWire::Untracked));
    let Reply::Diff(files) = execute(&client, &root, Operation::Diff { path: "new.txt".into(), staged: false, untracked: true }).await.unwrap() else { panic!("browse diff") };
    assert_eq!(files[0].status, DiffStatus::Added);
    if read_only {
        for operation in [Operation::Stage("new.txt".into()), Operation::Commit("refused".into())] {
            assert!(execute(&client, &root, operation).await.is_err(), "browse writes are refused for a read-only device");
        }
        assert!(git(dir.path(), &["diff", "--cached", "--name-only"]).is_empty());
    } else {
        execute(&client, &root, Operation::Stage("new.txt".into())).await.unwrap();
        let Reply::Mutated { sha: Some(sha), status: Ok(status) } = execute(&client, &root, Operation::Commit("browse commit".into())).await.unwrap() else { panic!("browse commit") };
        assert_eq!(sha, git(dir.path(), &["rev-parse", "HEAD"]).trim());
        assert!(!status.files.iter().any(|f| f.path == "new.txt"));
    }
    assert!(registry.is_empty(), "browse must not materialize a session");
    drop(client);
    pump.await.unwrap().unwrap();
    host.await.unwrap();
}

#[tokio::test]
async fn project_browse_git_works_without_any_session() { exercise_browse(false).await; }
#[tokio::test]
async fn project_browse_git_reads_but_never_writes_for_read_only() { exercise_browse(true).await; }

#[tokio::test]
async fn confirmed_commit_stays_successful_when_status_refresh_fails() {
    use oximux_remote_proto::{Transport, proto::{Request, Response, RpcError}};
    let (transport, server) = duplex_pair();
    let host = tokio::spawn(async move {
        let request = Request::from_bytes(&server.recv().await.unwrap().unwrap()).unwrap();
        assert!(matches!(request, Request::GitCommit { .. }));
        server.send(Response::GitCommitted { sha: "confirmed-sha".into() }.to_bytes().unwrap()).await.unwrap();
        let request = Request::from_bytes(&server.recv().await.unwrap().unwrap()).unwrap();
        assert!(matches!(request, Request::GitStatus { .. }));
        server.send(Response::Error(RpcError::UnknownSession).to_bytes().unwrap()).await.unwrap();
    });
    let client = RemoteSession::new(Arc::new(transport), ClientSigner::from_seed(&[7; 32]));
    let pump = tokio::spawn(client.take_pump().unwrap().run());
    let reply = execute(&client, &Root::Session("session".into()), Operation::Commit("message".into())).await.unwrap();
    assert!(matches!(reply, Reply::Mutated { sha: Some(ref sha), status: Err(_) } if sha == "confirmed-sha"));
    drop(client);
    host.await.unwrap();
    let _ = pump.await.unwrap();
}
