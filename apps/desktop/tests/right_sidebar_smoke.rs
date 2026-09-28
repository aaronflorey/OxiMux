//! Smoke tests: construct `RightSidebar` with a static watch channel (no tokio
//! thread-pool tasks), verify default state, tab switching, toggle, and
//! the no-repo fallback that guards against SourceControl tab when hidden.

use gpui::TestAppContext;
use oximux_app::shell::right_sidebar::{RightSidebar, SidebarTestConfig, tab::RightTab};
use oximux_git::{PollState, Repository};
use oximux_settings::{Density, Theme, Typography};
use std::process::Command;
use tokio::sync::watch;

fn init_git_repo(p: &std::path::Path) {
    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(p)
        .status()
        .expect("git on PATH");
}

/// Shared setup: temp git repo + current_thread runtime.
fn setup_repo() -> (tokio::runtime::Runtime, Repository) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let tmp = tempfile::tempdir().expect("tempdir");
    init_git_repo(tmp.path());
    // leak dir so the path stays valid for the duration of the test
    let path = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    let repo = rt.block_on(Repository::open(&path)).expect("open repo");
    (rt, repo)
}

#[gpui::test]
async fn right_sidebar_defaults_to_source_control_open(cx: &mut TestAppContext) {
    let (rt, repo) = setup_repo();
    let _guard = rt.enter();

    cx.update(gpui_component::init);

    let (_tx, rx) = watch::channel(PollState::Loading);

    let window = cx.add_window(|win, cx| {
        RightSidebar::new_for_test(
            repo,
            SidebarTestConfig {
                state_rx: rx,
                has_repo: true,
                theme: Theme::default(),
                density: Density::default(),
                typography: Typography::default(),
            },
            win,
            cx,
        )
    });
    cx.run_until_parked();

    cx.read(|app| {
        let sidebar = window.read(app).expect("RightSidebar root view alive");
        assert_eq!(sidebar.active_tab, RightTab::SourceControl);
        assert!(sidebar.open);
    });
}

#[gpui::test]
async fn right_sidebar_select_tab_and_toggle(cx: &mut TestAppContext) {
    let (rt, repo) = setup_repo();
    let _guard = rt.enter();

    // SearchPanel inside RightSidebar uses gpui-component InputState, which
    // reads the theme global initialised by `gpui_component::init`.
    cx.update(gpui_component::init);

    let (_tx, rx) = watch::channel(PollState::Loading);

    let window = cx.add_window(|win, cx| {
        RightSidebar::new_for_test(
            repo,
            SidebarTestConfig {
                state_rx: rx,
                has_repo: true,
                theme: Theme::default(),
                density: Density::default(),
                typography: Typography::default(),
            },
            win,
            cx,
        )
    });
    cx.run_until_parked();

    // Switch to Explorer tab.
    window
        .update(cx, |sidebar, _window, cx| {
            sidebar.select_tab(RightTab::Explorer, cx);
        })
        .expect("update succeeds");
    cx.run_until_parked();

    cx.read(|app| {
        let sidebar = window.read(app).expect("view alive");
        assert_eq!(sidebar.active_tab, RightTab::Explorer);
        assert!(sidebar.open);
    });

    // Toggle closes the sidebar.
    window
        .update(cx, |sidebar, _window, cx| {
            sidebar.toggle(cx);
        })
        .expect("update succeeds");
    cx.run_until_parked();

    cx.read(|app| {
        let sidebar = window.read(app).expect("view alive");
        assert!(!sidebar.open);
    });
}

/// When `has_repo = false`, `_poller` is None so visible_tabs omits
/// SourceControl. Attempting `select_tab(SourceControl)` must fall back to Explorer.
#[gpui::test]
async fn right_sidebar_no_repo_select_source_control_falls_back(cx: &mut TestAppContext) {
    let (rt, repo) = setup_repo();
    let _guard = rt.enter();

    cx.update(gpui_component::init);

    let (_tx, rx) = watch::channel(PollState::Loading);

    let window = cx.add_window(|win, cx| {
        RightSidebar::new_for_test(
            repo,
            SidebarTestConfig {
                state_rx: rx,
                has_repo: false, // _poller = None
                theme: Theme::default(),
                density: Density::default(),
                typography: Typography::default(),
            },
            win,
            cx,
        )
    });
    cx.run_until_parked();

    // Default tab with no repo should NOT be SourceControl.
    cx.read(|app| {
        let sidebar = window.read(app).expect("view alive");
        assert_ne!(sidebar.active_tab, RightTab::SourceControl);
        assert_eq!(sidebar.active_tab, RightTab::Explorer);
    });

    // Trying to switch to SourceControl (not in visible_tabs) falls back to Explorer.
    window
        .update(cx, |sidebar, _window, cx| {
            sidebar.select_tab(RightTab::SourceControl, cx);
        })
        .expect("update succeeds");
    cx.run_until_parked();

    cx.read(|app| {
        let sidebar = window.read(app).expect("view alive");
        assert_eq!(sidebar.active_tab, RightTab::Explorer);
    });
}

/// A sidebar built for a plain folder may try to open a repo once `git init`
/// lands — but only one open at a time, and a failed open (`.git` still being
/// written) leaves it free to try again on the next tick rather than stuck
/// without Source Control. A git-backed sidebar never asks.
#[gpui::test]
async fn a_plain_folder_sidebar_retries_git_init_one_open_at_a_time(cx: &mut TestAppContext) {
    let (rt, repo) = setup_repo();
    let _guard = rt.enter();
    cx.update(gpui_component::init);

    let build = |has_repo: bool, repo: Repository, cx: &mut TestAppContext| {
        let (_tx, rx) = watch::channel(PollState::Loading);
        cx.add_window(move |win, cx| {
            RightSidebar::new_for_test(
                repo,
                SidebarTestConfig {
                    state_rx: rx,
                    has_repo,
                    theme: Theme::default(),
                    density: Density::default(),
                    typography: Typography::default(),
                },
                win,
                cx,
            )
        })
    };
    let plain = build(false, repo.clone(), cx);
    let git = build(true, repo, cx);
    cx.run_until_parked();

    cx.read(|app| {
        let plain = plain.read(app).expect("plain sidebar alive");
        assert!(plain.awaits_git_init());
        assert!(!plain.visible_tabs().contains(&RightTab::SourceControl));
        assert!(!git.read(app).expect("git sidebar alive").awaits_git_init());
    });

    let set_in_flight = |in_flight: bool, cx: &mut TestAppContext| {
        plain
            .update(cx, |sidebar, _window, _cx| sidebar.set_repo_probe_in_flight(in_flight))
            .expect("update succeeds");
        cx.read(|app| plain.read(app).expect("plain sidebar alive").awaits_git_init())
    };
    assert!(!set_in_flight(true, cx), "no second open while one is in flight");
    assert!(set_in_flight(false, cx), "a failed open is retried");
}
