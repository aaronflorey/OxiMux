//! Sessions that outlived their daemon.
//!
//! Every session runs in its own session group (`setsid`), so when a daemon
//! stops without ending them first — a SIGTERM'd previous-protocol daemon, a
//! SIGKILL'd wedged one — all its children get is the kernel's hangup. One
//! that ignores SIGHUP keeps running, and the app, restoring from the
//! checkpoint, would start a second copy of it. This finds those survivors
//! from the daemon's checkpoint metadata and ends them.
//!
//! A pid from a checkpoint is only a claim, so a process is signalled only
//! when all of these hold: it leads its own session (as every session child
//! does), it started when the checkpoint says its session started, and its
//! parent is not a live daemon (whose sessions are not survivors).

use std::path::Path;
#[cfg(unix)]
use std::time::Duration;

/// Grace between SIGTERM and SIGKILL.
#[cfg(unix)]
const SURVIVOR_GRACE: Duration = Duration::from_secs(2);

/// The checkpoint fields this reads. The daemon writes more.
#[cfg(unix)]
#[derive(serde::Deserialize)]
struct Meta {
    started_at_epoch_secs: u64,
    #[serde(default)]
    ended_at_epoch_secs: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
}

/// End every session in `checkpoints_dir` whose daemon is gone but whose
/// process still runs. Returns how many were signalled. A no-op on Windows,
/// where the daemon's job objects end each session's tree with it.
pub async fn sweep_session_survivors(checkpoints_dir: &Path) -> usize {
    #[cfg(unix)]
    {
        let survivors = find_survivors(checkpoints_dir);
        if survivors.is_empty() {
            return 0;
        }
        tracing::warn!(count = survivors.len(), "ending sessions that outlived their daemon");
        for &(pid, _) in &survivors {
            signal_group(pid, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + SURVIVOR_GRACE;
        while std::time::Instant::now() < deadline
            && survivors.iter().any(|&(pid, started)| is_survivor(pid, started))
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        for &(pid, started) in &survivors {
            // Checked again: in the grace window the pid may have died and
            // been handed to something else.
            if is_survivor(pid, started) {
                signal_group(pid, libc::SIGKILL);
            }
        }
        survivors.len()
    }
    #[cfg(not(unix))]
    {
        let _ = checkpoints_dir;
        0
    }
}

#[cfg(unix)]
fn find_survivors(checkpoints_dir: &Path) -> Vec<(u32, u64)> {
    let Ok(dirs) = std::fs::read_dir(checkpoints_dir) else {
        return Vec::new();
    };
    dirs.flatten()
        .filter_map(|dir| {
            let raw = std::fs::read(dir.path().join("meta.json")).ok()?;
            let meta: Meta = serde_json::from_slice(&raw).ok()?;
            if meta.ended_at_epoch_secs.is_some() {
                return None;
            }
            let pid = meta.pid?;
            is_survivor(pid, meta.started_at_epoch_secs).then_some((pid, meta.started_at_epoch_secs))
        })
        .collect()
}

/// Whether `pid` is still the session child a checkpoint recorded, and no
/// live daemon owns it.
#[cfg(unix)]
fn is_survivor(pid: u32, started_at: u64) -> bool {
    let Ok(raw) = i32::try_from(pid) else { return false };
    if !crate::pid_alive(pid) {
        return false;
    }
    // SAFETY: getsid takes a plain pid and only reads kernel state.
    if unsafe { libc::getsid(raw) } != raw {
        return false;
    }
    let Some(started) = oximux_proc_tree::start_time_of_pid(pid) else {
        return false;
    };
    // The checkpoint is seeded right after the spawn.
    if started.abs_diff(started_at) > 2 {
        return false;
    }
    match oximux_proc_tree::parent_of_pid(pid) {
        // Reparented away from its daemon: that daemon is gone.
        Some(parent) => !is_live_daemon(parent),
        None => false,
    }
}

#[cfg(unix)]
fn is_live_daemon(pid: u32) -> bool {
    oximux_proc_tree::process(pid).is_some_and(|p| p.name.starts_with("oximux-relay"))
}

#[cfg(unix)]
fn signal_group(pid: u32, sig: libc::c_int) {
    let Ok(pid) = i32::try_from(pid) else { return };
    // SAFETY: kill(2) with a negative pid signals that process group; the
    // leader was verified just before.
    unsafe {
        libc::kill(-pid, sig);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn write_meta(dir: &Path, name: &str, pid: u32, started_at: u64) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("meta.json"),
            format!(
                r#"{{"cwd":"/","cols":80,"rows":24,"started_at_epoch_secs":{started_at},"ended_at_epoch_secs":null,"pid":{pid}}}"#
            ),
        )
        .unwrap();
    }

    /// A session leader in its own group that ignores HUP and TERM, the way a
    /// daemon's child does. `setsid` makes it lead its session, and its parent
    /// exits at once, so it is reparented exactly like the child of a dead
    /// daemon.
    fn orphaned_session() -> u32 {
        let out = std::process::Command::new("/usr/bin/perl")
            .args([
                "-e",
                "use POSIX; $SIG{HUP}='IGNORE'; $SIG{TERM}='IGNORE'; \
                 if (fork) { exit 0 } POSIX::setsid(); print \"$$\\n\"; close STDOUT; close STDERR; sleep 30",
            ])
            .output()
            .expect("spawn perl");
        String::from_utf8_lossy(&out.stdout).trim().parse().expect("child pid")
    }

    fn now() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    }

    #[tokio::test]
    async fn a_session_that_outlived_its_daemon_is_ended() {
        let dir = tempfile::TempDir::new().unwrap();
        let pid = orphaned_session();
        write_meta(dir.path(), "a", pid, now());

        assert_eq!(sweep_session_survivors(dir.path()).await, 1);
        assert!(crate::wait_dead(pid, Duration::from_secs(2)).await, "survivor killed");
    }

    #[tokio::test]
    async fn a_pid_whose_start_time_does_not_match_is_left_alone() {
        let dir = tempfile::TempDir::new().unwrap();
        let pid = orphaned_session();
        write_meta(dir.path(), "a", pid, now() - 3600);

        assert_eq!(sweep_session_survivors(dir.path()).await, 0);
        assert!(crate::pid_alive(pid), "a recycled pid is not signalled");
        signal_group(pid, libc::SIGKILL);
    }
}
