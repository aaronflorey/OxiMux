//! Remote pairing modal — host name + pairing ticket form as a centered
//! card over the shell, replacing the old full-window pairing screen.
//! Opened from the rail's "Connect a host…" row (and the remote workspace's
//! re-pair surface). Successful pair hands a `PairingTicket` to
//! [`RemoteHosts::pair`], which dials and registers the host in the rail.
//!
//! Layout mirrors `add_project_dialog.rs`: full-window overlay for
//! click-outside dismiss, centered card with `FocusHandle` for escape.

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, Styled, Window,
    div, px,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
};
use oximux_remote_proto::PairingTicket;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::remote_hosts::RemoteHosts;
use crate::ui::FloatingSurface;

/// Card width — narrower than the project dialog; one column of fields.
const MODAL_WIDTH: f32 = 420.0;
/// Vertical offset from the top of the viewport.
const MODAL_TOP_OFFSET: f32 = 120.0;

pub struct RemotePairModal {
    open: bool,
    /// The host pool — `pair` creates + dials the new entity.
    hosts: Entity<RemoteHosts>,
    /// Display-name field, created lazily on first `open` (the constructor
    /// runs without `&mut Window`, which `InputState` needs).
    name: Option<Entity<InputState>>,
    /// Ticket field — masked at rest, `show_ticket` reveals for debugging a
    /// paste problem (same affordance the takeover screen had).
    ticket: Option<Entity<InputState>>,
    show_ticket: bool,
    /// Last validation or dial failure, shown inline.
    error: Option<String>,
    /// `true` between submit and the host appearing `Connected` — lets the
    /// Connect button show progress. Cleared by `open`/`close`/`submit`.
    submitting: bool,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl RemotePairModal {
    pub fn new(
        hosts: Entity<RemoteHosts>,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            open: false,
            hosts,
            name: None,
            ticket: None,
            show_ticket: false,
            error: None,
            submitting: false,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.show_ticket = false;
        self.error = None;
        self.submitting = false;
        match &self.name {
            None => {
                let input =
                    cx.new(|cx| InputState::new(window, cx).placeholder("My home server"));
                self.name = Some(input);
            }
            Some(input) => input.update(cx, |input, cx| input.set_value("", window, cx)),
        }
        match &self.ticket {
            None => {
                let input = cx.new(|cx| {
                    let mut i = InputState::new(window, cx).placeholder("oximux://pair?…");
                    i.set_masked(true, window, cx);
                    i
                });
                self.ticket = Some(input);
            }
            Some(input) => input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.set_masked(true, window, cx);
            }),
        }
        if let Some(name) = &self.name {
            name.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.error = None;
        self.submitting = false;
        cx.notify();
    }

    /// Validate the form and hand the ticket to the host pool. Pairing
    /// itself is async (the dial runs on the driver task); the modal closes
    /// on submit and the rail row reflects connecting → connected/failed.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name_input) = self.name.clone() else {
            return;
        };
        let Some(ticket_input) = self.ticket.clone() else {
            return;
        };
        let name = name_input.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.error = Some("Enter a host name.".into());
            cx.notify();
            return;
        }
        let ticket_text = ticket_input.read(cx).unmask_value();
        let ticket = match PairingTicket::parse(ticket_text.as_ref()) {
            Ok(t) => t,
            Err(_) => {
                self.error = Some(
                    "Invalid pairing ticket. Paste the full connection URL, or mint a \
                     fresh ticket on the host with `oximux pair-new`."
                        .into(),
                );
                cx.notify();
                return;
            }
        };
        // Drop the bearer credential from the input immediately after
        // submission — same rule the takeover screen followed.
        ticket_input.update(cx, |input, cx| input.set_value("", window, cx));
        self.submitting = true;
        self.hosts.update(cx, |hosts, cx| {
            hosts.pair(name, ticket, cx);
        });
        self.close(cx);
    }
}

impl Focusable for RemotePairModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RemotePairModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(
            &mut self.theme,
            &mut self.density,
            &mut self.typography,
            cx,
        );
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();

        let title = div()
            .text_size(px(typography.t_body_md * 1.15))
            .font_weight(typography.w_semibold)
            .text_color(theme.fg_base)
            .child("Connect to a remote host");

        let subtitle = div()
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_muted)
            .child("Paste a pairing ticket from `oximux pair-new` on the host.");

        let mut col = div()
            .flex()
            .flex_col()
            .gap(px(density.gap_inline))
            .child(title)
            .child(subtitle)
            .child(
                div()
                    .text_size(px(typography.t_label_caps))
                    .text_color(theme.fg_subtle)
                    .child("Host name"),
            );
        if let Some(input) = &self.name {
            col = col.child(Input::new(input).disabled(self.submitting));
        }
        col = col.child(
            div()
                .text_size(px(typography.t_label_caps))
                .text_color(theme.fg_subtle)
                .child("Pairing ticket"),
        );
        if let Some(input) = &self.ticket {
            col = col.child(Input::new(input).disabled(self.submitting));
        }
        col = col.child(
            div()
                .id("remote-pair-show-ticket")
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .cursor_pointer()
                .hover(|s| s.text_color(theme.fg_muted))
                .child(if self.show_ticket {
                    "Hide ticket"
                } else {
                    "Show ticket"
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.show_ticket = !this.show_ticket;
                        let masked = !this.show_ticket;
                        if let Some(ticket) = &this.ticket {
                            ticket.update(cx, |i, cx| i.set_masked(masked, window, cx));
                        }
                        cx.notify();
                    }),
                ),
        );
        if let Some(err) = &self.error {
            col = col.child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.status_error)
                    .child(err.clone()),
            );
        }
        col = col.child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .gap(px(density.gap_inline))
                .child(
                    Button::new("remote-pair-cancel")
                        .label("Cancel")
                        .disabled(self.submitting)
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.close(cx);
                        })),
                )
                .child(
                    Button::new("remote-pair-connect")
                        .primary()
                        .label(if self.submitting {
                            "Connecting…"
                        } else {
                            "Connect"
                        })
                        .disabled(self.submitting)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.submit(window, cx);
                        })),
                ),
        );

        let card = div()
            .flex()
            .flex_col()
            .gap(px(density.gap_inline * 1.5))
            .w(px(MODAL_WIDTH))
            .p(px(density.pad_panel * 2.0))
            .floating_chrome(&theme, &density)
            .shadow_lg()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "escape" => this.close(cx),
                    "enter" => this.submit(window, cx),
                    _ => {}
                }
            }))
            .child(col);

        div()
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| this.close(cx)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(MODAL_TOP_OFFSET))
                    .flex()
                    .w_full()
                    .justify_center()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(card),
            )
            .into_any_element()
    }
}
