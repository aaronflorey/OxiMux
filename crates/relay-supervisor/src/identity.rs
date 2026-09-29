//! Is the process behind a pid file really our daemon — and stopping it only
//! when it is.
//!
//! A pid file is a claim, not a fact: the daemon it names may be long gone and
//! its pid handed to an unrelated process. Signalling on the claim alone could
//! kill the user's editor. So every signal here is preceded by a fresh
//! [`verify_daemon_identity`], and only [`Identity::Match`] may be signalled.
//! Anything that cannot be read is [`Identity::Unknown`], which fails closed:
//! no signal.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::pid_alive;

/// How far the process start time may sit from the record's `started_at`.
/// The daemon records the kernel's own start time, so this only absorbs
/// whole-second rounding.
const START_TIME_TOLERANCE_SECS: u64 = 2;

/// What the daemon we are looking for must look like.
#[derive(Debug, Clone)]
pub struct Expect {
    /// The `--socket` it was started with.
    pub socket_path: PathBuf,
    /// The `--pid-file` it was started with.
    pub pid_path: PathBuf,
    /// From a v10 pid record. Its process must have started within
    /// [`START_TIME_TOLERANCE_SECS`] of this.
    pub started_at: Option<u64>,
    /// For a bare pid file with no start time: the file's mtime. The daemon
    /// wrote it, so its process must have started no later than that.
    pub pid_file_mtime: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// Verified: this pid is the daemon described.
    Match,
    /// Not the daemon: gone, or a different process now holds the pid.
    Mismatch,
    /// Could not tell. Never signal on this.
    Unknown,
}

/// Check that `pid` is the daemon `expect` describes.
///
/// Unix: its argument vector must carry `--socket <socket_path>` and
/// `--pid-file <pid_path>` — the daemon's own flags — and its start time must
/// fit `expect`. Windows cannot read another process's arguments, so the image
/// name (`oximux-relay.exe`) stands in for them there.
///
/// A process carrying our daemon's exact flags whose start time does not fit
/// is `Unknown`, not `Mismatch`: it is almost certainly a daemon of ours, and
/// calling it "not running" would lead a caller to clear its files and start a
/// second one beside it.
pub fn verify_daemon_identity(pid: u32, expect: &Expect) -> Identity {
    if !pid_alive(pid) {
        return Identity::Mismatch;
    }
    let Some(started) = oximux_proc_tree::start_time_of_pid(pid) else {
        return Identity::Unknown;
    };
    let time_fits = match (expect.started_at, expect.pid_file_mtime) {
        (Some(at), _) => started.abs_diff(at) <= START_TIME_TOLERANCE_SECS,
        // +1: the mtime and the start time round to whole seconds separately.
        (None, Some(mtime)) => started <= mtime + 1,
        (None, None) => return Identity::Unknown,
    };
    let Some(is_daemon) = looks_like_the_daemon(pid, expect) else {
        return Identity::Unknown;
    };
    match (is_daemon, time_fits) {
        (true, true) => Identity::Match,
        (true, false) => Identity::Unknown,
        (false, _) => Identity::Mismatch,
    }
}

#[cfg(not(windows))]
fn looks_like_the_daemon(pid: u32, expect: &Expect) -> Option<bool> {
    let argv = oximux_proc_tree::argv_of_pid(pid)?;
    let has = |flag: &str, value: &Path| {
        argv.windows(2).any(|w| w[0] == flag && Path::new(&w[1]) == value)
    };
    Some(has("--socket", &expect.socket_path) && has("--pid-file", &expect.pid_path))
}

#[cfg(windows)]
fn looks_like_the_daemon(pid: u32, _expect: &Expect) -> Option<bool> {
    let name = oximux_proc_tree::process(pid)?.name;
    Some(name.eq_ignore_ascii_case(&oximux_sibling_binary::sibling_file_name("oximux-relay")))
}

/// How [`stop_verified_daemon`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// It was ours and it is gone now.
    Stopped,
    /// There was nothing of ours to stop: the pid was gone or belonged to
    /// something else. Nothing was signalled.
    NotRunning,
    /// Could not verify it; nothing was signalled.
    Unknown,
    /// It was ours and it outlived the kill.
    Survived,
}

/// Stop the daemon at `pid`, verifying it before each signal: a graceful stop
/// first (SIGTERM, so it takes its final checkpoint), then — only if it is
/// still there and still verifies — a kill. On Windows both steps are the same
/// `TerminateProcess`, since there is no graceful signal to send.
pub async fn stop_verified_daemon(pid: u32, expect: &Expect, grace: Duration) -> Stopped {
    match verify_daemon_identity(pid, expect) {
        Identity::Match => {}
        Identity::Mismatch => return Stopped::NotRunning,
        Identity::Unknown => return Stopped::Unknown,
    }
    tracing::info!(pid, "stopping verified relay daemon");
    terminate(pid);
    if wait_dead(pid, grace).await {
        return Stopped::Stopped;
    }
    match verify_daemon_identity(pid, expect) {
        Identity::Match => {}
        // It exited and the pid moved on between the two checks.
        Identity::Mismatch => return Stopped::Stopped,
        Identity::Unknown => return Stopped::Unknown,
    }
    tracing::warn!(pid, "relay daemon ignored the graceful stop; killing it");
    kill(pid);
    if wait_dead(pid, Duration::from_secs(1)).await { Stopped::Stopped } else { Stopped::Survived }
}

