use super::*;
use gpui::TestAppContext;

#[gpui::test]
fn remote_git_failed_preview_preserves_status_but_a_failed_mutation_invalidates(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let window = cx.add_window(|window, cx| RemoteGitView::new("s".into(), "session".into(),
        Theme::default(), Density::default(), Typography::default(), window, cx));
    window.update(cx, |view, _, cx| {
        view.status = Some(GitStatusWire { branch: Some("main".into()), upstream: None,
            ahead: 0, behind: 0, files: vec![] });
        // A failed diff preview leaves the last-known status in place.
        view.finish(view.revision, false, None, Err("diff read failed".into()), cx);
        assert!(view.status.is_some(), "a failed preview cannot erase a valid status");
        assert_eq!(view.notice.as_deref(), Some("diff read failed"));
        // So does a failed status refresh.
        view.finish(view.revision, false, None, Err("status read failed".into()), cx);
        assert!(view.status.is_some());
        // A failed mutation may have partially applied — the cached status is
        // no longer trustworthy and must invalidate.
        view.finish(view.revision, true, None, Err("stage failed".into()), cx);
        assert!(view.status.is_none());
        assert_eq!(view.notice.as_deref(), Some("stage failed"));
    }).unwrap();
}

#[gpui::test]
fn remote_git_failed_commit_keeps_draft_and_releases_controls(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let window = cx.add_window(|window, cx| RemoteGitView::new("server".into(), "session".into(),
        Theme::default(), Density::default(), Typography::default(), window, cx));
    window.update(cx, |view, window, cx| {
        view.commit.update(cx, |input, cx| input.set_value("preserve this message", window, cx));
        view.busy = true;
        view.finish(view.revision, true, Some("preserve this message".into()), Err("host refused commit".into()), cx);
        assert!(!view.busy);
        assert_eq!(view.notice.as_deref(), Some("host refused commit"));
    }).unwrap();
    // Use GPUI's frame lifecycle, which drops arena-owned elements and their
    // entity handles. Calling Render directly outside a frame leaks elements.
    cx.refresh().unwrap();
    window.update(cx, |view, _, cx| {
        assert_eq!(view.commit.read(cx).value().as_ref(), "preserve this message");
        assert!(view.clear_commit.is_none());
        let old = view.revision;
        view.bind(None, None, cx);
        view.finish(old, true, Some("preserve this message".into()), Ok(Reply::Mutated {
            sha: Some("stale-sha".into()), status: Err("closed".into()),
        }), cx);
    }).unwrap();
    cx.refresh().unwrap();
    window.update(cx, |view, _, cx| {
        assert_eq!(view.commit.read(cx).value().as_ref(), "preserve this message");
        assert!(!view.writable());
    }).unwrap();
}

#[gpui::test]
fn remote_git_clears_only_the_confirmed_draft_even_if_refresh_fails(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let window = cx.add_window(|window, cx| RemoteGitView::new("server".into(), "session".into(),
        Theme::default(), Density::default(), Typography::default(), window, cx));
    window.update(cx, |view, window, cx| {
        view.commit.update(cx, |input, cx| input.set_value("submitted", window, cx));
        view.finish(view.revision, true, Some("submitted".into()), Ok(Reply::Mutated {
            sha: Some("confirmed-sha".into()), status: Err("repository closed".into()),
        }), cx);
    }).unwrap();
    cx.refresh().unwrap();
    window.update(cx, |view, window, cx| {
        assert!(view.commit.read(cx).value().is_empty(), "a confirmed commit clears its message");
        assert!(view.notice.as_ref().unwrap().contains("succeeded"));
        assert!(view.status.is_none(), "an uncertain status cannot enable another commit");
        view.commit.update(cx, |input, cx| input.set_value("next commit", window, cx));
        view.finish(view.revision, true, Some("older message".into()), Ok(Reply::Mutated {
            sha: Some("another-confirmed-sha".into()), status: Err("offline".into()),
        }), cx);
    }).unwrap();
    cx.refresh().unwrap();
    window.update(cx, |view, _, cx| {
        assert_eq!(view.commit.read(cx).value().as_ref(), "next commit", "a new draft survives an older reply");
    }).unwrap();
}

#[gpui::test]
fn remote_git_commit_stays_visible_when_changed_files_scroll(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let window = cx.add_window(|window, cx| RemoteGitView::new("s".into(), "Fix tests".into(),
        Theme::default(), Density::default(), Typography::default(), window, cx));
    window.update(cx, |view, _, cx| {
        view.status = Some(GitStatusWire { branch: Some("main".into()), upstream: None, ahead: 0, behind: 0,
            files: (0..30).map(|i| oximux_remote_proto::messages::GitFileWire {
                path: format!("file-{i}.rs"), index: IndexStatusWire::Modified, worktree: WorktreeStatusWire::Modified,
                staged_lines: None, unstaged_lines: None,
            }).collect() });
        cx.notify();
    }).unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    for (height, zoom) in [(900.0, 100), (480.0, 160)] {
        cx.update(|cx| cx.set_global(oximux_settings::Appearance {
            scale: oximux_settings::UiScale::from_percent(zoom), ..Default::default()
        }));
        visual.simulate_resize(gpui::size(px(720.0), px(height)));
        cx.refresh().unwrap();
        let bounds = visual.debug_bounds("remote-git-commit").expect("commit action must paint outside the file scroll area");
        assert!(bounds.origin.y >= px(0.0) && bounds.bottom() <= px(height), "commit must remain visible");
    }
    window.update(cx, |view, _, cx| {
        view.finish(0, true, None, Ok(Reply::Mutated { sha: Some("0123456789abcdef0123456789abcdef01234567".into()),
            status: Ok(GitStatusWire { branch: None, upstream: None, ahead: 0, behind: 0, files: vec![] }) }), cx);
        assert_eq!(view.notice.as_deref(), Some("Committed 01234567"));
    }).unwrap();
}
