//! Familiar cockpit chrome, with navigation and tools scoped to this host.
use super::*;
use gpui::FontWeight;
use gpui_component::{Selectable, Sizable, Icon, menu::{DropdownMenu, PopupMenuItem}};
use crate::shell::chrome::top_bar;

impl RemoteWorkspace {
    fn host_picker(&self, id: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let hosts = self.hosts.entries.clone();
        let selected = self.selected.as_ref().map(|host| host.name.clone());
        let weak = cx.entity().downgrade();
        Button::new(id).small()
            .label(selected.clone().unwrap_or_else(|| "Choose host".into()))
            .icon(Icon::default().path("icons/chevron-down.svg"))
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.label("Remote hosts");
                for host in &hosts {
                    let host = host.clone();
                    let weak = weak.clone();
                    menu = menu.item(PopupMenuItem::new(host.name.clone())
                        .checked(selected.as_ref() == Some(&host.name))
                        .on_click(move |_, window, cx| { let _ = weak.update(cx, |view, cx| { view.connect(host.clone(), None, cx); view.focus_active(window, cx); }); }));
                }
                let weak = weak.clone();
                menu.separator().item(PopupMenuItem::new("Pair a new host…").on_click(move |_, window, cx| {
                    let _ = weak.update(cx, |view, cx| view.show_pairing(window, cx));
                })).item(PopupMenuItem::new("Back to local").on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(crate::actions::SelectLocalHost), cx);
                }))
            })
    }

    fn status(&self, cx: &gpui::App) -> (String, gpui::Hsla) {
        let host = self.selected.as_ref().map(|h| h.name.as_str()).unwrap_or("host");
        match self.conn_state(cx) {
            ConnState::Disconnected if self.selected.is_none() => ("No host selected".into(), self.theme.fg_muted),
            ConnState::Disconnected => (format!("Disconnected from {host}"), self.theme.status_muted),
            ConnState::Connecting => (format!("Connecting to {host}…"), self.theme.status_info),
            ConnState::Connected => ("Connected".into(), self.theme.status_ok),
            ConnState::WaitingToRetry { attempt, .. } => (format!("Reconnecting to {host}… ({attempt})"), self.theme.status_warn),
            ConnState::Unreachable { .. } => (format!("Cannot reach {host}"), self.theme.status_error),
        }
    }

    fn connection_details(&self, cx: &mut Context<Self>) -> gpui::Div {
        let (status, color) = self.status(cx);
        let state = self.conn_state(cx);
        div().flex().flex_col().items_start().gap(px(self.density.gap_inline)).p(px(self.density.pad_panel))
            .child(self.host_picker("remote-host-picker", cx))
            .child(div().flex().items_center().gap(px(self.density.gap_inline))
                .child(div().size(px(self.density.scale(6.0))).rounded_full().bg(color))
                .child(div().text_size(px(self.typography.t_body_sm)).text_color(self.theme.fg_muted).child(status)))
            .when(self.host_access(cx).is_some_and(|(read_only, _)| read_only), |row| row.child("Read-only access"))
            .when(matches!(state, ConnState::Connecting | ConnState::WaitingToRetry { .. } | ConnState::Connected), |row| {
                row.child(Button::new("disconnect-remote-host").debug_selector(|| "remote-disconnect".into()).small().ghost()
                    .label(if state == ConnState::Connected { "Disconnect" } else { "Cancel connection" })
                    .on_click(cx.listener(|view, _, window, cx| { view.disconnect(cx); view.focus_active(window, cx); cx.notify(); })))
            })
    }

    pub(super) fn section(&self, title: &str, state: &resources::LoadState, list_empty: bool, empty: &str) -> gpui::Div {
        let d = self.density;
        div().flex().flex_col().items_start().gap(px(d.gap_inline)).py(px(d.pad_panel))
            .border_t_1().border_color(self.theme.border_inactive)
            .child(div().text_size(px(self.typography.t_label_caps)).font_weight(FontWeight::SEMIBOLD)
                .text_color(self.theme.fg_muted).child(title.to_uppercase()))
            .when(matches!(state, resources::LoadState::Loading), |row| row.child(self.hint("Loading…")))
            .when_some(match state { resources::LoadState::Failed(error) => Some(error.clone()), _ => None }, |row, error| {
                row.child(self.hint(&format!("Could not load {title}: {error}. Use Refresh resources to retry.")))
            })
            .when(matches!(state, resources::LoadState::Ready) && list_empty, |row| row.child(self.hint(empty)))
    }

    fn hint(&self, text: &str) -> gpui::Div {
        div().text_size(px(self.typography.t_body_sm)).text_color(self.theme.fg_muted).child(text.to_string())
    }

    fn resources(&self, cx: &mut Context<Self>) -> gpui::Div {
        let states = self.host_resource_states(cx);
        let projects = self.host_projects(cx);
        let sessions_list = self.host_sessions(cx);
        let access = self.host_access(cx);
        let creating = self.host_creating(cx);
        let mut list = div().flex().flex_col().p(px(self.density.pad_panel))
            .child(div().flex().child(Button::new("refresh-remote-resources").small().ghost().label("Refresh resources")
                .disabled(self.host_refreshing(cx))
                .tooltip("Reload projects, sessions, and terminals from this host")
                .on_click(cx.listener(|view, _, _, cx| view.refresh_resources(cx)))));
        let mut projects_section = self.section("Projects", &states[0], projects.is_empty(), "No projects on this host.");
        if matches!(states[0], resources::LoadState::Ready) {
            for (i, project) in projects.iter().enumerate() {
                let path = project.path.clone();
                let reason = match access {
                    None => "Waiting for the host to verify access",
                    Some((true, _)) => "This enrollment has read-only access",
                    Some((_, false)) => "Session creation is disabled on this host",
                    _ => "Create an agent session in this project",
                };
                projects_section = projects_section.child(div().flex().flex_col().items_start().gap(px(self.density.gap_inline))
                    .child(project.name.clone()).child(self.hint(&path))
                    .child(Button::new(("create-remote-session", i)).small().ghost().label("New agent")
                        .disabled(!access.is_some_and(|(_, can_create)| can_create) || creating)
                        .tooltip(reason)
                        .on_click(cx.listener(move |view, _, _, cx| view.create_session(path.clone(), cx)))));
            }
        }
        let mut sessions = self.section("Sessions", &states[1], sessions_list.is_empty(), if access.is_some_and(|(_, can_create)| can_create) { "No sessions yet. Create an agent from a project above." } else { "No sessions on this host." });
        if matches!(states[1], resources::LoadState::Ready) {
            for (i, session) in sessions_list.iter().enumerate() {
                let id = session.session_id.clone();
                let title = session.title.clone();
                sessions = sessions.child(Button::new(("open-remote-chat", i)).max_w(gpui::relative(1.0)).small().ghost()
                    .selected(self.active_terminal.is_none() && self.active_chat.as_ref() == Some(&id))
                    .label(title.clone()).text_align(gpui::TextAlign::Left).tooltip(title.clone())
                    .on_click(cx.listener(move |view, _, window, cx| view.open_chat(id.clone(), title.clone(), window, cx))));
            }
        }
        list = list.when(!access.is_some_and(|(read_only, _)| read_only), |list| list.child(projects_section))
            .child(sessions).child(self.terminal_list(cx));
        list
    }

    fn empty_content(&self, cx: &mut Context<Self>) -> gpui::Div {
        let online = self.host_session(cx).is_some();
        let busy = matches!(self.conn_state(cx), ConnState::Connecting | ConnState::WaitingToRetry { .. });
        let (heading, guidance) = if online {
            ("Open a remote workspace", "Select a session to open its chat, Git changes, and files, or select a terminal to attach.")
        } else if busy {
            ("Connecting to your host", "Your remote resources will appear when the connection is ready.")
        } else {
            ("Connect to a remote host", "Choose a saved host or pair a new host to view its projects, sessions, and terminals.")
        };
        div().flex_1().min_h_0().flex().items_center().justify_center().p(px(self.density.pad_tab))
            .child(div().max_w(px(self.density.scale(440.0))).w_full().flex().flex_col().gap(px(self.density.pad_tab))
                .child(div().text_size(px(self.typography.t_display)).font_weight(FontWeight::SEMIBOLD).child(heading))
                .child(div().text_color(self.theme.fg_muted).child(guidance))
                .when(!online && !busy && (self.selected.is_some() || self.hosts.entries.is_empty()), |body| body.child(Button::new("connect-empty-state").debug_selector(|| "remote-connect".into()).primary()
                    .label(if self.selected.is_some() { "Reconnect to host" } else { "Pair a new host…" })
                    .on_click(cx.listener(|view, _, window, cx| {
                        if let Some(host) = view.selected.clone() { view.connect(host, None, cx); view.focus_active(window, cx); }
                        else { view.show_pairing(window, cx); }
                    }))))
                .when(!online && !busy && self.selected.is_none() && !self.hosts.entries.is_empty(), |body| body.child(self.host_picker("empty-remote-host-picker", cx)))
                .when(online && !self.sidebar_open, |body| body.child(Button::new("show-remote-resources").label("Browse resources")
                    .on_click(cx.listener(|view, _, _, cx| { view.sidebar_open = true; cx.notify(); }))))
                .when(!online && !busy, |body| body.child(self.hint("On the host, run oximux pair-new and paste the one-time ticket here. For a desktop host, get a ticket from Settings → Remote."))))
    }

    fn pairing(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().id("remote-pairing-scroll").flex_1().min_h_0().overflow_y_scroll()
            .child(div().w_full().min_h(gpui::relative(1.0)).flex().items_center().justify_center().p(px(self.density.pad_tab))
            .child(div().w_full().max_w(px(self.density.scale(440.0))).flex().flex_col().gap(px(self.density.gap_inline))
                .child(div().text_size(px(self.typography.t_display)).font_weight(FontWeight::SEMIBOLD).child("Pair a remote host"))
                .child(self.hint("On the host, run oximux pair-new, or use Settings → Remote on a desktop host. Paste the one-time ticket below."))
                .child("Host name").child(Input::new(&self.name))
                .child("Pairing ticket or connection URL").child(Input::new(&self.ticket))
                .child(Button::new("show-remote-ticket").small().ghost().label(if self.show_ticket { "Hide ticket" } else { "Show ticket" })
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.show_ticket = !view.show_ticket;
                        view.ticket.update(cx, |input, cx| input.set_masked(!view.show_ticket, window, cx));
                        cx.notify();
                    })))
                .when_some(self.error.clone(), |body, error| body.child(self.error_banner(error, cx)))
                .child(div().flex().gap(px(self.density.gap_inline))
                    .child(Button::new("submit-remote-pairing").debug_selector(|| "remote-pair-submit".into()).primary().label("Connect")
                        .on_click(cx.listener(|view, _, window, cx| view.pair(window, cx))))
                    .child(Button::new("cancel-remote-pairing").ghost().label("Cancel")
                        .on_click(cx.listener(|view, _, window, cx| view.cancel_pairing(window, cx)))))))
    }

    fn tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut tabs = div().flex().items_center().gap(px(self.density.gap_inline));
        let mut chats: Vec<_> = self.chats.iter().collect();
        chats.sort_by_key(|(_, chat)| chat.order);
        for (i, (id, chat)) in chats.into_iter().enumerate() {
            let activate = id.clone();
            let close = id.clone();
            tabs = tabs.child(Button::new(("remote-chat-tab", i)).small().ghost().label(chat.title.clone())
                .selected(self.active_terminal.is_none() && self.active_chat.as_ref() == Some(id))
                .on_click(cx.listener(move |view, _, window, cx| view.activate_chat(activate.clone(), window, cx))))
                .child(Button::new(("close-remote-chat", i)).small().ghost().label("×").tooltip("Close chat tab")
                    .on_click(cx.listener(move |view, _, window, cx| view.close_chat(&close, window, cx))));
        }
        div().id("remote-tabs").flex().flex_none().h(px(self.density.h_tab)).overflow_x_scroll()
            .bg(self.theme.bg_panel).border_b_1().border_color(self.theme.border_inactive)
            .child(tabs).child(self.terminal_tab_bar(cx))
    }

    fn content_tools(&self, cx: &mut Context<Self>) -> gpui::Div {
        let chat = self.active_chat.as_ref().filter(|_| self.active_terminal.is_none()).and_then(|id| self.chats.get(id));
        let title = chat.map(|chat| chat.title.clone()).unwrap_or_else(|| "Select a session to view its Git changes and files.".into());
        div().flex().flex_none().items_center().gap(px(self.density.gap_inline)).p(px(self.density.pad_panel))
            .border_b_1().border_color(self.theme.border_inactive)
            .child(div().flex_1().min_w_0().truncate().text_color(self.theme.fg_muted).child(title))
            .when(chat.is_some(), |row| row
                .child(Button::new("toggle-remote-git").debug_selector(|| "remote-git-toggle".into()).flex_shrink_0().small().label("Git").selected(self.show_git)
                    .tooltip("Git changes for the selected session")
                    .on_click(cx.listener(|view, _, window, cx| { view.show_git = !view.show_git; view.show_files = false; view.focus_active(window, cx); cx.notify(); })))
                .child(Button::new("toggle-remote-files").debug_selector(|| "remote-files-toggle".into()).flex_shrink_0().small().label("Files").selected(self.show_files)
                    .tooltip("Files for the selected session")
                    .on_click(cx.listener(|view, _, window, cx| { view.show_files = !view.show_files; view.show_git = false; view.focus_active(window, cx); cx.notify(); }))))
    }

    /// Inline save/discard/cancel for closing a session whose file editor
    /// still holds unsaved host drafts — mirrors the reload confirm in the
    /// files panel.
    fn close_confirm(&self, id: &str, cx: &mut Context<Self>) -> gpui::Div {
        let dirty = self.chats.get(id).map(|chat| chat.files.read(cx).dirty_buffers(cx)).unwrap_or(0);
        let writable = self.host_session(cx).is_some() && self.host_access(cx).is_some_and(|(read_only, _)| !read_only);
        div().flex().flex_none().items_center().gap(px(self.density.gap_inline)).p(px(self.density.pad_panel))
            .border_b_1().border_color(self.theme.border_inactive)
            .child(Icon::default().path("icons/alert-triangle.svg").text_color(self.theme.status_error))
            .child(div().flex_1().min_w_0().child(format!("Close this session? {dirty} unsaved host-file draft{} will be lost.", if dirty == 1 { "" } else { "s" })))
            .child(Button::new("remote-close-save").small().label("Save all & close").disabled(!writable)
                .on_click(cx.listener(|view, _, _, cx| view.save_all_close(cx))))
            .child(Button::new("remote-close-discard").small().label("Discard & close")
                .on_click(cx.listener(|view, _, window, cx| view.discard_close(window, cx))))
            .child(Button::new("remote-close-cancel").small().ghost().label("Cancel")
                .on_click(cx.listener(|view, _, _, cx| { view.pending_close = None; cx.notify(); })))
    }

    fn error_banner(&self, error: String, cx: &mut Context<Self>) -> gpui::Div {
        div().flex().flex_none().items_center().gap(px(self.density.gap_inline)).p(px(self.density.pad_panel))
            .text_color(self.theme.status_error)
            .child(Icon::default().path("icons/alert-triangle.svg"))
            .child(div().flex_1().min_w_0().child(error))
            .child(Button::new("dismiss-remote-error").small().ghost().label("Dismiss")
                .on_click(cx.listener(|view, _, _, cx| { view.error = None; cx.notify(); })))
    }
}

