//! Stopping a real daemon binary the way the app's restart does: the
//! supervisor's `stop_daemon` against `oximux-relay` itself, not an in-process
//! server — the fallback signals a pid, and only a real process has one.
#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use oximux_relay_client::RelayClient;
use oximux_relay_proto::{Request, Response};
use oximux_relay_supervisor::{
    RelaySupervisor, StopError, StopPath, StopTimeouts, pid_alive, wait_dead,
};
use oximux_shell_env::test_support::test_cwd;
use tempfile::TempDir;

/// A daemon started through the supervisor, SIGKILLed on drop so a failing
/// test never leaves one running.
struct Daemon {
    _dir: TempDir,
    supervisor: RelaySupervisor,
    client: RelayClient,
    pid: u32,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(pid) = self.supervisor.read_pid() {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
        let _ = kill(Pid::from_raw(self.pid as i32), Signal::SIGKILL);
    }
}

async fn start_daemon() -> Daemon {
    static BINARY: std::sync::Once = std::sync::Once::new();
    // SAFETY: set once, before any test in this binary reads it.
    BINARY.call_once(|| unsafe {
        std::env::set_var("OXIMUX_RELAY_BINARY", env!("CARGO_BIN_EXE_oximux-relay"));
    });
    // Short: a unix socket path is capped near 104 bytes.
    let dir = tempfile::Builder::new().prefix("rs").tempdir_in("/tmp").expect("tempdir");
    let supervisor = RelaySupervisor::new(dir.path().to_path_buf(), dir.path().join("logs"));
    let client = supervisor.ensure_running().await.expect("daemon up");
    let pid = supervisor.read_pid().expect("pid record");
    Daemon { _dir: dir, supervisor, client, pid }
}

async fn spawn(client: &RelayClient, script: &str) -> String {
    let spawned = client
        .request(Request::Spawn {
            cwd: test_cwd().to_string_lossy().into_owned(),
            cols: 80,
            rows: 24,
            shell: Some("/bin/sh".into()),
            args: vec!["-c".into(), script.into()],
            env: Vec::new(),
        })
        .await
        .expect("spawn");
    match spawned {
        Response::SpawnOk { pty_id, .. } => pty_id,
        other => panic!("spawn: {other:?}"),
    }
}

fn child_pid(checkpoints: &Path, pty_id: &str) -> u32 {
    let raw = std::fs::read(checkpoints.join(pty_id).join("meta.json")).expect("meta");
    let meta: serde_json::Value = serde_json::from_slice(&raw).expect("meta json");
    meta["pid"].as_u64().expect("pid") as u32
}

fn fast() -> StopTimeouts {
    StopTimeouts {
        rpc: Duration::from_secs(1),
        exit: Duration::from_secs(5),
        signal_grace: Duration::from_millis(500),
    }
}

// The normal restart: the daemon ends its sessions — even one ignoring HUP and
// TERM — keeps their checkpoints, and exits on request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_responsive_daemon_stops_on_request() {
    let d = start_daemon().await;
    let plain = spawn(&d.client, "echo plain; sleep 1000").await;
    let stubborn = spawn(&d.client, "trap '' HUP TERM; echo stubborn; while :; do sleep 1; done").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let checkpoints = d.supervisor.checkpoints_dir();
    let children = [child_pid(&checkpoints, &plain), child_pid(&checkpoints, &stubborn)];

    // Default timeouts: the daemon's own kill grace for a session ignoring
    // TERM is longer than `fast()` would wait for its answer.
    let path =
        d.supervisor.stop_daemon(&d.client, StopTimeouts::default()).await.expect("stopped");

    assert_eq!(path, StopPath::Rpc);
    assert!(!pid_alive(d.pid), "daemon gone");
    // The old connection is dead: a request on it fails now, not after the
    // client's 10s request timeout (which, from the UI thread, is a freeze).
    let asked = std::time::Instant::now();
    assert!(d.client.request(Request::ListPtys).await.is_err());
    assert!(asked.elapsed() < Duration::from_secs(1), "took {:?}", asked.elapsed());
    for pid in children {
        assert!(wait_dead(pid, Duration::from_secs(2)).await, "session child {pid} gone");
    }
    assert!(checkpoints.join(&plain).exists() && checkpoints.join(&stubborn).exists(), "kept");
    let next = d.supervisor.ensure_running().await.expect("a new daemon comes up");
    assert_ne!(d.supervisor.read_pid(), Some(d.pid));
    drop(next);
}

// A wedged daemon cannot answer: it is verified and killed, and the session it
// could no longer end is swept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wedged_daemon_is_killed_and_its_sessions_swept() {
    let d = start_daemon().await;
    let stubborn = spawn(&d.client, "trap '' HUP TERM; echo stubborn; while :; do sleep 1; done").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let child = child_pid(&d.supervisor.checkpoints_dir(), &stubborn);
    kill(Pid::from_raw(d.pid as i32), Signal::SIGSTOP).expect("stop the daemon");

    let path = d.supervisor.stop_daemon(&d.client, fast()).await.expect("stopped");

    assert_eq!(path, StopPath::Signal);
    assert!(!pid_alive(d.pid), "daemon gone");
    assert!(wait_dead(child, Duration::from_secs(4)).await, "orphaned session swept");
}

// A pid record naming some other process must never get it signalled — and
// while the real daemon still holds the endpoint, it must not be declared
// dead either, or a second daemon would start beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pid_record_naming_another_process_never_signals_it() {
    let d = start_daemon().await;
    let mut bystander = std::process::Command::new("sleep").arg("30").spawn().expect("sleep");
    let record = d.supervisor.read_pid_record().expect("record");
    let started = oximux_proc_tree::start_time_of_pid(bystander.id()).expect("start time");
    let forged = serde_json::json!({
        "pid": bystander.id(),
        "version": record.version,
        "started_at_epoch_secs": started,
        "exe": record.exe,
    });
    std::fs::write(d.supervisor.pid_path(), forged.to_string()).expect("forge record");
    kill(Pid::from_raw(d.pid as i32), Signal::SIGSTOP).expect("wedge the real daemon");

    let result = d.supervisor.stop_daemon(&d.client, fast()).await;

    assert_eq!(
        result,
        Err(StopError::IdentityUnknown { pid: bystander.id(), shutdown_accepted: false }),
        "the wedged daemon still answers on its endpoint"
    );
    assert!(pid_alive(bystander.id()), "the bystander was left alone");
    assert!(d.supervisor.socket_path().exists(), "the live daemon's socket is left in place");
    let _ = bystander.kill();
    let _ = bystander.wait();
}

// A daemon that already died (a crash just before the restart) is recognised
// at once, without waiting on a request nobody will read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_already_died_is_reported_dead_at_once() {
    let d = start_daemon().await;
    kill(Pid::from_raw(d.pid as i32), Signal::SIGKILL).expect("kill the daemon");
    assert!(wait_dead(d.pid, Duration::from_secs(2)).await);
    let started = std::time::Instant::now();

    let path = d.supervisor.stop_daemon(&d.client, StopTimeouts::default()).await.expect("stopped");

    assert_eq!(path, StopPath::AlreadyDead);
    assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
}