/// Poll until `pid` is gone or `within` elapses.
pub async fn wait_dead(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if !pid_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A file's mtime in whole seconds since the epoch.
pub fn mtime_secs(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

#[cfg(unix)]
fn terminate(pid: u32) {
    signal(pid, libc::SIGTERM);
}

#[cfg(unix)]
fn kill(pid: u32) {
    signal(pid, libc::SIGKILL);
}

#[cfg(unix)]
fn signal(pid: u32, sig: libc::c_int) {
    let Ok(pid) = i32::try_from(pid) else { return };
    // SAFETY: kill(2) takes plain values; the pid was verified just before.
    unsafe {
        libc::kill(pid, sig);
    }
}

#[cfg(windows)]
fn terminate(pid: u32) {
    kill(pid);
}

#[cfg(windows)]
fn kill(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    // SAFETY: the handle is ours until CloseHandle; the pid was verified just
    // before.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return;
        }
        TerminateProcess(handle, 1);
        CloseHandle(handle);
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate(_pid: u32) {}

#[cfg(not(any(unix, windows)))]
fn kill(_pid: u32) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn now_secs() -> u64 {
        std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    /// A stand-in carrying the daemon's flags: `sh -c 'sleep …' sh --socket S
    /// --pid-file P` puts both flags in its argv exactly as the supervisor's
    /// spawn does. Identity is decided by argv + start time, so this exercises
    /// the same decision the real daemon goes through (the phase-8 drill checks
    /// the real one).
    fn fake_daemon(socket: &Path, pid_file: &Path) -> std::process::Child {
        std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30; :", "sh", "--socket"])
            .arg(socket)
            .arg("--pid-file")
            .arg(pid_file)
            .spawn()
            .expect("spawn stand-in")
    }

    fn expect(socket: &Path, pid_file: &Path, started_at: Option<u64>) -> Expect {
        Expect {
            socket_path: socket.to_path_buf(),
            pid_path: pid_file.to_path_buf(),
            started_at,
            pid_file_mtime: None,
        }
    }

    #[test]
    fn a_process_with_the_daemons_flags_and_start_time_matches() {
        let (sock, pid) = (Path::new("/tmp/x/relay-v10.sock"), Path::new("/tmp/x/relay-v10.pid"));
        let mut child = fake_daemon(sock, pid);
        let got = verify_daemon_identity(child.id(), &expect(sock, pid, Some(now_secs())));
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(got, Identity::Match);
    }

    #[test]
    fn right_flags_but_the_wrong_start_time_is_left_alone() {
        let (sock, pid) = (Path::new("/tmp/x/relay-v10.sock"), Path::new("/tmp/x/relay-v10.pid"));
        let mut child = fake_daemon(sock, pid);
        let an_hour_ago = now_secs() - 3600;
        let got = verify_daemon_identity(child.id(), &expect(sock, pid, Some(an_hour_ago)));
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(got, Identity::Unknown, "our flags, wrong start: never signal, never replace");
    }

    // The v9 rule: a bare pid file, so the process must have started no later
    // than the file was written.
    #[test]
    fn a_bare_pid_file_matches_a_daemon_that_started_before_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let (sock, pid) = (dir.path().join("relay-v9.sock"), dir.path().join("relay-v9.pid"));
        let mut child = fake_daemon(&sock, &pid);
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(&pid, child.id().to_string()).unwrap();
        let e = Expect {
            socket_path: sock.clone(),
            pid_path: pid.clone(),
            started_at: None,
            pid_file_mtime: mtime_secs(&pid),
        };
        let got = verify_daemon_identity(child.id(), &e);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(got, Identity::Match);
    }

    #[test]
    fn a_process_without_the_flags_is_not_the_daemon() {
        let (sock, pid) = (Path::new("/tmp/x/relay-v10.sock"), Path::new("/tmp/x/relay-v10.pid"));
        let own_start = oximux_proc_tree::start_time_of_pid(std::process::id());
        let got = verify_daemon_identity(std::process::id(), &expect(sock, pid, own_start));
        assert_eq!(got, Identity::Mismatch);
    }

    #[test]
    fn a_gone_pid_is_not_the_daemon() {
        let got = verify_daemon_identity(
            i32::MAX as u32,
            &expect(Path::new("/s"), Path::new("/p"), Some(now_secs())),
        );
        assert_eq!(got, Identity::Mismatch);
    }

    #[tokio::test]
    async fn stop_never_signals_a_process_that_is_not_the_daemon() {
        let mut bystander = std::process::Command::new("sleep").arg("30").spawn().expect("spawn");
        let e = expect(Path::new("/s"), Path::new("/p"), Some(now_secs()));
        let got = stop_verified_daemon(bystander.id(), &e, Duration::from_millis(200)).await;
        assert_eq!(got, Stopped::NotRunning);
        assert!(pid_alive(bystander.id()), "the bystander was left alone");
        let _ = bystander.kill();
        let _ = bystander.wait();
    }

    #[tokio::test]
    async fn stop_ends_a_verified_daemon() {
        let (sock, pid) = (Path::new("/tmp/x/relay-v10.sock"), Path::new("/tmp/x/relay-v10.pid"));
        let child = fake_daemon(sock, pid);
        let id = child.id();
        // Mirror the supervisor: the daemon is never waited on by its parent.
        std::mem::forget(child);
        let got = stop_verified_daemon(id, &expect(sock, pid, Some(now_secs())), Duration::from_secs(3)).await;
        assert_eq!(got, Stopped::Stopped);
    }
}
