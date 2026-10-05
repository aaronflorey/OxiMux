use super::*;
use gpui::TestAppContext;

#[gpui::test]
fn remote_file_save_failure_retains_draft_and_stale_reply_cannot_replace_it(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let window = cx.add_window(|_, _| RemoteFilesView::new(crate::shell::remote_workspace::Root::Session("s".into()), Theme::default(), Density::default(), Typography::default()));
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
    let window = cx.add_window(|_, _| RemoteFilesView::new(crate::shell::remote_workspace::Root::Session("s".into()), Theme::default(), Density::default(), Typography::default()));
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

/// The durable-draft contract under the window-close guard: a dirty
/// buffer serializes with its host baseline (text + version), and a
/// fresh view — the mount after a close or quit — rehydrates it as a
/// dirty buffer that diffs and saves exactly like the original.
#[gpui::test]
fn remote_draft_capture_and_restore_roundtrips_dirty_buffers(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
    let window = cx.add_window(|_, _| RemoteFilesView::new(crate::shell::remote_workspace::Root::Session("s".into()), Theme::default(), Density::default(), Typography::default()));
    let captured = window.update(cx, |view, window, cx| {
        view.plant_buffer("a.txt", "host base", "unsaved work", window, cx);
        // Clean buffers must not serialize — only divergence is a draft.
        view.plant_buffer("b.txt", "untouched", "untouched", window, cx);
        view.capture_drafts("ab12", "session:s", "w1", cx)
    }).unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].endpoint, "ab12");
    assert_eq!(captured[0].root, "session:s");
    assert_eq!(captured[0].window, "w1");
    assert_eq!(captured[0].path, "a.txt");
    assert_eq!(captured[0].base_text, "host base");
    assert_eq!(captured[0].base_version, "v0");
    assert_eq!(captured[0].draft, "unsaved work");

    let remounted = cx.add_window(|_, _| RemoteFilesView::new(crate::shell::remote_workspace::Root::Session("s".into()), Theme::default(), Density::default(), Typography::default()));
    remounted.update(cx, |view, window, cx| {
        view.restore_drafts(captured, window, cx);
        assert_eq!(view.dirty_buffers(cx), 1, "restored draft counts dirty");
        let buffer = &view.buffers["a.txt"];
        assert_eq!(buffer.loaded.text, "host base");
        assert_eq!(buffer.loaded.version, "v0", "the baseline rides along so a later save version-checks the same way");
        assert_eq!(buffer.editor.read(cx).value().as_ref(), "unsaved work");
        assert_eq!(view.active.as_deref(), Some("a.txt"));
        assert!(view.dirty(cx));
        assert!(!view.buffers.contains_key("b.txt"), "clean buffers never restore");
    }).unwrap();
    cx.refresh().unwrap();
}

#[gpui::test]
fn remote_files_initial_error_still_offers_refresh_and_success_clears_notice(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let window = cx.add_window(|_, _| RemoteFilesView::new(crate::shell::remote_workspace::Root::Session("s".into()), Theme::default(), Density::default(), Typography::default()));
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
