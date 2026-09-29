use oximux_relay_proto::PidRecord;
use oximux_relay_supervisor::StopError;

use super::*;
use crate::relay_lifecycle::state::Details;

fn restarted() -> RestartOutcome {
    RestartOutcome { old_pid: Some(1), new_pid: Some(2), new_session: "s".into(), stop_path: None }
}

#[test]
fn the_restart_confirm_counts_sessions_and_names_serve() {
    assert!(restart_body(Some(7), false).starts_with("7 terminal sessions will restart"));
    assert!(restart_body(Some(1), false).starts_with("1 terminal session will restart"));
    assert!(restart_body(None, false).starts_with("Every terminal session will restart"));
    assert!(!restart_body(Some(2), false).contains("oximux serve"));
    assert!(restart_body(Some(2), true).ends_with(SERVE_NOTE));
}

#[test]
fn the_kill_all_confirm_splits_what_closes_from_what_else_stops() {
    let body = kill_all_body(SessionSplit { visible: 3, other: 2 });
    assert!(body.starts_with("Ends all 5 terminal sessions: 3 in open terminal and agent CLI tabs"));
    assert!(body.contains("2 more in background projects"));
    assert!(body.contains("The daemon keeps running"));

    let only_hidden = kill_all_body(SessionSplit { visible: 0, other: 1 });
    assert!(only_hidden.starts_with("Ends all 1 terminal session: 1 in background projects"));
}

#[test]
fn a_restart_reports_its_outcome() {
    let ok = restart_notice(&Ok(restarted()), false).unwrap();
    assert_eq!((ok.kind, ok.text.as_str()), (ToastKind::Success, "Terminal daemon restarted."));
    assert!(restart_notice(&Ok(restarted()), true).unwrap().text.contains("oximux serve"));

    let unverified = restart_notice(
        &Err(RestartError::Stop(StopError::IdentityUnknown { pid: 9, shutdown_accepted: false })),
        false,
    )
    .unwrap();
    assert!(unverified.text.starts_with("Couldn't verify the daemon process"));
    assert!(unverified.offer_log);

    let lingering = restart_notice(
        &Err(RestartError::Stop(StopError::DaemonSurvived { pid: 9, shutdown_accepted: true })),
        false,
    )
    .unwrap();
    assert!(lingering.text.starts_with("The daemon stopped its sessions but did not exit"));

    let respawn = restart_notice(&Err(RestartError::Respawn(RespawnFailure::KeepsStopping)), false).unwrap();
    assert_eq!(respawn.kind, ToastKind::Error);
    assert!(respawn.text.contains("did not start (keeps stopping)"));

    assert_eq!(restart_notice(&Err(RestartError::Quitting), false), None);
}

#[test]
fn a_kill_all_reports_against_the_confirms_count() {
    let ok = kill_all_notice(5, &Ok(KillAllOutcome { before: 2, after: 0 })).unwrap();
    assert_eq!((ok.kind, ok.text.as_str()), (ToastKind::Success, "Ended 5 terminal sessions."));

    let unconfirmed = kill_all_notice(5, &Ok(KillAllOutcome { before: 5, after: 1 })).unwrap();
    assert_eq!(unconfirmed.kind, ToastKind::Warning);
    assert_eq!(unconfirmed.text, "Couldn't confirm that 1 terminal session ended.");
    assert!(unconfirmed.offer_restart && unconfirmed.offer_log);

    let replaced = kill_all_notice(5, &Err(KillAllError::Replaced)).unwrap();
    assert_eq!(replaced.kind, ToastKind::Info);
    assert_eq!(kill_all_notice(5, &Err(KillAllError::Quitting)), None);
}

