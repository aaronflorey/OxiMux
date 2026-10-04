//! The command center searches only resources advertised by the remote host.
use super::*;
use gpui_component::Sizable;

impl RemoteWorkspace {
    pub(super) fn navigator(&self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let query = self.filter.read(cx).value().to_lowercase();
        let mut results = div().id("remote-search-results").max_h(px(self.density.scale(240.0))).overflow_y_scroll().flex().flex_col();
        let mut count = 0;
        if self.session.is_some() {
            for (i, session) in self.sessions.iter().enumerate().filter(|(_, session)| session.title.to_lowercase().contains(&query)) {
                let id = session.session_id.clone();
                let title = session.title.clone();
                count += 1;
                results = results.child(Button::new(("remote-search-chat", i)).debug_selector(move || format!("remote-search-chat-{i}")).small().ghost().label(format!("Session · {title}"))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.navigator_open = false;
                        view.open_chat(id.clone(), title.clone(), window, cx);
                    })));
            }
            for (i, terminal) in self.terminals.iter().enumerate() {
                let title = if terminal.cwd.is_empty() { terminal.pty_id.clone() } else { terminal.cwd.clone() };
                if !title.to_lowercase().contains(&query) { continue; }
                let id = terminal.pty_id.clone();
                count += 1;
                results = results.child(Button::new(("remote-search-terminal", i)).small().ghost().label(format!("Terminal · {title}"))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.navigator_open = false;
                        view.open_terminal(id.clone(), title.clone(), window, cx);
                    })));
            }
        }
        div().flex_none().flex().flex_col().gap(px(self.density.gap_inline)).p(px(self.density.pad_panel))
            .border_b_1().border_color(self.theme.border_inactive)
            .child(div().flex().items_center().gap(px(self.density.gap_inline))
                .child(div().flex_1().min_w_0().child(Input::new(&self.filter)))
                .child(Button::new("close-remote-search").small().ghost().label("Close")
                    .on_click(cx.listener(|view, _, window, cx| { view.navigator_open = false; view.focus_active(window, cx); cx.notify(); }))))
            .child(results)
            .when(count == 0, |body| body.child(div().text_color(self.theme.fg_muted)
                .child(if self.session.is_some() { "No matching sessions or terminals." } else { "Connect to a host to search its sessions and terminals." })))
    }
}
