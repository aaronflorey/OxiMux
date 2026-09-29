//! A daemon replaced mid-session is reached through the SAME `RelayTerminals`.
//!
//! The remote host and its dispatcher each cache an `Arc` to the terminal source
//! at boot and are never re-wired. So after a respawn the only thing that can
//! move them onto the new daemon is `rebind` on the shared instance — this drives
//! that with two real daemons booted in-process, standing in for "the one that
//! died" and "the one that replaced it".

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use oximux_relay::{ServerConfig, run_server};
use oximux_relay_client::RelayClient;
use oximux_relay_proto::{Request, Response};
use oximux_relay_terminals::RelayTerminals;
use oximux_remote_host::{TerminalFrame, TerminalSource};
use oximux_shell_env::test_support::{test_cwd, test_shell};
use tempfile::TempDir;

struct TestRelay {
    socket: PathBuf,
    token: String,
    _dir: TempDir,
    server: tokio::task::JoinHandle<()>,
}

async fn boot_relay() -> TestRelay {
    let dir = TempDir::new().expect("tempdir");
    let socket = dir.path().join("relay-test.sock");
    let token_file = dir.path().join("relay-test.token");
    let token = "rebind-test-token".to_string();
    std::fs::write(&token_file, &token).expect("write token");

    let cfg = ServerConfig::idle_disabled(socket.clone(), token_file);
    let server = tokio::spawn(async move {
        let _ = run_server(cfg).await;
    });
    for _ in 0..200 {
        if RelayClient::connect(&socket, &token).await.is_ok() {
            return TestRelay { socket, token, _dir: dir, server };
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("relay socket never came up");
}

async fn connect(relay: &TestRelay) -> Arc<RelayClient> {
    Arc::new(RelayClient::connect(&relay.socket, &relay.token).await.expect("connect"))
}

async fn spawn_pty(client: &RelayClient) -> String {
    let spawned = client
        .request(Request::Spawn {
            cwd: test_cwd().to_string_lossy().into_owned(),
            cols: 80,
            rows: 24,
            shell: Some(test_shell()),
            args: Vec::new(),
            env: Vec::new(),
            prefill: Vec::new(),
        })
        .await
        .expect("spawn");
    match spawned {
        Response::SpawnOk { pty_id, .. } => pty_id,
        other => panic!("expected SpawnOk, got {other:?}"),
    }
}

async fn listed(source: &dyn TerminalSource) -> Vec<String> {
    source.list().await.expect("list").into_iter().map(|t| t.pty_id).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebind_moves_a_cached_source_onto_the_new_daemon() {
    let old = boot_relay().await;
    let old_client = connect(&old).await;
    let old_pty = spawn_pty(&old_client).await;

    let terminals = Arc::new(RelayTerminals::new(Arc::clone(&old_client)));
    // What the remote dispatcher holds: a clone taken once, at boot.
    let cached: Arc<dyn TerminalSource> = terminals.clone();
    assert_eq!(listed(cached.as_ref()).await, vec![old_pty.clone()]);

    // The old daemon goes away and a new one takes over.
    old.server.abort();
    let new = boot_relay().await;
    let new_client = connect(&new).await;
    let new_pty = spawn_pty(&new_client).await;
    assert_ne!(old_pty, new_pty);

    terminals.rebind(Arc::clone(&new_client));

    assert_eq!(
        listed(cached.as_ref()).await,
        vec![new_pty.clone()],
        "the Arc cached before the respawn must be served by the new daemon",
    );
    let (attach, _frames) = cached.attach(&new_pty).await.expect("attach on the new daemon");
    cached.detach(&new_pty, attach.attachment).await;
}

/// Collect output from `frames` until it contains `needle`, or give up.
async fn saw_output(frames: &mut tokio::sync::mpsc::Receiver<TerminalFrame>, needle: &str) -> bool {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, frames.recv()).await {
        if let TerminalFrame::Output(bytes) = frame {
            seen.extend_from_slice(&bytes);
            if String::from_utf8_lossy(&seen).contains(needle) {
                return true;
            }
        }
    }
    false
}

// A new daemon numbers its attachments from 1 again, so a detach left over from
// the old daemon can carry the same attachment id as a live attachment on the
// new one. It must release only the attachment it named — never the live one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_detach_after_rebind_leaves_the_new_attachment_alive() {
    let old = boot_relay().await;
    let old_client = connect(&old).await;
    let old_pty = spawn_pty(&old_client).await;
    let terminals = Arc::new(RelayTerminals::new(Arc::clone(&old_client)));
    let (stale, _stale_frames) = terminals.attach(&old_pty).await.expect("attach on the old daemon");

    old.server.abort();
    let new = boot_relay().await;
    let new_client = connect(&new).await;
    let new_pty = spawn_pty(&new_client).await;
    terminals.rebind(Arc::clone(&new_client));
    let (live, mut frames) = terminals.attach(&new_pty).await.expect("attach on the new daemon");
    assert_eq!(stale.attachment, live.attachment, "both daemons minted the same id");

    // The phone that watched the old terminal leaves.
    terminals.detach(&old_pty, stale.attachment).await;

    terminals.input(&new_pty, b"echo still-alive\n").await.expect("write");
    assert!(
        saw_output(&mut frames, "still-alive").await,
        "the live attachment on the new daemon must keep streaming",
    );
    terminals.detach(&new_pty, live.attachment).await;
}
