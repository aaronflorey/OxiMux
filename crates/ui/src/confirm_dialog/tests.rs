//! Busy mode: a confirmed long action holds the dialog up, undismissable,
//! until the host finishes it.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{Entity, TestAppContext, VisualTestContext};

use super::*;

fn prompt(fired: Rc<Cell<u32>>) -> ConfirmPrompt {
    ConfirmPrompt {
        title: "Restart the terminal daemon?".into(),
        body: "Sessions restart.".into(),
        on_confirm: Rc::new(move |_, _| fired.set(fired.get() + 1)),
        confirm_label: Some("Restart".into()),
        on_cancel: None,
        secondary: None,
    }
}

fn open(cx: &mut TestAppContext, busy: bool) -> (Entity<ConfirmDialog>, Rc<Cell<u32>>, VisualTestContext) {
    cx.update(gpui_component::init);
    let fired = Rc::new(Cell::new(0));
    let prompt = prompt(Rc::clone(&fired));
    let window = cx.add_window(|window, cx| {
        let mut dialog = ConfirmDialog::new(
            prompt,
            Theme::default(),
            Density::default(),
            Typography::default(),
            window,
            cx,
        );
        if busy {
            dialog.set_busy_on_confirm("Restarting…");
        }
        dialog
    });
    let dialog = window.root(cx).expect("dialog");
    let vcx = VisualTestContext::from_window(window.into(), cx);
    vcx.run_until_parked();
    (dialog, fired, vcx)
}

#[gpui::test]
async fn a_busy_dialog_holds_until_finished(cx: &mut TestAppContext) {
    let (dialog, fired, mut cx) = open(cx, true);

    cx.simulate_keystrokes("enter");
    dialog.read_with(&cx, |d, _| {
        assert!(d.is_busy(), "busy once confirmed");
        // What the host's slot guard and observer test: still unresolved.
        assert!(!d.is_confirmed() && !d.is_cancelled());
    });
    assert_eq!(fired.get(), 1);

    cx.simulate_keystrokes("escape");
    cx.simulate_keystrokes("enter");
    dialog.update_in(&mut cx, |d, window, cx| d.cancel(window, cx));
    dialog.read_with(&cx, |d, _| {
        assert!(d.is_busy() && !d.is_cancelled(), "nothing dismisses it while busy");
    });
    assert_eq!(fired.get(), 1, "confirm runs once");

    dialog.update(&mut cx, |d, cx| d.finish(cx));
    dialog.read_with(&cx, |d, _| {
        assert!(!d.is_busy());
        assert!(d.is_confirmed(), "resolved, so the host drops it");
    });
}

#[gpui::test]
async fn without_busy_mode_confirm_resolves_at_once(cx: &mut TestAppContext) {
    let (dialog, fired, mut cx) = open(cx, false);

    cx.simulate_keystrokes("enter");

    dialog.read_with(&cx, |d, _| assert!(d.is_confirmed() && !d.is_busy()));
    assert_eq!(fired.get(), 1);
}

#[gpui::test]
async fn a_busy_dialog_can_still_be_cancelled_before_confirming(cx: &mut TestAppContext) {
    let (dialog, fired, mut cx) = open(cx, true);

    cx.simulate_keystrokes("escape");

    dialog.read_with(&cx, |d, _| assert!(d.is_cancelled() && !d.is_busy()));
    // A finish that arrives after a cancel changes nothing.
    dialog.update(&mut cx, |d, cx| d.finish(cx));
    dialog.read_with(&cx, |d, _| assert!(!d.is_confirmed()));
    assert_eq!(fired.get(), 0);
}
