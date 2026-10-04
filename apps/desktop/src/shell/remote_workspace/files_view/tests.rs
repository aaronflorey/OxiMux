use super::*;
use gpui::TestAppContext;

#[gpui::test]
fn remote_file_save_failure_retains_draft_and_stale_reply_cannot_replace_it(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let window = cx.add_window(|_, _| RemoteFilesView::new("s".into(), Theme::default(), Density::default(), Typography::default()));
    window.update(cx, |view, _, cx| {
        view.pending = Some(TextFileWire { path: "host.txt".into(), text: "original".into(), version: "v1".into() });
        cx.notify();
    }).unwrap();
    cx.refresh().unwrap();
    window.update(cx, |view, window, cx| {
        view.buffers["host.txt"].editor.update(cx, |editor, cx| editor.set_value("unsaved draft", window, cx));
        view.busy = true;
        view.finish(0, false, Err("save failed".into()), cx);
        assert_eq!(view.buffers["host.txt"].loaded.text, "original");
        assert_eq!(view.buffers["host.txt"].editor.read(cx).value().as_ref(), "unsaved draft");
        assert_eq!(view.notice.as_deref(), Some("save failed"));
        assert!(view.dirty(cx));
        view.revision = 2;
        view.finish(1, false, Ok(Reply::Saved(TextFileWire { path: "host.txt".into(), text: "stale".into(), version: "v2".into() })), cx);
        assert_eq!(view.buffers["host.txt"].loaded.version, "v1");
        assert!(!view.writable());
        view.bind(None, None, cx);
        assert_eq!(view.buffers["host.txt"].editor.read(cx).value().as_ref(), "unsaved draft");
    }).unwrap();
    cx.refresh().unwrap();
}

#[gpui::test]
fn remote_file_confirmed_save_preserves_newer_edits_and_switching_buffers(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let window = cx.add_window(|_, _| RemoteFilesView::new("s".into(), Theme::default(), Density::default(), Typography::default()));
    window.update(cx, |view, _, cx| {
        view.pending = Some(TextFileWire { path: "a".into(), text: "original".into(), version: "v1".into() });
        cx.notify();
    }).unwrap();
    cx.refresh().unwrap();
    window.update(cx, |view, window, cx| {
        view.buffers["a"].editor.update(cx, |editor, cx| editor.set_value("newer edits", window, cx));
        view.finish(0, false, Ok(Reply::Saved(TextFileWire { path: "a".into(), text: "submitted".into(), version: "v2".into() })), cx);
        assert_eq!(view.buffers["a"].loaded.version, "v2");
        assert_eq!(view.buffers["a"].editor.read(cx).value().as_ref(), "newer edits");
        view.pending = Some(TextFileWire { path: "b".into(), text: "another".into(), version: "b1".into() });
        cx.notify();
    }).unwrap();
    cx.refresh().unwrap();
    window.update(cx, |view, _, cx| {
        assert_eq!(view.active.as_deref(), Some("b"));
        view.open("a".into(), cx);
        assert_eq!(view.buffers["a"].editor.read(cx).value().as_ref(), "newer edits");
        assert!(view.dirty(cx));
    }).unwrap();
    cx.refresh().unwrap();
}

#[gpui::test]
fn remote_files_initial_error_still_offers_refresh_and_success_clears_notice(cx: &mut TestAppContext) {
    cx.update(|cx| gpui_component::init(cx));
    let window = cx.add_window(|_, _| RemoteFilesView::new("s".into(), Theme::default(), Density::default(), Typography::default()));
    window.update(cx, |view, _, cx| view.finish(0, false, Err("session has no working directory".into()), cx)).unwrap();
    let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
    cx.refresh().unwrap();
    assert!(visual.debug_bounds("remote-files-refresh").is_some(), "the first listing failure must leave a retry affordance");
    window.update(cx, |view, _, cx| {
        view.finish(0, false, Ok(Reply::Directory(DirectoryWire { path: "".into(), entries: vec![], next: None })), cx);
        assert!(view.notice.is_none(), "a successful listing clears the previous error");
        assert!(view.directory.is_some());
    }).unwrap();
}
