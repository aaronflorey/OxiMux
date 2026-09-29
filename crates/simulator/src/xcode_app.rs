//! Finding an `Xcode*.app` on disk when `xcode-select` doesn't name one, and
//! switching the active developer directory to it.
//!
//! Installing Xcode (App Store or `.xip`) **never** changes `xcode-select`,
//! and neither does launching it: a Mac that had the Command Line Tools first
//! keeps them selected until someone runs `sudo xcode-select -s`. Xcode.app
//! itself works regardless (it uses its own toolchain), so from the person's
//! side "Xcode is installed and running" while `xcrun` still resolves to the
//! CLT, which have no simulator. [`installed_xcode_apps`] lets the panel say
//! *which* Xcode it found, and [`select`] performs the switch behind macOS's
//! own administrator prompt.
//!
//! Discovery is a directory listing only: it never runs `xcrun` or
//! `xcodebuild`, so it keeps the `availability` module's hard rule.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::runner::Runner;
use crate::{Result, SimError};

/// How long [`select`] waits: it blocks on a password prompt a person may
/// take a while to answer.
pub const SELECT_TIMEOUT: Duration = Duration::from_secs(600);

/// Every `Xcode*.app` bundle with a `Contents/Developer` directory in
/// `/Applications` and `~/Applications` — where the App Store and Apple's
/// `.xip` instructions put it. Unsorted; [`pick`] chooses among them.
pub fn installed_xcode_apps() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(Path::new(&home).join("Applications"));
    }
    roots
        .iter()
        .filter_map(|root| std::fs::read_dir(root).ok())
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_xcode_bundle_name(path) && developer_dir(path).is_dir())
        .collect()
}

/// The one to offer when several are installed: plain `Xcode.app` first
/// (the App Store's name), then non-beta builds, then the highest version
/// in the name, compared numerically (`Xcode-26.10.app` over `Xcode-26.3.app`).
pub fn pick(mut apps: Vec<PathBuf>) -> Option<PathBuf> {
    apps.sort_by_key(|app| {
        let name = file_name(app);
        let beta = name.to_ascii_lowercase().contains("beta");
        (name != "Xcode.app", beta, std::cmp::Reverse(version_in_name(&name)), name)
    });
    apps.into_iter().next()
}

/// The digit runs in a bundle name as numbers: `Xcode-26.10.app` → `[26, 10]`.
fn version_in_name(name: &str) -> Vec<u64> {
    name.split(|c: char| !c.is_ascii_digit()).filter_map(|run| run.parse().ok()).collect()
}

/// `<app>/Contents/Developer`: what `xcode-select -s` and `DEVELOPER_DIR` take.
pub fn developer_dir(app: &Path) -> PathBuf {
    app.join("Contents").join("Developer")
}

/// The shell command a person can run by hand for the same switch. The path
/// is single-quoted (a `'` inside it as `'\''`), so pasting it can never
/// expand `$…` or backticks from a bundle name under `sudo`.
pub fn select_command(app: &Path) -> String {
    let dir = developer_dir(app).display().to_string().replace('\'', "'\\''");
    format!("sudo xcode-select -s '{dir}'")
}

/// How [`select`] ended when the switch did not fail outright.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selected {
    Switched,
    /// The person dismissed the administrator prompt.
    Cancelled,
}

/// Make `app` the active developer directory (`xcode-select -s`), asking for
/// an administrator password through macOS's standard prompt. Blocking —
/// run it off the UI thread. The path travels as an `argv` item and is
/// quoted by AppleScript's `quoted form of`, never spliced into a script.
pub fn select(runner: &dyn Runner, app: &Path) -> Result<Selected> {
    let dir = developer_dir(app);
    let dir = dir.to_str().ok_or_else(|| SimError::Protocol(format!("non-UTF-8 Xcode path: {}", app.display())))?;
    let out = runner.run("/usr/bin/osascript", &select_args(dir), None, SELECT_TIMEOUT)?;
    if out.success() {
        return Ok(Selected::Switched);
    }
    // AppleScript's "User canceled." is error -128.
    if String::from_utf8_lossy(&out.stderr).contains("-128") {
        return Ok(Selected::Cancelled);
    }
    out.into_success("xcode-select").map(|_| Selected::Switched)
}

