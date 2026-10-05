//! Remote ownership on the existing chat renderer/composer seam.
use super::*;
use oximux_remote_proto::proto::SessionChoices;
use oximux_agents::thread::AskQuestion;
use oximux_remote_session::RemoteSession;
use gpui_component::{Sizable, button::ButtonVariants};
#[path = "outbound_connection.rs"]
mod connection;
use connection::RemoteAgentConnection;

pub(super) struct OutboundChat {
    id: String,
    session: Option<Arc<RemoteSession>>,
    choices: SessionChoices,
    read_only: Option<bool>,
    busy: bool,
    supports_steer: bool,
    last_seq: Option<u64>,
    refresh: Option<Arc<dyn Fn() + Send + Sync>>,
    revision: u64,
    error: Option<String>,
    failed_prompt: Option<(String, Vec<ChatImage>)>,
    pending_prompt: Option<(String, Vec<ChatImage>)>,
    // NEVER abort on view drop: the task holds an in-flight `Demux::call`,
    // and dropping its `RpcGuard` marks the whole correlation-free
    // connection dead — severing every sibling view on the host. Dropping
    // the handle just detaches it; the RPC finishes, `tx.send` fails, and
    // the file/Git panels' spawn-and-forget lifetime rule holds here too.
    rpc_task: Option<tokio::task::JoinHandle<()>>,
    waiter: Option<gpui::Task<()>>,
}

type Reply = (Result<(), String>, Result<oximux_remote_session::SessionSubscription, String>, Option<SessionChoices>, Option<(bool, bool)>);

enum Command {
    Send(String, Vec<ChatImage>), Stop, Model(String), Mode(String), Steer(String),
    Permission(String, PermissionDecision), Question(String, Vec<AskQuestion>, QuestionAnswers),
}
impl Command {
    fn run(self, connection: &dyn AgentConnection) -> anyhow::Result<()> {
        match self {
            Self::Send(text, images) => connection.send_user_message_with_images(&text, &images),
            Self::Stop => connection.cancel(), Self::Model(model) => connection.set_model(&model),
            Self::Mode(mode) => connection.set_mode(&mode), Self::Steer(text) => connection.steer(&text),
            Self::Permission(id, decision) => connection.resolve_permission(&id, decision),
            Self::Question(id, questions, answers) => connection.answer_question(&id, &questions, &answers),
        }
    }
}

impl AgentChatView {
    pub(crate) fn new_remote(id: String, theme: Theme, density: Density, typography: Typography,
        window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self::assemble(PathBuf::new(), None, ChatBackend::stream_json(), ChatThread::new(),
            ConnectMode::Remote, RestoredPosture::default(), theme, density, typography, window, cx);
        view.outbound = Some(OutboundChat { id, session: None, choices: SessionChoices {
            models: vec![], modes: vec![], current_model: None, current_mode: None },
            read_only: None, busy: false, supports_steer: false, last_seq: None, refresh: None, revision: 0, error: None, failed_prompt: None, pending_prompt: None, rpc_task: None, waiter: None });
        view.composer.update(cx, |c, cx| c.set_remote_view(cx));
        view.sync_outbound_composer(cx);
        view
    }

    /// The host session id a remote-bound chat serves (`outbound.id`) —
    /// distinct from `remote_session_id()`, which names the view to
    /// remote-control clients and stays a placeholder for host-bound views.
    /// `None` on local chats.
    pub(crate) fn outbound_session_id(&self) -> Option<&str> {
        self.outbound.as_ref().map(|o| o.id.as_str())
    }

