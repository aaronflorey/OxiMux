//! The input method stays on when a program switches to the alt-screen.
//! Full-screen TUIs take typed text too — Claude Code's fullscreen UI is one —
//! and with the input method switched off there Telex typed `oo` as "oo".

use std::sync::Mutex;

use gpui::{AppContext as _, TestAppContext, VisualTestContext};
use oximux_pty::{MouseMode, SpawnConfig, TerminalEvent, TerminalSnapshot};

use super::*;

/// One session on the alt-screen that records what reaches its PTY.
struct AltScreenBackend {
    written: Arc<Mutex<Vec<u8>>>,
}

impl TerminalBackend for AltScreenBackend {
    fn spawn(&mut self, _cfg: SpawnConfig) -> anyhow::Result<TerminalSessionId> {
        anyhow::bail!("not used")
    }
    fn write(&mut self, _id: TerminalSessionId, bytes: &[u8]) -> anyhow::Result<()> {
        self.written.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    fn resize(&mut self, _id: TerminalSessionId, _cols: u16, _rows: u16) -> anyhow::Result<()> {
        Ok(())
    }
    fn snapshot(&self, _id: TerminalSessionId) -> anyhow::Result<TerminalSnapshot> {
        Ok(TerminalSnapshot::empty(80, 24))
    }
    fn drain_events(&mut self) -> Vec<TerminalEvent> {
        Vec::new()
    }
    fn close(&mut self, _id: TerminalSessionId) -> anyhow::Result<()> {
        Ok(())
    }
    fn mouse_mode(&self, _id: TerminalSessionId) -> MouseMode {
        MouseMode {
            alt_screen: true,
            ..MouseMode::default()
        }
    }
}

fn alt_screen_view(
    cx: &mut TestAppContext,
) -> (gpui::WindowHandle<TerminalView>, Arc<Mutex<Vec<u8>>>) {
    let written = Arc::new(Mutex::new(Vec::new()));
    let backend: SharedBackend = Arc::new(std::sync::Mutex::new(Box::new(AltScreenBackend {
        written: Arc::clone(&written),
    })));
    let window = cx.add_window(|win, cx| {
        TerminalView::mount_background(
            backend,
            TerminalSessionId(1),
            SurfaceIds::restored("/proj", "surface-1".into(), "tab-1".into()),
            Theme::default(),
            Density::default(),
            Typography::default(),
            win,
            cx,
        )
    });
    (window, written)
}

/// Telex on the alt-screen: the first `o` is committed, the second reads it
/// back and rewrites it as "ô" — the PTY gets `o`, one Backspace, then `ô`,
/// exactly as off the alt-screen.
#[gpui::test]
async fn telex_composes_on_the_alt_screen(cx: &mut TestAppContext) {
    let (window, written) = alt_screen_view(cx);
    let view = window.root(cx).expect("view");
    let mut handler = TerminalInputHandler {
        view,
        cursor_bounds: None,
    };

    cx.update_window(window.into(), |_, win, cx| {
        let selection = handler.selected_text_range(false, win, cx);
        assert_eq!(
            selection.map(|s| s.range),
            Some(0..0),
            "the input method is on, at the caret"
        );
        handler.replace_text_in_range(None, "o", win, cx);
        let mut covered = None;
        assert_eq!(handler.text_for_range(0..200, &mut covered, win, cx).as_deref(), Some("o"));
        handler.replace_text_in_range(Some(0..1), "ô", win, cx);
    })
    .unwrap();

    assert_eq!(
        String::from_utf8(written.lock().unwrap().clone()).unwrap(),
        "o\u{7f}ô"
    );
}

/// A plain letter on the alt-screen goes to the input method, not the byte
/// encoder: written once, and kept as what the input method typed (the byte
/// path would have cleared that, leaving Telex nothing to read back).
#[gpui::test]
async fn a_letter_on_the_alt_screen_goes_through_the_input_method(cx: &mut TestAppContext) {
    let (window, written) = alt_screen_view(cx);
    let view = window.root(cx).expect("view");
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    cx.update(|win, cx| {
        let focus = view.read(cx).focus_handle.clone();
        focus.focus(win, cx);
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("o");

    assert_eq!(written.lock().unwrap().as_slice(), b"o", "written once");
    view.read_with(cx, |v, _| {
        assert_eq!(v.ime_typed.text(None, 0..200).0, "o", "the input method typed it");
    });
}

/// Ctrl+C mid-composition still interrupts the program: control keys are not
/// the input method's to hold back.
#[gpui::test]
async fn ctrl_c_mid_composition_reaches_the_program(cx: &mut TestAppContext) {
    let (window, written) = alt_screen_view(cx);
    let view = window.root(cx).expect("view");
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    cx.update(|win, cx| {
        let focus = view.read(cx).focus_handle.clone();
        focus.focus(win, cx);
    });
    cx.run_until_parked();
    view.update(cx, |v, cx| v.set_ime_marked("ô".into(), cx));

    cx.simulate_keystrokes("ctrl-c");

    assert_eq!(written.lock().unwrap().as_slice(), b"\x03");
}
