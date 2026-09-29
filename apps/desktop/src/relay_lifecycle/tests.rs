//! The lifecycle's decisions, against a real daemon booted in-process. Only the
//! paths that decide NOT to start a daemon are driven here: starting one spawns
//! the `oximux-relay` binary through the supervisor, which a unit test cannot.
//! The success path is exercised by the live drill (phase 8).

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use oximux_relay::{ServerConfig, run_server};
use oximux_relay_client::RelayClient;
use oximux_relay_supervisor::RelaySupervisor;
use tempfile::TempDir;

use super::*;

struct Fixture {
    lifecycle: Arc<RelayLifecycle>,
    events: UnboundedReceiver<RelayLifecycleEvent>,
    _dir: TempDir,
}

async fn fixture() -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let socket = dir.path().join("relay-test.sock");
    let token_file = dir.path().join("relay-test.token");
    let token = "lifecycle-test-token";
    std::fs::write(&token_file, token).expect("write token");
    let cfg = ServerConfig::idle_disabled(socket.clone(), token_file);
    tokio::spawn(async move {
        let _ = run_server(cfg).await;
    });
    let mut client = None;
    for _ in 0..200 {
        if let Ok(c) = RelayClient::connect(&socket, token).await {
            client = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let client = Arc::new(client.expect("relay socket never came up"));
    let repo = oximux_storage::PaneRelayIdRepo::new(oximux_storage::open_memory().expect("db"));
    let supervisor = RelaySupervisor::new(dir.path().to_path_buf(), dir.path().to_path_buf());
    let lifecycle =
        RelayLifecycle::new(supervisor, repo, tokio::runtime::Handle::current(), client);
    let events = lifecycle.take_events().expect("fresh lifecycle has its events");
    Fixture { lifecycle, events, _dir: dir }
}

fn no_event(events: &mut UnboundedReceiver<RelayLifecycleEvent>) -> bool {
    events.try_recv().is_err()
}

// A second respawn for a death that was already handled finds the epoch moved
// on: it touches nothing and tells no one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respawn_for_a_replaced_session_is_a_no_op() {
    let mut f = fixture().await;
    let dead = f.lifecycle.current_session();
    // What a successful respawn leaves behind: a new current session.
    *f.lifecycle.current_session.write().unwrap() = "the-replacement".into();

    let outcome = f.lifecycle.respawn(RespawnReason::Crash, dead).await;

    assert_eq!(outcome, RespawnOutcome::AlreadyRespawned);
    assert_eq!(f.lifecycle.current_session(), "the-replacement");
    assert!(no_event(&mut f.events), "a no-op respawn emits nothing");
}

// The heartbeat stands down for a death a manual restart has claimed, and only
// for that one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_stands_down_for_an_expected_death() {
    let f = fixture().await;
    let session = f.lifecycle.current_session();
    assert!(f.lifecycle.claim_death(&session), "an unclaimed death is a crash");

    *f.lifecycle.expected_death.lock().unwrap() = Some(session.clone());
    assert!(!f.lifecycle.claim_death(&session), "a claimed death is not");
    assert!(f.lifecycle.claim_death("some-other-session"));
}

// A crash respawn the throttle refuses reports "keeps stopping" and leaves the
// epoch alone — the daemon stays down rather than looping.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_crash_respawn_reports_keeps_stopping() {
    let mut f = fixture().await;
    {
        let mut throttle = f.lifecycle.crash_throttle.lock().unwrap();
        for _ in 0..CRASH_LIMIT {
            assert!(throttle.admit(Instant::now()));
        }
    }
    let session = f.lifecycle.current_session();

    let outcome = f.lifecycle.respawn(RespawnReason::Crash, session.clone()).await;

    assert_eq!(outcome, RespawnOutcome::Failed(RespawnFailure::KeepsStopping));
    assert_eq!(f.lifecycle.current_session(), session);
    match f.events.next().await {
        Some(RelayLifecycleEvent::RespawnFailed { failure, reason }) => {
            assert_eq!(failure, RespawnFailure::KeepsStopping);
            assert_eq!(reason, RespawnReason::Crash);
        }
        other => panic!("expected RespawnFailed, got {other:?}"),
    }
}

