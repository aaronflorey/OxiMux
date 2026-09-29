//! ConfirmDialog — reusable confirm modal.
//!
//! One shape: title, body, a Cancel button and a destructive Confirm button
//! that fires on the first click. Destructive weight is carried by the copy
//! and the danger-styled button, not by a typing gate — a modal the user has
//! to transcribe a filename into costs every discard a detour while stopping
//! nothing a second look wouldn't.
//!
//! An optional middle action turns it into a three-way prompt (Save /
//! Discard / Cancel). Escape and the Cancel button dismiss; Enter confirms.
//! The dialog flips `confirmed` / `cancelled` so the host can drop it from
//! the modal stack.
//!
//! Busy mode ([`ConfirmDialog::set_busy_on_confirm`]) is for an action that
//! takes a while: confirming runs the callback and keeps the dialog up with a
//! spinner, undismissable, until the host calls [`ConfirmDialog::finish`].
//! It stays unresolved (neither confirmed nor cancelled) until then, so the
//! host's slot guard refuses to replace it and its observer keeps it mounted.

use gpui::{
    App, ClickEvent, Context, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, SharedString, Styled, Window, div, px,
    prelude::FluentBuilder,
};
use gpui_component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    spinner::Spinner,
};
use oximux_settings::{Density, Theme, Typography};
use std::rc::Rc;

/// Boxed callback fired when the user resolves the dialog — confirm, the
/// secondary action, or cancel. `Rc` (not `Arc`) because GPUI views are
/// single-threaded on the foreground executor; `Rc<dyn Fn>` avoids the
/// `Send` bound that the closure would otherwise need.
pub type ConfirmCallback = Rc<dyn Fn(&mut Window, &mut App) + 'static>;

/// Bundle of message + intent fields. Keeps the constructor under clippy's
/// arg-count ceiling and makes the call site read like a builder.
pub struct ConfirmPrompt {
    pub title: SharedString,
    pub body: SharedString,
    pub on_confirm: ConfirmCallback,
    /// Optional override for the destructive button label. Defaults to
    /// `"Confirm"` for back-compat with stash / workspace callers; SCM
    /// discard passes `Some("Delete")` / `"Restore"` / `"Discard"`
    /// keyed off `DiscardKind`.
    pub confirm_label: Option<SharedString>,
    /// Optional callback fired when the user dismisses the dialog
    /// without confirming (Escape, click-outside, explicit cancel).
    /// Use it to clear any host-side "pending request" state so the
    /// dialog can re-mount on the next click.
    pub on_cancel: Option<ConfirmCallback>,
    /// Optional middle action, turning the dialog into a three-way choice
    /// (e.g. unsaved-changes: Save / Discard / Cancel). When present the
    /// primary confirm button renders as the safe default (`primary`) and
    /// this secondary button takes the destructive `danger` styling. Firing
    /// it runs the callback and resolves the dialog like confirm. `None`
    /// keeps the classic two-button (Cancel + danger Confirm) layout.
    pub secondary: Option<ConfirmSecondary>,
}

/// A third "secondary" action for [`ConfirmPrompt`] (the destructive middle
/// option in a three-way prompt).
pub struct ConfirmSecondary {
    pub label: SharedString,
    pub on_click: ConfirmCallback,
}

pub struct ConfirmDialog {
    title: SharedString,
    body: SharedString,
    confirm_label: SharedString,
    on_confirm: Option<ConfirmCallback>,
    on_cancel: Option<ConfirmCallback>,
    /// Label + callback for the optional destructive middle action. `None`
    /// for the classic two-button layout.
    secondary_label: Option<SharedString>,
    on_secondary: Option<ConfirmCallback>,
    confirmed: bool,
    cancelled: bool,
    /// Set by [`Self::set_busy_on_confirm`]: the label the confirm button
    /// shows while the confirmed action runs.
    busy_label: Option<SharedString>,
    busy: bool,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl ConfirmDialog {
    pub fn new(
        prompt: ConfirmPrompt,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ConfirmPrompt {
            title,
            body,
            on_confirm,
            confirm_label,
            on_cancel,
            secondary,
        } = prompt;
        let (secondary_label, on_secondary) = match secondary {
            Some(ConfirmSecondary { label, on_click }) => (Some(label), Some(on_click)),
            None => (None, None),
        };
        let focus_handle = cx.focus_handle();
        // Land focus on the card so Enter / Escape work without a click.
        // Deferred: these dialogs open from a click handler, and GPUI's
        // post-click focus dispatch runs after it and would clobber a
        // synchronous focus here.
        {
            let handle = focus_handle.clone();
            window.defer(cx, move |window, cx| window.focus(&handle, cx));
        }
        Self {
            title,
            body,
            confirm_label: confirm_label.unwrap_or_else(|| "Confirm".into()),
            on_confirm: Some(on_confirm),
            on_cancel,
            secondary_label,
            on_secondary,
            confirmed: false,
            cancelled: false,
            busy_label: None,
            busy: false,
            focus_handle,
            theme,
            density,
            typography,
        }
    }

    /// Keep the dialog up once confirmed, showing `label` beside a spinner on
    /// the disabled confirm button, until [`Self::finish`]. Nothing dismisses
    /// it meanwhile — not Cancel, not Escape.
    pub fn set_busy_on_confirm(&mut self, label: impl Into<SharedString>) {
        self.busy_label = Some(label.into());
    }

    /// The confirmed action is running.
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// The confirmed action is done: resolve the dialog so the host drops it.
    /// A no-op unless busy, so "confirmed" always means the action ran.
    ///
    /// Call it from the action's completion, never synchronously from inside
    /// `on_confirm`: that runs while this dialog is being updated, and a
    /// nested update of it panics. An action that fails at once finishes
    /// from `cx.defer`.
    pub fn finish(&mut self, cx: &mut Context<Self>) {
        if !self.busy {
            return;
        }
        self.busy = false;
        self.confirmed = true;
        cx.notify();
    }

    pub fn is_confirmed(&self) -> bool {
        self.confirmed
    }

    /// `true` once the user has explicitly dismissed the dialog without
    /// confirming. The host observes this flag to drop the dialog from
    /// its slot and clear the matching pending request.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Whether the destructive button may fire right now — i.e. the dialog
    /// hasn't already been resolved.
    fn can_confirm(&self) -> bool {
        !self.confirmed && !self.cancelled && !self.busy
    }

    fn try_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_confirm() {
            return;
        }
        if let Some(cb) = self.on_confirm.take() {
            cb(window, cx);
        }
        if self.busy_label.is_some() {
            self.busy = true;
        } else {
            self.confirmed = true;
        }
        cx.notify();
    }

