//! A session that dies with its daemon is not a program exit: the view marks
//! it lost (so it can be brought back, and a quit keeps its restore hint) and
//! never asks for its tab to be auto-closed.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::TestAppContext;
use oximux_pty::{SpawnConfig, TerminalEvent, TerminalSnapshot};

use super::*;

/// A backend whose one session was lost with its daemon: once `lost` is set,
/// its next drain reports `DaemonLost`, the way the relay backend's inherited
/// sessions do. Held back until then so the test is listening first — the
/// view's own poll task drains too.
struct LostBackend {
    lost: Arc<AtomicBool>,
    reported: bool,
}

impl TerminalBackend for LostBackend {
    fn spawn(&mut self, _cfg: SpawnConfig) -> anyhow::Result<TerminalSessionId> {
        anyhow::bail!("not used")
    }
    fn external_id_of(&self, _id: TerminalSessionId) -> Option<String> {
        Some("pty-gone".into())
    }
    fn write(&mut self, _id: TerminalSessionId, _bytes: &[u8]) -> anyhow::Result<()> {
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
    fn drain_events_for(&mut self, id: TerminalSessionId) -> Vec<TerminalEvent> {
        if !self.lost.load(Ordering::SeqCst) || std::mem::replace(&mut self.reported, true) {
            Vec::new()
        } else {
            vec![TerminalEvent::DaemonLost { id }]
        }
    }
    fn close(&mut self, _id: TerminalSessionId) -> anyhow::Result<()> {
        Ok(())
    }
}

#[gpui::test]
async fn a_session_lost_with_its_daemon_is_marked_and_never_clean_exits(cx: &mut TestAppContext) {
    let lost = Arc::new(AtomicBool::new(false));
    let backend: SharedBackend = Arc::new(std::sync::Mutex::new(Box::new(LostBackend {
        lost: Arc::clone(&lost),
        reported: false,
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
    let view = window.root(cx).expect("view");
    let seen = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&seen);
    cx.update(|cx| {
        cx.subscribe(&view, move |_, event, _| {
            sink.borrow_mut().push(match event {
                TerminalViewEvent::CleanExit { .. } => "clean-exit",
                TerminalViewEvent::DaemonLost { .. } => "daemon-lost",
            });
        })
        .detach();
    });

    lost.store(true, Ordering::SeqCst);
    view.update(cx, |v, cx| v.tick(cx));
    cx.run_until_parked();

    view.read_with(cx, |v, _| {
        assert!(v.lost_to_daemon, "marked lost");
        assert_eq!(v.exited, Some(-1), "shown as ended until it is brought back");
        assert_eq!(
            v.relay_id_for_capture().as_deref(),
            Some("pty-gone"),
            "a quit before recovery still restores it from its checkpoint"
        );
    });
    assert_eq!(*seen.borrow(), ["daemon-lost"], "lost, and never a clean exit");
}