fn select_args(developer_dir: &str) -> [&str; 7] {
    [
        "-e",
        "on run argv",
        "-e",
        "do shell script \"/usr/bin/xcode-select -s \" & quoted form of (item 1 of argv) with administrator privileges",
        "-e",
        "end run",
        developer_dir,
    ]
}

fn is_xcode_bundle_name(path: &Path) -> bool {
    let name = file_name(path);
    name.starts_with("Xcode") && name.ends_with(".app")
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)] // only the unix-gated `select` tests script a runner
    use crate::runner::{CmdOutput, ScriptedRunner};

    fn apps(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| Path::new("/Applications").join(n)).collect()
    }

    #[test]
    fn pick_prefers_the_app_store_name_then_release_then_newest() {
        let got = pick(apps(&["Xcode-beta.app", "Xcode.app", "Xcode-26.3.app"]));
        assert_eq!(got, Some(PathBuf::from("/Applications/Xcode.app")));
        let got = pick(apps(&["Xcode-beta.app", "Xcode-26.1.app", "Xcode-26.3.app"]));
        assert_eq!(got, Some(PathBuf::from("/Applications/Xcode-26.3.app")));
        let got = pick(apps(&["Xcode-26.3.app", "Xcode-26.10.app", "Xcode-9.4.app"]));
        assert_eq!(got, Some(PathBuf::from("/Applications/Xcode-26.10.app")), "numeric, not lexical");
        assert_eq!(pick(apps(&["Xcode-beta.app"])), Some(PathBuf::from("/Applications/Xcode-beta.app")));
        assert_eq!(pick(Vec::new()), None);
    }

    #[test]
    fn only_xcode_named_bundles_count() {
        assert!(is_xcode_bundle_name(Path::new("/Applications/Xcode-26.app")));
        assert!(!is_xcode_bundle_name(Path::new("/Applications/Xcodes.zip")));
        assert!(!is_xcode_bundle_name(Path::new("/Applications/Simulator.app")));
    }

    // Asserts Unix path spelling (`Path::join` writes `\` on Windows); the
    // feature only runs on macOS.
    #[test]
    #[cfg(unix)]
    fn select_passes_the_path_as_argv_not_script_text() {
        let dir = "/Applications/Xcode 26 \"q\".app/Contents/Developer";
        let command = std::iter::once("/usr/bin/osascript").chain(select_args(dir)).collect::<Vec<_>>().join(" ");
        let runner = ScriptedRunner::default().expect(&command, CmdOutput::ok(""));
        let got = select(&runner, Path::new("/Applications/Xcode 26 \"q\".app")).unwrap();
        assert_eq!(got, Selected::Switched);
        // The path is its own argv item; the script text never contains it.
        assert!(!select_args(dir)[3].contains("Applications"));
        assert_eq!(select_args(dir)[6], dir);
    }

    #[test]
    #[cfg(unix)]
    fn select_reports_cancel_and_failure_apart() {
        let app = Path::new("/Applications/Xcode.app");
        let command = std::iter::once("/usr/bin/osascript")
            .chain(select_args("/Applications/Xcode.app/Contents/Developer"))
            .collect::<Vec<_>>()
            .join(" ");
        let runner = ScriptedRunner::default().expect(&command, CmdOutput::failed(1, "execution error: User canceled. (-128)"));
        assert_eq!(select(&runner, app).unwrap(), Selected::Cancelled);
        let runner = ScriptedRunner::default().expect(&command, CmdOutput::failed(1, "xcode-select: error: invalid developer directory"));
        assert!(select(&runner, app).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn select_command_single_quotes_the_developer_dir() {
        assert_eq!(
            select_command(Path::new("/Applications/Xcode.app")),
            "sudo xcode-select -s '/Applications/Xcode.app/Contents/Developer'"
        );
        // Nothing in a bundle name can expand when the command is pasted.
        assert_eq!(
            select_command(Path::new("/Users/me/Applications/Xcode`id`$(x)'s.app")),
            "sudo xcode-select -s '/Users/me/Applications/Xcode`id`$(x)'\\''s.app/Contents/Developer'"
        );
    }
}
