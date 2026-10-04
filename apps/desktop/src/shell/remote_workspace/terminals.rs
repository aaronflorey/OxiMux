//! Remote terminal tabs reuse the desktop canvas and keystroke encoder.
use super::*;
use gpui_component::{Selectable, Sizable, button::{Button, ButtonVariants}};
use oximux_agents::SharedBackend;
use oximux_pty::remote_backend::{RemoteTerminalBackend, RemoteTerminalControl, REMOTE_SESSION};
use crate::shell::{terminal_view::TerminalView, context_env::SurfaceIds};

pub(super) struct TerminalTab {
    pub view: Entity<TerminalView>,
    pub control: RemoteTerminalControl,
    pub title: String,
}

impl RemoteWorkspace {
    pub(super) fn bind_terminals(&mut self) {
        for (id, tab) in &self.terminal_tabs {
            let sender = self.terminal_driver.as_ref().map(|driver| terminal_driver::sender(driver.tx.clone(), id.clone()));
            tab.control.bind(sender, self.access.is_some_and(|(read_only, _)| !read_only));
        }
    }

    pub(super) fn open_terminal(&mut self, id: String, title: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.terminal_tabs.contains_key(&id) {
            let (backend, control) = RemoteTerminalBackend::new();
            let backend: SharedBackend = Arc::new(std::sync::Mutex::new(Box::new(backend)));
            let host = self.selected.as_ref().map(|h| h.endpoint_id.as_str()).unwrap_or("unselected");
            let ids = SurfaceIds::fresh(format!("remote:{host}"));
            let view = cx.new(|cx| TerminalView::mount(backend, REMOTE_SESSION, ids, self.theme,
                self.density, self.typography.clone(), window, cx));
            if let Some(driver) = &self.terminal_driver {
                let feed = control.bind(Some(terminal_driver::sender(driver.tx.clone(), id.clone())),
                    self.access.is_some_and(|(read_only, _)| !read_only));
                let _ = driver.tx.send(terminal_driver::Command::Open(id.clone(), feed));
            }
            self.terminal_tabs.insert(id.clone(), TerminalTab { view, control, title });
        }
        self.terminal_tabs[&id].view.read(cx).focus_handle(cx).focus(window, cx);
        self.active_terminal = Some(id);
        self.show_git = false;
        self.show_files = false;
        cx.notify();
    }

    pub(super) fn close_terminal(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.terminal_tabs.remove(id) {
            // Explicit close releases the attachment immediately, independent of
            // GPUI entity retention; TerminalView's later Drop is idempotent.
            tab.view.read(cx).detach();
        }
        if self.active_terminal.as_deref() == Some(id) {
            self.active_terminal = None;
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    pub(super) fn terminal_list(&self, cx: &mut Context<Self>) -> gpui::Div {
        let mut list = self.section("Terminals", 2, "No terminals on this host. Start one on the host to attach here.");
        if !matches!(self.resource_states[2], resources::LoadState::Ready) { return list; }
        for (i, terminal) in self.terminals.iter().enumerate() {
            let id = terminal.pty_id.clone();
            let title = if terminal.cwd.is_empty() { terminal.pty_id.clone() } else { terminal.cwd.clone() };
            list = list.child(Button::new(("attach-remote-terminal", i)).max_w(gpui::relative(1.0)).label(title.clone()).ghost().small()
                .selected(self.active_terminal.as_ref() == Some(&id)).tooltip("Attach this terminal in the workspace")
                .disabled(self.session.is_none())
                .on_click(cx.listener(move |view, _, window, cx| view.open_terminal(id.clone(), title.clone(), window, cx))));
        }
        list
    }

    pub(super) fn terminal_tab_bar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let mut tabs = div().flex().items_center().gap(px(self.density.gap_inline));
        for (i, (id, tab)) in self.terminal_tabs.iter().enumerate() {
            let activate = id.clone();
            let close = id.clone();
            tabs = tabs.child(Button::new(("remote-terminal-tab", i)).label(tab.title.clone()).ghost().small().selected(self.active_terminal.as_ref() == Some(id))
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.active_terminal = Some(activate.clone());
                    view.show_git = false;
                    view.show_files = false;
                    if let Some(tab) = view.terminal_tabs.get(&activate) { tab.view.read(cx).focus_handle(cx).focus(window, cx); }
                    cx.notify();
                })))
                .child(Button::new(("close-remote-terminal", i)).label("×").ghost().small().tooltip("Detach terminal tab")
                    .on_click(cx.listener(move |view, _, window, cx| view.close_terminal(&close, window, cx))));
        }
        tabs
    }
}