impl Render for RemoteWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.restore_tabs(window, cx);
        if !self.show_pairing && (self.name.read(cx).focus_handle(cx).is_focused(window)
            || self.ticket.read(cx).focus_handle(cx).is_focused(window)) {
            self.focus_active(window, cx);
        }
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        for (id, tab) in &self.terminal_tabs {
            tab.view.update(cx, |view, cx| view.set_visible(!self.show_pairing && self.active_terminal.as_ref() == Some(id), cx));
        }
        let theme = self.theme;
        let d = self.density;
        let rail_width = d.scale(260.0).min(f32::from(window.viewport_size().width) * 0.38);
        let panel_only = window.viewport_size().width - px(if self.sidebar_open { rail_width } else { 0.0 }) < px(d.scale(900.0));
        let label = format!("Remote · {}", self.selected.as_ref().map(|host| host.name.as_str()).unwrap_or("No host selected"));
        let active_terminal = self.active_terminal.as_ref().and_then(|id| self.terminal_tabs.get(id)).map(|tab| tab.view.clone());
        let chat = self.active_chat.as_ref().filter(|_| active_terminal.is_none()).and_then(|id| self.chats.get(id));
        let git = chat.filter(|_| self.show_git).map(|chat| chat.git.clone());
        let files = chat.filter(|_| self.show_files).map(|chat| chat.files.clone());
        let active_chat = chat.map(|chat| chat.view.clone());
        if panel_only && (self.show_git || self.show_files)
            && active_chat.as_ref().is_some_and(|chat| chat.read(cx).focus_handle(cx).contains_focused(window, cx)) {
            self.focus.focus(window, cx);
        }
        let empty = active_chat.is_none() && active_terminal.is_none();
        div().size_full().track_focus(&self.focus).flex().flex_col().bg(theme.bg_base)
            .text_color(theme.fg_base).font(self.typography.ui_font()).text_size(px(self.typography.t_body_md))
            .on_action(cx.listener(|view, _: &crate::actions::ToggleLeftSidebar, _, cx| { view.sidebar_open = !view.sidebar_open; cx.notify(); }))
            .on_action(cx.listener(|view, _: &crate::actions::ToggleRightSidebar, window, cx| {
                if view.active_chat.is_some() && view.active_terminal.is_none() { view.show_git = !view.show_git; }
                view.show_files = false; view.focus_active(window, cx); cx.notify();
            }))
            .on_action(cx.listener(|view, _: &crate::actions::DismissOverlay, window, cx| {
                view.navigator_open = false;
                if view.show_pairing { view.cancel_pairing(window, cx); }
                else { view.focus_active(window, cx); cx.notify(); }
            }))
            .on_action(cx.listener(|view, _: &crate::actions::OpenQuickOpen, window, cx| {
                view.navigator_open = !view.navigator_open;
                if view.navigator_open { view.filter.read(cx).focus_handle(cx).focus(window, cx); }
                else { view.focus_active(window, cx); }
                cx.notify();
            }))
            .child(div().flex().flex_1().min_h_0()
                .when(self.sidebar_open, |body| body.child(div().flex().flex_col().flex_shrink_0().w(px(rail_width))
                    .bg(theme.bg_rail).border_r_1().border_color(theme.border_inactive)
                    .child(top_bar::remote_header(true, None, theme, d, &self.typography))
                    .child(div().id("remote-resource-sidebar").flex_1().min_h_0().overflow_y_scroll().flex().flex_col()
                        .child(self.connection_details(cx))
                        .when(self.host_session(cx).is_some(), |rail| rail.child(self.resources(cx)))
                        .when(self.host_session(cx).is_none(), |rail| rail.child(div().p(px(d.pad_panel)).child(self.hint("Connect to view remote resources.")))))
                    .child(div().flex().flex_col().items_start().p(px(d.pad_panel)).border_t_1().border_color(theme.border_inactive)
                        .child(Button::new("remote-back-local").small().ghost().label("Back to local")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(crate::actions::SelectLocalHost), cx)))
                        .child(Button::new("remote-settings").small().ghost().label("Settings…")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(crate::actions::OpenSettings), cx))))))
                .child(div().flex().flex_col().flex_1().min_w_0().bg(theme.bg_panel)
                    .child(div().flex().flex_none().items_center()
                        .child(div().flex_1().min_w_0().child(top_bar::remote_header(self.sidebar_open,
                            Some(top_bar::command_center(Some("Search remote sessions and terminals".into()), theme, d, &self.typography).into_any_element()), theme, d, &self.typography)))
                        .when(cfg!(windows), |header| header.child(crate::shell::chrome::window_controls::WindowsWindowControls::new(theme))))
                    .when(!self.sidebar_open, |body| body.child(self.connection_details(cx)))
                    .when_some(self.display_error(cx).filter(|_| !self.show_pairing), |body, error| {
                        body.child(self.error_banner(error, cx))
                    })
                    .when_some(match self.conn_state(cx) {
                        ConnState::Unreachable { cause } => Some(format!("{cause}. Check the host and network. If access was revoked, pair again with a fresh ticket.")),
                        _ => None,
                    }, |body, message| body.child(div().flex_none().p(px(d.pad_panel)).text_color(theme.status_error).child(message)))
                    .when_some(self.pending_close.clone().filter(|_| !self.show_pairing), |body, id| body.child(self.close_confirm(&id, cx)))
                    .when(!self.show_pairing && !empty, |body| body.child(self.tab_bar(cx)))
                    .when(!self.show_pairing && self.host_session(cx).is_some() && active_terminal.is_none(), |body| body.child(self.content_tools(cx)))
                    .when(self.navigator_open, |body| body.child(self.navigator(window, cx)))
                    .when(self.show_pairing, |body| body.child(self.pairing(cx)))
                    .when(!self.show_pairing, |body| body
                        .when(empty, |body| body.child(self.empty_content(cx)))
                        .when(!empty, |body| body.child(div().flex().flex_1().min_h_0().min_w_0()
                            .when_some(active_chat.filter(|_| !(panel_only && (self.show_git || self.show_files))), |body, chat| body.child(div().flex_1().min_w_0().h_full().child(chat)))
                            .when_some(active_terminal, |body, terminal| body.child(div().flex_1().min_w_0().h_full().child(terminal)))
                            .when_some(git, |body, git| body.child(div().flex_1().min_w_0().h_full().overflow_hidden().child(git)))
                            .when_some(files, |body, files| body.child(div().flex_1().min_w_0().h_full().overflow_hidden().child(files))))))))
            .child(div().h(px(d.h_status_bar)).flex_none().flex().items_center().gap(px(d.pad_tab)).px(px(d.pad_panel))
                .bg(theme.bg_panel).border_t_1().border_color(theme.border_inactive)
                .text_size(px(self.typography.t_body_sm)).text_color(theme.fg_muted).child(div().min_w_0().truncate().child(label)))
    }
}
