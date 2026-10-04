//! Server data is rendered as read-only; actions live in the remote Git panel.
use super::*;

impl DiffView {
    pub(crate) fn set_remote_diffs(&mut self, diffs: Vec<FileDiff>, cx: &mut Context<Self>) {
        if self.repo.is_some() { return; }
        self.invalidate_plan();
        self.collapsed.clear();
        self.expanded_folds.clear();
        self.reset_rail_state();
        self.notes.clear();
        self.images.clear();
        self.state = DiffViewState::CombinedReady {
            scope: CombinedDiffScope::AllChanges,
            groups: vec![FileGroup::Committed; diffs.len()], diffs, expanded: false,
        };
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn remote_diff_never_enters_local_loading_or_note_storage(cx: &mut TestAppContext) {
        let view = cx.new(|cx| DiffView::new_remote(Theme::default(), Density::default(), Typography::default(), cx));
        view.update(cx, |view, cx| {
            view.set_remote_diffs(vec![FileDiff { path: "server/file.txt".into(),
                status: oximux_core::DiffStatus::Added, hunks: vec![], large: false, mode: None }], cx);
            assert!(view.repo.is_none());
            assert!(view._live_refresh_task.is_none());
            assert!(view.side_for_region(0).is_none(), "no local staging card");
            view.load("server/file.txt".into(), false, false, cx);
            assert!(matches!(&view.state, DiffViewState::CombinedReady { diffs, .. } if diffs.len() == 1),
                "local load is inert and preserves the server diff");
            assert!(view.notes.is_empty());
            assert!(view._load_task.is_none());
        });
    }
}
