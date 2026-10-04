use super::*;

impl AgentChatView {
    /// Construct a chat view and spawn its headless `claude` subprocess in
    /// `cwd`. A spawn failure degrades to a read-only error state rather than
    /// panicking, so the tab still opens and explains what went wrong.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cwd: PathBuf,
        model: Option<String>,
        backend: ChatBackend,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::assemble(
            cwd,
            model,
            backend,
            ChatThread::new(),
            ConnectMode::Connect,
            RestoredPosture::default(),
            theme,
            density,
            typography,
            window,
            cx,
        );
        // A Claude tab opened straight from the launcher refreshes the CLI's
        // model list too, not only the draft that picks Claude by hand.
        view.probe_claude_catalog_if_bound(cx);
        view
    }

    /// Construct an **unbound** chat view for the unified *New Agent* entry: no
    /// subprocess is spawned. `backend`/`model` seed the *currently picked* agent
    /// (the composer's agent picker can change them before the first send); the
    /// first `send_text` binds the transport and spawns the chosen agent. A
    /// provider-agnostic caller defaults to [`ChatBackend::stream_json`] (Claude).
    #[allow(clippy::too_many_arguments)]
    pub fn new_unbound(
        cwd: PathBuf,
        model: Option<String>,
        backend: ChatBackend,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::assemble(
            cwd,
            model,
            backend,
            ChatThread::new(),
            ConnectMode::UnboundDraft,
            RestoredPosture::default(),
            theme,
            density,
            typography,
            window,
            cx,
        );
        // Seed the composer's agent + model pickers from the chat roster so the
        // draft offers the choice on its first paint (before any subprocess). If
        // the seed agent is dynamic-model (unusual — the entry defaults to Claude),
        // kick its catalog probe so the picker still fills.
        view.sync_unbound_composer(cx);
        if let Some(id) = view.unbound_agent_id.clone() {
            view.maybe_probe_catalog(id, cx);
        }
        view
    }

    /// Rebuild a chat view on session restore: seed the thread from the
    /// persisted transcript and spawn the subprocess with `--resume
    /// <session_id>` (via [`ChatThread::rehydrated`]'s captured id) so the
    /// continued conversation keeps its context. The visible history paints
    /// immediately from `entries` — it does not wait on the resumed process.
    ///
    /// LIVE-VERIFY: `claude -p --resume` in stream-json mode is expected to load
    /// the session server-side and wait for input (not replay prior turns to
    /// stdout). If it *does* replay, the drain would append duplicate entries
    /// atop the rehydrated ones — watch for doubled bubbles on the first restore
    /// eyeball; the fix would be to drop the rehydrated seed and render purely
    /// from the replay.
    #[allow(clippy::too_many_arguments)]
    pub fn new_resumed(
        cwd: PathBuf,
        model: Option<String>,
        backend: ChatBackend,
        session_id: Option<String>,
        entries: Vec<ThreadEntry>,
        slash_commands: Vec<String>,
        session_meta: oximux_agents::thread::SessionMeta,
        thinking_level: ThinkingLevel,
        posture: RestoredPosture,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut thread = ChatThread::rehydrated(session_id, model.clone(), entries, slash_commands);
        // Seeded from the blob so the session-detail popover is populated on a
        // restored chat; a later live init overwrites it.
        thread.session_meta = session_meta;
        // Dormant: a restored chat spawns NO subprocess at construction (a
        // resumed CLI re-reads its whole session file — a layout with many
        // chat tabs would cold-start them all at boot). First render or a
        // remote open connects via `ensure_connected` → `--resume`.
        let mut view = Self::assemble(
            cwd,
            model,
            backend,
            thread,
            ConnectMode::DormantResume,
            posture,
            theme,
            density,
            typography,
            window,
            cx,
        );
        // The blob this view was built from IS the on-disk state — a save
        // before any mutation must skip it.
        view.last_saved_revision.set(view.thread.revision());
        view.thinking_level = thinking_level;
        // A resumed chat that already has history must NOT regenerate (or
        // overwrite) its label on the next send — mark it already-titled.
        view.title_generated = !view.thread.entries.is_empty();
        view
    }

    /// Construct a transcript-only **import bridge** for an OpenCode / Pi
    /// session: seed the transcript, but spawn NO subprocess
    /// ([`ConnectMode::ImportBridge`]) — these providers have no in-app chat
    /// backend. Unlike a *New
    /// Agent* draft (also connection-less), this is not `unbound`: the composer
    /// is swapped for a Resume-in-terminal action ([`Self::import_bridge`]), and
    /// `send_text` is a no-op, so it can never masquerade as a live chat.
    #[allow(clippy::too_many_arguments)]
    pub fn new_import_bridge(
        cwd: PathBuf,
        entries: Vec<ThreadEntry>,
        bridge: ImportBridge,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Seed the transcript with no session id (no `--resume` this view can
        // drive) and the Claude backend as an inert placeholder — it's never
        // connected. The placeholder must not leak into the UI:
        // `provider_label` reads the bridge's own name, so bubbles are
        // captioned with the provider the transcript actually came from.
        let thread = ChatThread::rehydrated(None, None, entries, Vec::new());
        let mut view = Self::assemble(
            cwd,
            None,
            ChatBackend::stream_json(),
            thread,
            ConnectMode::ImportBridge,
            RestoredPosture::default(),
            theme,
            density,
            typography,
            window,
            cx,
        );
        view.title_generated = true;
        view.import_bridge = Some(bridge);
        view
    }


}