    /// Fire the optional middle action (e.g. "Discard") and resolve the
    /// dialog. No-op if no secondary action is registered or already resolved.
    fn try_secondary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirmed || self.cancelled || self.busy {
            return;
        }
        if let Some(cb) = self.on_secondary.take() {
            cb(window, cx);
            self.confirmed = true;
            cx.notify();
        }
    }

    /// Trigger the cancel pathway: fire `on_cancel` (if registered) and
    /// flip `cancelled = true` so the host's observer can drop the
    /// dialog. Idempotent — repeated calls are harmless. Ignored while busy.
    pub fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cancelled || self.confirmed || self.busy {
            return;
        }
        if let Some(cb) = self.on_cancel.take() {
            cb(window, cx);
        }
        self.cancelled = true;
        cx.notify();
    }
}

impl Focusable for ConfirmDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConfirmDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let can_confirm = self.can_confirm();
        let busy = self.busy;
        let confirm_label = match (&self.busy_label, busy) {
            (Some(label), true) => label.clone(),
            _ => self.confirm_label.clone(),
        };
        let secondary_label = self.secondary_label.clone();
        let has_secondary = secondary_label.is_some();
        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(
                |dlg, event: &KeyDownEvent, window, cx| {
                    // Enter fires the primary action, Escape dismisses;
                    // everything else falls through untouched. A busy
                    // dialog swallows both, so no ancestor acts on them.
                    match event.keystroke.key.as_str() {
                        "enter" | "escape" if dlg.busy => cx.stop_propagation(),
                        "enter" => dlg.try_confirm(window, cx),
                        "escape" => dlg.cancel(window, cx),
                        _ => {}
                    }
                },
            ))
            .flex()
            .flex_col()
            .w(px(360.0))
            .p(px(density.pad_panel + density.pad_overlay))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .gap(px(density.gap_inline))
            .child(
                div()
                    .text_size(px(typography.t_body_md))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_base)
                    .child(self.title.clone()),
            )
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_muted)
                    .line_height(px(typography.t_body_sm * 1.45))
                    .child(self.body.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(density.gap_inline))
                    .mt(px(density.pad_panel))
                    .child(
                        Button::new("cancel-button")
                            .small()
                            .outline()
                            .label("Cancel")
                            .disabled(busy)
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.cancel(window, cx);
                            })),
                    )
                    // Destructive middle action (e.g. "Discard"), present only
                    // for three-way prompts. Disabled only while busy.
                    .when_some(secondary_label, |row, label| {
                        row.child(
                            Button::new("secondary-button")
                                .small()
                                .danger()
                                .label(label)
                                .disabled(busy)
                                .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                    dlg.try_secondary(window, cx);
                                })),
                        )
                    })
                    .child(
                        // Primary/safe default when a destructive secondary
                        // exists (Save); otherwise this IS the destructive
                        // action (danger) for the classic two-button prompt.
                        Button::new("confirm-button")
                            .small()
                            .map(|b| if has_secondary { b.primary() } else { b.danger() })
                            .label(confirm_label)
                            // The kit only draws its spinner in place of an
                            // icon, so a label-only button needs one.
                            .when(busy, |b| b.icon(Spinner::new()))
                            .loading(busy)
                            .disabled(!can_confirm)
                            .on_click(cx.listener(|dlg, _: &ClickEvent, window, cx| {
                                dlg.try_confirm(window, cx);
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests;