    pub(crate) fn set_remote_connection(&mut self, session: Option<Arc<RemoteSession>>, read_only: Option<bool>, cx: &mut Context<Self>) {
        let remote = self.outbound.as_mut().expect("outbound view");
        remote.revision += 1;
        if let Some(prompt) = remote.pending_prompt.take() {
            remote.failed_prompt = Some(prompt);
            remote.error = Some("Connection changed before the prompt was acknowledged. Check the server transcript before retrying.".into());
        } else if remote.busy {
            remote.error = Some("Connection changed before the operation completed. Refresh the server state before retrying.".into());
        }
        // The task still owns the OLD session — a different connection —
        // so aborting can only sever a socket already being discarded.
        if let Some(task) = remote.rpc_task.take() { task.abort(); }
        remote.waiter = None;
        remote.busy = false;
        remote.last_seq = None;
        remote.session = session;
        remote.read_only = read_only;
        self.sync_outbound_composer(cx);
        cx.notify();
    }

    pub(crate) fn set_remote_refresh(&mut self, refresh: Option<Arc<dyn Fn() + Send + Sync>>) {
        self.outbound.as_mut().unwrap().refresh = refresh;
    }
    pub(crate) fn set_remote_access(&mut self, read_only: bool, cx: &mut Context<Self>) {
        self.outbound.as_mut().unwrap().read_only = Some(read_only);
        self.sync_outbound_composer(cx);
        cx.notify();
    }
    pub(crate) fn update_remote_thread(&mut self, seq: u64, thread: ChatThread, supports_steer: bool, choices: Option<SessionChoices>, cx: &mut Context<Self>) {
        let remote = self.outbound.as_mut().unwrap();
        if let Some(choices) = choices { remote.choices = choices; }
        // Decisions and model/mode metadata can change without a new event.
        if remote.last_seq.is_some_and(|last| seq < last) { self.sync_outbound_composer(cx); return; }
        remote.last_seq = Some(seq);
        remote.supports_steer = supports_steer;
        self.model = thread.model.clone();
        self.permission_mode = thread.permission_mode.clone();
        self.thread = thread;
        self.sync_outbound_composer(cx);
        self.follow_frames = FOLLOW_FRAMES;
        // An idle snapshot releases the next prompt the composer parked while
        // the turn streamed — the local path's queued drain, replayed through
        // the host RPC. One at a time: the send flips `busy`, which gates the
        // drain on every following snapshot until it finishes.
        if !self.thread.turn_active {
            self.flush_remote_queued(cx);
        }
        cx.notify();
    }
    /// Drain one parked composer message into a host prompt. A command already
    /// in flight, a Stop the user just issued, or losing write/connectivity
    /// keeps the chips parked — none of those may fire or silently drop it.
    fn flush_remote_queued(&mut self, cx: &mut Context<Self>) {
        if self.interrupted || !self.outbound_mutations_enabled() { return; }
        if let Some((text, images)) = self.composer.update(cx, |c, cx| c.take_next_queued(cx)) {
            self.outbound_command(Command::Send(text, images), cx);
        }
    }
    pub(crate) fn remote_error(&mut self, error: String, cx: &mut Context<Self>) {
        let remote = self.outbound.as_mut().unwrap();
        remote.error = Some(error);
        remote.last_seq = None;
        self.sync_outbound_composer(cx);
        cx.notify();
    }
    pub(super) fn outbound_mutations_enabled(&self) -> bool {
        self.outbound.as_ref().is_none_or(|r| r.session.is_some() && r.last_seq.is_some() && r.read_only == Some(false) && !r.busy)
    }
    pub(super) fn sync_outbound_composer(&self, cx: &mut Context<Self>) {
        let remote = self.outbound.as_ref().unwrap();
        let enabled = self.outbound_mutations_enabled();
        let vocab = ControlVocab {
            models: if enabled { remote.choices.models.iter().map(|c| oximux_agents::thread::ModelChoice {
                wire: c.id.clone(), label: c.label.clone(), description: c.description.clone() }).collect() } else { vec![] },
            permission_modes: if enabled { remote.choices.modes.iter().map(|c| oximux_agents::thread::ModeChoice {
                wire: c.id.clone(), label: c.label.clone() }).collect() } else { vec![] },
            default_model: remote.choices.current_model.clone(), default_mode: remote.choices.current_mode.clone(),
            ..Default::default()
        };
        self.composer.update(cx, |c, cx| {
            c.set_state(!enabled, enabled && self.thread.turn_active, cx);
            c.set_can_steer(enabled && remote.supports_steer, cx);
            c.set_controls(self.model.clone(), self.permission_mode.clone(), None, enabled && !remote.choices.modes.is_empty(), false, vocab, cx);
            c.set_provider_label("remote agent".into(), cx);
            c.set_slash_commands(if enabled { self.thread.slash_commands.clone() } else { vec![] },
                self.thread.slash_command_descriptions.clone(), self.thread.slash_command_hints.clone(), cx);
        });
    }
    pub(super) fn outbound_send(&mut self, text: String, images: Vec<ChatImage>, cx: &mut Context<Self>) {
        self.outbound_command(Command::Send(text, images), cx);
    }
    pub(super) fn outbound_stop(&mut self, cx: &mut Context<Self>) { self.outbound_command(Command::Stop, cx); }
    pub(super) fn render_unavailable_remote_request(&self, tc: &ToolCall) -> AnyElement {
        let description = match &tc.status {
            ToolCallStatus::WaitingForConfirmation(r) => r.description.clone(),
            ToolCallStatus::AwaitingAnswer(r) => r.questions.iter().map(|q| q.question.clone()).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        };
        div().p(px(self.density.pad_panel)).flex().flex_col().child(tc.name.clone()).child(description)
            .child("Answering is unavailable while read-only, disconnected, or awaiting the server.").into_any_element()
    }
    pub(super) fn outbound_composer_event(&mut self, event: &ComposerEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            ComposerEvent::Submit { text, images } => self.outbound_command(Command::Send(text.clone(), images.clone()), cx),
            ComposerEvent::Stop => self.outbound_command(Command::Stop, cx),
            ComposerEvent::ModelPicked(model) => self.outbound_command(Command::Model(model.clone()), cx),
            ComposerEvent::PermissionModePicked(mode) => self.outbound_command(Command::Mode(mode.clone()), cx),
            ComposerEvent::SteerNow { text } => self.outbound_command(Command::Steer(text.clone()), cx),
            ComposerEvent::ThinkingDisplayPicked(wire) => self.set_thinking_display_level(wire, cx),
            // These paths were explicitly chosen on this laptop's image picker.
            ComposerEvent::PathsPicked(paths) => self.attach_paths(paths.clone(), window, cx),
            _ => (),
        }
    }
    pub(super) fn outbound_permission(&mut self, tool_id: String, request_id: String, decision: PermissionDecision, cx: &mut Context<Self>) {
        if self.thread.entries.iter().any(|e| matches!(e, ThreadEntry::ToolCall(tc) if tc.id == tool_id
            && matches!(&tc.status, ToolCallStatus::WaitingForConfirmation(r) if r.request_id == request_id))) {
            self.outbound_command(Command::Permission(request_id, decision), cx);
        }
    }
    pub(super) fn outbound_answer(&mut self, tool_id: String, answers: QuestionAnswers, cx: &mut Context<Self>) {
        let request = self.thread.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == tool_id => match &tc.status {
                ToolCallStatus::AwaitingAnswer(r) => Some((r.request_id.clone(), r.questions.clone())), _ => None }, _ => None });
        if let Some((id, questions)) = request { self.outbound_command(Command::Question(id, questions, answers), cx); }
    }
    pub(super) fn outbound_retry(&mut self, cx: &mut Context<Self>) {
        if let Some((text, images)) = self.thread.entries.iter().rev().find_map(|e| match e {
            ThreadEntry::User { text, images, .. } => Some((text.clone(), images.clone())), _ => None }) {
            self.outbound_command(Command::Send(text, images), cx);
        }
    }
    fn outbound_command(&mut self, command: Command, cx: &mut Context<Self>) {
        if !self.outbound_mutations_enabled() { return; }
        // Mirror the local `interrupted` flag: an issued Stop parks the queued
        // chips until the user's next send, which is a new turn's intent.
        match &command {
            Command::Stop => self.interrupted = true,
            Command::Send(..) => self.interrupted = false,
            _ => (),
        }
        let remote = self.outbound.as_mut().unwrap();
        let failed_prompt = match &command { Command::Send(text, images) => Some((text.clone(), images.clone())), _ => None };
        let session = remote.session.clone().unwrap();
        let id = remote.id.clone();
        let connection = RemoteAgentConnection { session: session.clone(), id: id.clone(), choices: remote.choices.clone(),
            read_only: remote.read_only != Some(false), supports_steer: remote.supports_steer, runtime: tokio::runtime::Handle::current() };
        remote.busy = true;
        remote.pending_prompt = failed_prompt.clone();
        remote.error = None;
        let revision = remote.revision;
        let (tx, rx) = tokio::sync::oneshot::channel();
        remote.rpc_task = Some(tokio::spawn(async move {
            let outcome = tokio::time::timeout(Duration::from_secs(45), async {
            let result = tokio::task::spawn_blocking(move || command.run(&connection)).await
                .map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| e.to_string()));
            let snapshot = session.fetch_chat_state(&id).await.map_err(|e| e.to_string());
            let choices = session.list_choices(&id).await.ok();
            let access = session.client_access().await.ok();
            (result, snapshot, choices, access)
            }).await.unwrap_or_else(|_| (Err("Remote command timed out; check the server transcript before retrying".into()),
                Err("Host did not provide a fresh snapshot".into()), None, None));
            let _ = tx.send(outcome);
        }));
        remote.waiter = Some(cx.spawn(async move |view, cx| {
            if let Ok((result, snapshot, choices, access)) = rx.await {
                let _ = view.update(cx, |view, cx| {
                    view.finish_outbound_rpc(revision, failed_prompt, (result, snapshot, choices, access), cx);
                });
            }
        }));
        self.sync_outbound_composer(cx);
        cx.notify();
    }
    fn finish_outbound_rpc(&mut self, revision: u64, failed_prompt: Option<(String, Vec<ChatImage>)>, reply: Reply, cx: &mut Context<Self>) {
        let (result, snapshot, choices, access) = reply;
        let remote = self.outbound.as_mut().unwrap();
        if remote.revision != revision { return; }
        remote.busy = false;
        remote.pending_prompt = None;
        remote.rpc_task = None;
        if let Some((read_only, _)) = access { remote.read_only = Some(read_only); }
        match result {
            Err(error) => { remote.error = Some(error); if failed_prompt.is_some() { remote.failed_prompt = failed_prompt; } }
            Ok(()) => { if failed_prompt.is_some() { remote.failed_prompt = None; } }
        }
        match snapshot {
            Ok(snapshot) => {
                let remote = self.outbound.as_mut().unwrap();
                remote.last_seq = None;
                if let Some(refresh) = &remote.refresh { refresh(); }
                self.update_remote_thread(snapshot.last_seq(), snapshot.thread().clone(), snapshot.supports_steer(), choices, cx);
            }
            Err(error) => {
                let remote = self.outbound.as_mut().unwrap();
                remote.last_seq = None;
                remote.error.get_or_insert_with(|| format!("Could not refresh server state: {error}"));
                self.sync_outbound_composer(cx);
                cx.notify();
            }
        }
    }
    pub(super) fn render_outbound(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.reconcile_question_cards(window, cx);
        let read_only = self.outbound.as_ref().unwrap().read_only == Some(true);
        if read_only && self.composer.read(cx).focus_handle(cx).is_focused(window) {
            self.focus_handle.focus(window, cx);
        }
        if !read_only && self.focus_handle.is_focused(window) {
            let composer = self.composer.clone();
            window.defer(cx, move |window, cx| composer.read(cx).focus_handle(cx).focus(window, cx));
        }
        self.settle_follow_spring(window, cx);
        self.settle_legacy_follow(window, cx);
        self.settle_pending_reveal(window, cx);
        self.markdown.retain_entries(self.thread.entries.len());
        self.markdown.dispatch_highlighting(cx);
        let remote = self.outbound.as_ref().unwrap();
        let status = if remote.session.is_none() { "Disconnected — waiting for host" } else if remote.busy { "Waiting for server…" }
            else if remote.last_seq.is_none() { "Waiting for server transcript" }
            else if remote.read_only == Some(true) { "Read-only enrollment" }
            else if remote.read_only.is_none() { "Verifying host access" } else { "Remote session" };
        div().size_full().track_focus(&self.focus_handle).flex().flex_col().bg(self.theme.bg_base).text_color(self.theme.fg_base)
            .child(div().flex_none().p(px(self.density.pad_panel))
                .child(div().flex().items_center().gap(px(self.density.gap_inline))
                    .child(div().flex_1().min_w_0().child(status))
                    .when(remote.session.is_some() && !remote.busy, |row| row.child(gpui_component::button::Button::new("refresh-remote-chat")
                        .small().ghost().label("Refresh").on_click(cx.listener(|view, _, _, _| {
                            if let Some(refresh) = &view.outbound.as_ref().unwrap().refresh { refresh(); }
                        })))))
                .when_some(remote.error.clone(), |row, error| row.child(div().text_color(self.theme.status_error).child(error)))
                .when(remote.failed_prompt.is_some(), |row| row.child(gpui_component::button::Button::new("restore-remote-prompt")
                    .label("Restore prompt").on_click(cx.listener(|view, _, window, cx| {
                        if let Some((text, images)) = view.outbound.as_mut().unwrap().failed_prompt.take() {
                            view.composer.update(cx, |c, cx| c.prefill(text, images, window, cx));
                        }
                    })))))
            .child(self.render_transcript(cx))
            .when(remote.read_only == Some(true), |body| body.child(div().id("remote-read-only-composer").debug_selector(|| "remote-read-only-composer".into()).flex_none().p(px(self.density.pad_panel))
                .border_t_1().border_color(self.theme.border_inactive).text_color(self.theme.fg_muted)
                .child("Read-only enrollment — messages and agent controls are unavailable.")))
            .when(remote.read_only != Some(true), |body| body.child(self.composer.clone()))
            .children(self.render_image_preview(cx)).children(self.render_tool_sheet(cx)).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// The reviewer's regression: a prompt submitted while the remote turn is
    /// still streaming parks in the composer; the first idle snapshot must
    /// issue exactly one send. Busy, a stale snapshot, a user Stop, and losing
    /// access all keep the chip parked rather than firing or dropping it.
    /// (The RPC task is registered but never polled — a gpui test cannot drive
    /// a real transport — so `rpc_task.is_some()` marks an issued send and the
    /// send itself is covered by the loopback test in `outbound_connection`.)
    #[gpui::test]
    fn queued_remote_prompt_drains_through_one_send_per_idle_snapshot(cx: &mut TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let _entered = runtime.enter();
        cx.update(gpui_component::init);
        let (transport, _host) = oximux_remote_proto::testing::duplex_pair();
        let session = std::sync::Arc::new(RemoteSession::new(std::sync::Arc::new(transport),
            oximux_remote_session::ClientSigner::from_seed(&[3; 32])));
        let window = cx.add_window(|window, cx| AgentChatView::new_remote("live".into(), Theme::default(),
            Density::default(), Typography::default(), window, cx));
        window.update(cx, |view, window, cx| {
            view.set_remote_connection(Some(session), Some(false), cx);
            let mut active = ChatThread::new();
            active.turn_active = true;
            view.update_remote_thread(1, active.clone(), false, None, cx);
            // Submitting while the turn streams parks the message, sends nothing.
            view.composer.update(cx, |c, cx| {
                c.set_draft_for_test("second prompt", window, cx);
                c.submit(window, cx);
            });
            assert_eq!(view.composer.read(cx).queued_texts(), vec!["second prompt".to_string()]);
            assert!(view.outbound.as_ref().unwrap().rpc_task.is_none());

            // The turn ending issues exactly one send; the chip leaves the queue.
            view.update_remote_thread(2, ChatThread::new(), false, None, cx);
            assert!(view.composer.read(cx).queued_texts().is_empty());
            assert!(view.outbound.as_ref().unwrap().rpc_task.is_some(), "an idle snapshot drains the parked prompt");

            // While that send is in flight a further idle snapshot cannot
            // double-fire the queue.
            view.update_remote_thread(3, ChatThread::new(), false, None, cx);
            view.composer.update(cx, |c, cx| {
                c.set_draft_for_test("third prompt", window, cx);
                c.submit(window, cx);
            });
            assert_eq!(view.composer.read(cx).queued_texts(), Vec::<String>::new(),
                "a busy command leaves the draft in the box, not on the wire");
            // Settle the in-flight send, then park the next message as if the
            // turn were still streaming.
            {
                let remote = view.outbound.as_mut().unwrap();
                remote.busy = false;
                remote.rpc_task = None;
            }
            let mut active = ChatThread::new();
            active.turn_active = true;
            view.thread = active;
            view.sync_outbound_composer(cx);
            view.composer.update(cx, |c, cx| c.submit(window, cx));
            assert_eq!(view.composer.read(cx).queued_texts(), vec!["third prompt".to_string()]);

            // A stale snapshot below the accepted seq must not drain.
            view.update_remote_thread(2, ChatThread::new(), false, None, cx);
            assert_eq!(view.composer.read(cx).queued_texts(), vec!["third prompt".to_string()]);
            // Read-only enrollment: parked, not sent.
            view.set_remote_access(true, cx);
            view.update_remote_thread(5, ChatThread::new(), false, None, cx);
            assert_eq!(view.composer.read(cx).queued_texts(), vec!["third prompt".to_string()]);
            assert!(view.outbound.as_ref().unwrap().rpc_task.is_none());
            view.set_remote_access(false, cx);
            // A user Stop keeps the queue parked even after the turn idles.
            view.thread.turn_active = true;
            view.sync_outbound_composer(cx);
            view.outbound_stop(cx);
            assert!(view.interrupted);
            {
                let remote = view.outbound.as_mut().unwrap();
                remote.busy = false;
                remote.rpc_task = None;
            }
            view.update_remote_thread(6, ChatThread::new(), false, None, cx);
            assert_eq!(view.composer.read(cx).queued_texts(), vec!["third prompt".to_string()],
                "a stopped turn leaves queued prompts parked");
            assert!(view.outbound.as_ref().unwrap().rpc_task.is_none(), "no send fires after Stop");
            // The user's next send is a fresh turn's intent — it clears the flag.
            view.outbound_send("user retry".into(), vec![], cx);
            assert!(!view.interrupted);
            assert!(view.outbound.as_ref().unwrap().rpc_task.is_some());
        }).unwrap();
    }

    /// Closing the chat must not abort its in-flight command: the spawned
    /// task owns an `RpcGuard` on the shared demux connection, so dropping
    /// the view detaches the handle and lets the reply land. If the task
    /// were aborted its future's `Drop` would run — flagged here — and the
    /// connection would die under every sibling view.
    #[gpui::test]
    fn dropped_outbound_chat_detaches_its_in_flight_rpc(_cx: &mut TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let _entered = runtime.enter();
        struct Flag(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) { self.0.store(true, std::sync::atomic::Ordering::SeqCst) }
        }
        let aborted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Flag(aborted.clone());
        let task = tokio::spawn(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        });
        {
            let _chat = OutboundChat { id: "s".into(), session: None, choices: SessionChoices {
                models: vec![], modes: vec![], current_model: None, current_mode: None },
                read_only: None, busy: true, supports_steer: false, last_seq: None, refresh: None,
                revision: 0, error: None, failed_prompt: None, pending_prompt: None,
                rpc_task: Some(task), waiter: None };
        }
        assert!(!aborted.load(std::sync::atomic::Ordering::SeqCst),
            "a closing view must leave the outstanding RPC running");
    }

    #[gpui::test]
    fn read_only_remote_chat_replaces_the_composer_and_releases_its_focus(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| AgentChatView::new_remote("s".into(), Theme::default(),
            Density::default(), Typography::default(), window, cx));
        window.update(cx, |view, window, cx| {
            view.composer.read(cx).focus_handle(cx).focus(window, cx);
            view.set_remote_access(true, cx);
        }).unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        cx.refresh().unwrap();
        assert!(visual.debug_bounds("remote-read-only-composer").is_some(), "read-only restrictions must be visible beside the transcript");
        window.update(cx, |view, window, cx| {
            assert!(view.focus_handle.is_focused(window), "focus must move off the unmounted input");
            assert!(!view.outbound_mutations_enabled());
            view.send_text("blocked".into(), vec![], cx);
            assert!(view.thread.entries.is_empty());
        }).unwrap();
    }

    #[gpui::test]
    fn remote_constructor_is_isolated_and_read_only_without_a_snapshot(cx: &mut TestAppContext) {
        cx.update(|cx| cx.set_global(gpui_component::Theme::default()));
        let window = cx.add_window(|window, cx| AgentChatView::new_remote("server-id".into(), Theme::default(),
            Density::default(), Typography::default(), window, cx));
        window.update(cx, |view, _, cx| {
            assert!(view.connection.is_none());
            assert!(view.remote.is_none());
            assert!(view.checkpoint_engine.is_none());
            assert!(!view.is_git_project, "the local checkout is never probed for remote views");
            assert!(!view.dormant && !view.unbound);
            assert!(view.cwd.as_os_str().is_empty());
            let mut thread = ChatThread::new();
            thread.session_id = Some("server-id".into());
            thread.session_meta.cwd = Some("/server/only".into());
            thread.push_user_message("server-owned history");
            view.update_remote_thread(5, thread, false, None, cx);
            assert!(view.transcript_snapshot().is_none(), "remote history never enters local persistence");
            assert!(!view.outbound_mutations_enabled());
            view.send_text("blocked".into(), vec![], cx);
            assert_eq!(view.thread.entries.len(), 1);
            assert!(view.outbound.as_ref().unwrap().rpc_task.is_none());
            view.ensure_connected(true, cx);
            assert!(view.connection.is_none(), "restore cannot spawn a local agent");
            view.update_remote_thread(4, ChatThread::new(), false, None, cx);
            assert_eq!(view.thread.entries.len(), 1, "stale snapshot ignored");
            let remote = view.outbound.as_mut().unwrap();
            remote.busy = true;
            let revision = remote.revision;
            let thread = view.thread.clone();
            view.finish_outbound_rpc(revision, Some(("refused prompt".into(), vec![])),
                (Err("host refused".into()), Ok(oximux_remote_session::SessionSubscription::from_snapshot("server-id", thread, 5)), None, Some((true, false))), cx);
            let remote = view.outbound.as_ref().unwrap();
            assert!(!remote.busy, "failed RPC releases the composer");
            assert_eq!(remote.error.as_deref(), Some("host refused"));
            assert_eq!(remote.failed_prompt.as_ref().unwrap().0, "refused prompt");
            assert_eq!(view.thread.entries.len(), 1, "failed send creates no local bubble");
            view.outbound.as_mut().unwrap().pending_prompt = Some(("interrupted prompt".into(), vec![]));
            view.outbound.as_mut().unwrap().busy = true;
            view.set_remote_connection(None, None, cx);
            assert_eq!(view.outbound.as_ref().unwrap().failed_prompt.as_ref().unwrap().0, "interrupted prompt");
            assert!(!view.outbound.as_ref().unwrap().busy);
            view.outbound.as_mut().unwrap().error = None;
            view.finish_outbound_rpc(revision, None, (Err("stale error".into()), Err("closed".into()), None, None), cx);
            assert!(view.outbound.as_ref().unwrap().error.is_none(), "old RPC cannot affect a reconnected view");
        }).unwrap();
    }
}
