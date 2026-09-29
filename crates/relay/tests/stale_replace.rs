//! Boot's stale-daemon check against a real `oximux-relay`: a daemon from
//! another app version is replaced only when it has no sessions. The test
//! plays the newer app by passing a version the daemon did not record.
#![cfg(unix)]

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use oximux_relay_client::RelayClient;
use oximux_relay_proto::{Request, Response};
use oximux_relay_supervisor::{RelaySupervisor, pid_alive};
use oximux_shell_env::test_support::test_cwd;
use tempfile::TempDir;

const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");
const NEWER_APP: &str = "999.0.0";

/// A daemon started through the supervisor. Every daemon it ever started is
/// SIGKILLed on drop, so a failing test never leaves one running.
struct Daemon {
    _dir: TempDir,
    supervisor: RelaySupervisor,
    pids: Vec<u32>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let current = self.supervisor.read_pid();
        for pid in self.pids.iter().copied().chain(current) {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
    }
}

async fn start_daemon() -> (Daemon, RelayClient) {
    static BINARY: std::sync::Once = std::sync::Once::new();
    // SAFETY: set once, before any test in this binary reads it.
    BINARY.call_once(|| unsafe {
        std::env::set_var("OXIMUX_RELAY_BINARY", env!("CARGO_BIN_EXE_oximux-relay"));
    });
    // Short: a unix socket path is capped near 104 bytes.
    let dir = tempfile::Builder::new().prefix("rv").tempdir_in("/tmp").expect("tempdir");
    let supervisor = RelaySupervisor::new(dir.path().to_path_buf(), dir.path().join("logs"));
    let client = supervisor.ensure_running().await.expect("daemon up");
    let pid = supervisor.read_pid().expect("pid record");
    (Daemon { _dir: dir, supervisor, pids: vec![pid] }, client)
}

async fn spawn_shell(client: &RelayClient) {
    let spawned = client
        .request(Request::Spawn {
            cwd: test_cwd().to_string_lossy().into_owned(),
            cols: 80,
            rows: 24,
            shell: Some("/bin/sh".into()),
            args: vec!["-c".into(), "sleep 30".into()],
            env: Vec::new(),
        })
        .await
        .expect("spawn");
    assert!(matches!(spawned, Response::SpawnOk { .. }), "spawn: {spawned:?}");
}

#[tokio::test]
async fn an_idle_daemon_from_another_version_is_replaced() {
    let (mut daemon, client) = start_daemon().await;
    let old = daemon.pids[0];

    let boot = daemon
        .supervisor
        .replace_if_stale_and_idle(client, NEWER_APP, || false)
        .await
        .expect("a new daemon");

    let new = daemon.supervisor.read_pid().expect("new pid record");
    daemon.pids.push(new);
    assert_ne!(new, old, "a new daemon");
    assert!(!pid_alive(old), "the old one is gone");
    // The binary beside this "app" is the same old version, so the fresh
    // daemon is still flagged rather than passed off as current.
    assert_eq!(boot.stale_version.as_deref(), Some(DAEMON_VERSION));
    let stats = boot.client.request(Request::Stats).await.expect("the client answers");
    assert!(matches!(stats, Response::StatsOk(_)), "stats: {stats:?}");
}

#[tokio::test]
async fn a_daemon_with_a_session_is_kept_and_reported() {
    let (daemon, client) = start_daemon().await;
    spawn_shell(&client).await;

    let boot = daemon
        .supervisor
        .replace_if_stale_and_idle(client, NEWER_APP, || false)
        .await
        .expect("kept");

    assert_eq!(daemon.supervisor.read_pid(), Some(daemon.pids[0]), "same daemon");
    assert_eq!(boot.stale_version.as_deref(), Some(DAEMON_VERSION));
    let stats = boot.client.request(Request::Stats).await.expect("the client answers");
    assert!(matches!(stats, Response::StatsOk(_)), "stats: {stats:?}");
}

#[tokio::test]
async fn an_idle_stale_daemon_serve_uses_is_kept() {
    let (daemon, client) = start_daemon().await;

    let boot = daemon
        .supervisor
        .replace_if_stale_and_idle(client, NEWER_APP, || true)
        .await
        .expect("kept");

    assert_eq!(daemon.supervisor.read_pid(), Some(daemon.pids[0]), "same daemon");
    assert_eq!(boot.stale_version.as_deref(), Some(DAEMON_VERSION));
}

#[tokio::test]
async fn a_daemon_of_this_version_is_left_alone() {
    let (daemon, client) = start_daemon().await;

    let boot = daemon
        .supervisor
        .replace_if_stale_and_idle(client, DAEMON_VERSION, || panic!("not asked when fresh"))
        .await
        .expect("kept");

    assert_eq!(daemon.supervisor.read_pid(), Some(daemon.pids[0]), "same daemon");
    assert_eq!(boot.stale_version, None);
}