#[test]
fn crash_throttle_refuses_the_fourth_crash_in_a_minute() {
    let mut throttle = CrashThrottle::default();
    let start = Instant::now();
    assert!(throttle.admit(start));
    assert!(throttle.admit(start + Duration::from_secs(10)));
    assert!(throttle.admit(start + Duration::from_secs(20)));
    assert!(!throttle.admit(start + Duration::from_secs(30)), "4th within 60s");
    // The first crash ages out of the window.
    assert!(throttle.admit(start + Duration::from_secs(61)));
}

#[test]
fn a_manual_restart_resets_the_crash_window() {
    let mut throttle = CrashThrottle::default();
    let now = Instant::now();
    for _ in 0..CRASH_LIMIT {
        assert!(throttle.admit(now));
    }
    assert!(!throttle.admit(now));
    throttle.reset();
    assert!(throttle.admit(now));
}

// Two restarts asked for together are one restart: one run, one result for
// both. (The fixture has no pid record, so the run fails fast at the stop —
// which is enough to count runs without starting a daemon binary.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_restarts_coalesce_into_one() {
    let mut f = fixture().await;
    // Held so the first run cannot finish before the second request arrives.
    let held = f.lifecycle.respawn_lock.lock().await;
    let first = f.lifecycle.restart();
    let second = f.lifecycle.restart();
    drop(held);

    let (a, b) = futures::join!(first, second);

    assert_eq!(a, b);
    assert_eq!(a, Err(RestartError::Stop(oximux_relay_supervisor::StopError::NoPidRecord)));
    assert_eq!(f.lifecycle.restart_runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(matches!(f.events.next().await, Some(RelayLifecycleEvent::RestartFailed { .. })));
    assert_eq!(*f.lifecycle.expected_death.lock().unwrap(), None, "the claim is released");

    // Finished restarts do not linger: the next request runs again.
    let _ = f.lifecycle.restart().await;
    assert_eq!(f.lifecycle.restart_runs.load(std::sync::atomic::Ordering::SeqCst), 2);
}

// A crash respawn that wins the lock first has already replaced the daemon the
// user meant to restart; the restart must not stop the fresh one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_that_waited_behind_a_respawn_stops_nothing() {
    let f = fixture().await;
    let held = f.lifecycle.respawn_lock.lock().await;
    let restart = f.lifecycle.restart();
    // Let the restart take its snapshot and park on the lock.
    tokio::time::sleep(Duration::from_millis(200)).await;
    *f.lifecycle.current_session.write().unwrap() = "respawned-meanwhile".into();
    drop(held);

    let outcome = restart.await.expect("nothing to fail");

    assert_eq!(outcome.stop_path, None);
    assert_eq!(outcome.new_session, "respawned-meanwhile");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_daemon_answers_the_probe() {
    let mut f = fixture().await;
    f.lifecycle.probe();
    match tokio::time::timeout(Duration::from_secs(5), f.events.next()).await {
        Ok(Some(RelayLifecycleEvent::Probed { responsive })) => assert!(responsive),
        other => panic!("expected a probe result, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_responding_is_reported_once_per_window() {
    let mut f = fixture().await;
    let session = f.lifecycle.current_session();
    f.lifecycle.report_probe(false, &session);
    f.lifecycle.report_probe(false, &session);
    assert!(matches!(f.events.next().await, Some(RelayLifecycleEvent::Probed { responsive: false })));
    assert!(no_event(&mut f.events), "the second report is debounced");
}

// An answer about a daemon that has since been replaced is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_probe_of_a_replaced_daemon_is_ignored() {
    let mut f = fixture().await;
    f.lifecycle.report_probe(false, "an-older-session");
    assert!(no_event(&mut f.events));
}
