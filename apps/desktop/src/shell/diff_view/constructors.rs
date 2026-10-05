use super::*;

impl DiffView {
    pub fn new(repo: Repository, theme: Theme, density: Density, typography: Typography, cx: &mut Context<Self>) -> Self {
        Self::assemble(Some(repo), theme, density, typography, cx)
    }

    /// Render server-supplied diffs without any local repository or poller.
    pub(crate) fn new_remote(theme: Theme, density: Density, typography: Typography, cx: &mut Context<Self>) -> Self {
        Self::assemble(None, theme, density, typography, cx)
    }

    fn assemble(
        repo: Option<Repository>,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        // Editor-global font zoom changes from any editor (or this diff's own
        // Cmd+/-) must repaint the diff body so its code lines track the same
        // size.
        let _zoom_sub = cx.observe_global::<EditorZoom>(|_view, cx| cx.notify());
        // Local repository heartbeat. `tick_live_refresh`
        // decides on each one whether there is anything worth asking, so a
        // commit view or an unfocused window costs a wakeup and nothing else.
        let _live_refresh_task = repo.is_some().then(|| cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(LIVE_REFRESH_TICK).await;
                if weak
                    .update(cx, |view, cx| view.tick_live_refresh(cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        Self {
            repo,
            state: DiffViewState::Empty,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            commit_noun: "commit",
            _load_task: None,
            _op_task: None,
            _live_refresh_task,
            _live_fetch_task: None,
            live_refresh_in_flight: false,
            window_active: true,
            _activation_sub: None,
            confirm_dialog: None,
            _confirm_dialog_observer: None,
            body_list: ListState::new(0, ListAlignment::Top, px(400.0)),
            body_list_zoom: 1.0,
            body_list_was_populated: false,
            prepared: None,
            plan_cache: None,
            plan_gen: 0,
            _highlight_task: None,
            images: HashMap::new(),
            _image_task: None,
            image_gen: 0,
            recently_copied_file: None,
            _copied_clear_task: None,
            prepared_widest: 0,
            prepared_widest_chars: 0,
            split_h_offset: 0.0,
            hovered_region: None,
            hovered_row: None,
            overview: Rc::new(Vec::new()),
            split: false,
            collapsed: HashSet::new(),
            expanded_folds: HashSet::new(),
            row_owner: Rc::new(Vec::new()),
            headers: Rc::new(Vec::new()),
            first_row_of_file: Rc::new(Vec::new()),
            rail_open: true,
            rail_collapsed_dirs: HashSet::new(),
            rail_filter: None,
            _rail_filter_sub: None,
            pending_scroll_anchor: None,
            notes: ReviewNoteStore::new(),
            note_popover: None,
            _note_popover_observer: None,
            opener: None,
            _zoom_sub,
        }
    }

}