#[test]
fn daemon_events_raise_alerts_but_a_manual_restart_reports_itself() {
    let keeps = event_notice(&RelayLifecycleEvent::RespawnFailed {
        reason: RespawnReason::Crash,
        failure: RespawnFailure::KeepsStopping,
    })
    .unwrap();
    assert!(keeps.text.starts_with("Terminal daemon keeps stopping"));
    assert!(keeps.alert && keeps.offer_restart && keeps.offer_log);

    let held = event_notice(&RelayLifecycleEvent::RespawnFailed {
        reason: RespawnReason::Crash,
        failure: RespawnFailure::EndpointHeld,
    })
    .unwrap();
    assert!(held.text.contains("pipe name") && !held.offer_restart);

    let unreachable = event_notice(&RelayLifecycleEvent::Probed { responsive: false }).unwrap();
    assert!(unreachable.alert && unreachable.offer_restart);

    assert_eq!(
        event_notice(&RelayLifecycleEvent::RespawnFailed {
            reason: RespawnReason::Manual,
            failure: RespawnFailure::KeepsStopping,
        }),
        None
    );
    assert_eq!(event_notice(&RelayLifecycleEvent::Probed { responsive: true }), None);
    let updated = event_notice(&RelayLifecycleEvent::PreviousDaemonRetired { foreign_serve: true }).unwrap();
    assert!(updated.text.contains("Restart oximux serve too"));
}

#[test]
fn the_status_line_reads_busy_first_then_the_daemon() {
    let mut state = RelayDaemonState::new(
        DaemonStatus::Running { pid: Some(84589), session_id: "s".into() },
        None,
    );
    assert_eq!(status_line(&state, 0), ("running · PID 84589".into(), Tone::Ok));

    state.details = Some(Details {
        sessions: Some(7),
        record: Some(PidRecord {
            pid: 84589,
            version: "0.1.32".into(),
            started_at_epoch_secs: 1_000,
            exe: String::new(),
        }),
    });
    let now = 1_000 + 3 * 86_400 + 4 * 3_600;
    assert_eq!(status_line(&state, now).0, "running · PID 84589 · up 3d 4h · 7 sessions");

    state.busy = Some(Busy::KillingAll);
    assert_eq!(status_line(&state, now), ("killing sessions…".into(), Tone::Busy));

    let in_process = RelayDaemonState::new(DaemonStatus::InProcess, None);
    assert_eq!(status_line(&in_process, 0).1, Tone::Warn);

    let silent = RelayDaemonState::new(DaemonStatus::Unreachable { reason: NOT_RESPONDING.into() }, None);
    assert_eq!(status_line(&silent, 0), ("not responding".into(), Tone::Error));
    let down = RelayDaemonState::new(DaemonStatus::Unreachable { reason: "keeps stopping".into() }, None);
    assert_eq!(status_line(&down, 0).0, "unavailable — keeps stopping");
}

// The event loop probes after an event, and a probe's answer is an event: it
// must not probe again, or it never stops.
#[test]
fn a_probes_answer_does_not_probe_again() {
    use crate::relay_lifecycle::state::reprobe_after;
    assert!(!reprobe_after(&RelayLifecycleEvent::Probed { responsive: true }));
    assert!(!reprobe_after(&RelayLifecycleEvent::Probed { responsive: false }));
    assert!(!reprobe_after(&RelayLifecycleEvent::RespawnFailed {
        reason: RespawnReason::Crash,
        failure: RespawnFailure::KeepsStopping,
    }));
    // A stop that failed may have left the daemon answering: ask.
    assert!(reprobe_after(&RelayLifecycleEvent::RestartFailed { reason: "x".into() }));
}

#[test]
fn uptime_is_two_units_at_most() {
    assert_eq!(uptime(30), "<1m");
    assert_eq!(uptime(12 * 60), "12m");
    assert_eq!(uptime(4 * 3_600 + 12 * 60), "4h 12m");
    assert_eq!(uptime(3 * 86_400 + 4 * 3_600 + 59), "3d 4h");
}

// A recovery takes down what the daemon's outage raised; nothing else is one.
#[test]
fn only_a_daemon_up_again_is_a_recovery() {
    use crate::relay_lifecycle::state::recovers;
    let down = DaemonStatus::Unreachable { reason: NOT_RESPONDING.into() };
    let up = DaemonStatus::Running { pid: Some(1), session_id: "s".into() };
    let respawned = RelayLifecycleEvent::Respawned {
        dead_session: "a".into(),
        new_session: "b".into(),
        reason: RespawnReason::Manual,
    };
    assert!(recovers(&respawned, &up));
    assert!(recovers(&RelayLifecycleEvent::Probed { responsive: true }, &down));
    // Answering while already up changes nothing.
    assert!(!recovers(&RelayLifecycleEvent::Probed { responsive: true }, &up));
    assert!(!recovers(&RelayLifecycleEvent::Probed { responsive: false }, &down));
    assert!(!recovers(&RelayLifecycleEvent::RestartFailed { reason: "x".into() }, &down));
}
