//! Durable host-file draft recovery.
//!
//! Dirty remote buffers used to live only in memory — `remote_drafts`,
//! cached sidebars, `RemoteFilesView::buffers` — so closing a workspace
//! window or quitting dropped them silently. This store mirrors the local
//! pane-buffer capture contract: at session capture (quit, last-window
//! close, single-window close) the workspace serializes every dirty host
//! buffer to `remote-drafts.json` beside `hosts.toml`, and the next mount
//! of that `(endpoint, surface)` rehydrates them as dirty buffers.
//!
//! The baseline text + version travel with each draft: a rehydrated
//! buffer diffs and saves exactly like the original would have, and a
//! base that went stale on the host fails the version check instead of
//! silently clobbering newer host content.

use serde::{Deserialize, Serialize};

/// One unsaved host-file draft, fully replayable.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct DraftEntry {
    /// Host endpoint (lowercase enrollment id) owning the surface — the
    /// draft-parking key's first half.
    pub endpoint: String,
    /// `Root::key()` of the surface — `session:{id}` or `project:{path}`.
    pub root: String,
    /// The file's path inside that surface (host-relative or session-scoped).
    pub path: String,
    /// The host baseline the draft diverged from — replayed as the buffer's
    /// `loaded` so dirty detection and Save behave as if never interrupted.
    pub base_text: String,
    /// The baseline's host version token. A base that went stale on the
    /// host makes the next save fail version-mismatch — the conflict
    /// surfaces instead of the draft clobbering newer host content.
    pub base_version: String,
    /// The unsaved editor contents.
    pub draft: String,
}

#[derive(Default, Serialize, Deserialize)]
struct DraftsFile {
    #[serde(default)]
    drafts: Vec<DraftEntry>,
}

fn store_path() -> Option<std::path::PathBuf> {
    oximux_remote_session::hosts_store::config_dir()
        .ok()
        .map(|dir| dir.join("remote-drafts.json"))
}

fn load(path: &std::path::Path) -> Vec<DraftEntry> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<DraftsFile>(&bytes)
            .map(|file| file.drafts)
            .unwrap_or_else(|error| {
                tracing::warn!(?error, "remote-drafts.json unreadable — starting empty");
                Vec::new()
            }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            tracing::warn!(?error, "remote-drafts.json unreadable — starting empty");
            Vec::new()
        }
    }
}

fn write(path: &std::path::Path, drafts: Vec<DraftEntry>) {
    let file = DraftsFile { drafts };
    match serde_json::to_vec(&file) {
        Ok(bytes) => {
            if let Err(error) = std::fs::write(path, bytes) {
                tracing::warn!(?error, "persisting remote drafts failed");
            }
        }
        Err(error) => tracing::warn!(?error, "serializing remote drafts failed"),
    }
}

/// The merge `replace_for` persists: `stored` entries for surfaces in
/// `owned` drop out entirely, `entries` append. Pure so the merge is
/// unit-testable without a config dir.
fn merged(
    stored: Vec<DraftEntry>,
    owned: &std::collections::HashSet<(String, String)>,
    entries: Vec<DraftEntry>,
) -> Vec<DraftEntry> {
    let mut drafts: Vec<DraftEntry> = stored
        .into_iter()
        .filter(|entry| !owned.contains(&(entry.endpoint.clone(), entry.root.clone())))
        .collect();
    drafts.extend(entries);
    drafts
}

/// Replace the store's entries for the `(endpoint, root)` surfaces in
/// `owned` with `entries` — the caller's current dirty set, possibly
/// empty. Surfaces no caller owns keep their stored drafts, so several
/// windows capturing independently never clobber each other's.
///
/// A surface's whole entry set is rewritten even when its dirty count
/// dropped to zero — that's what retires entries for buffers the user
/// saved or reverted since the last capture.
pub(crate) fn replace_for(owned: &std::collections::HashSet<(String, String)>, entries: Vec<DraftEntry>) {
    if owned.is_empty() {
        return;
    }
    let Some(path) = store_path() else { return };
    write(&path, merged(load(&path), owned, entries));
}

/// Remove and return every stored draft for `(endpoint, root)` — the
/// mount-time half of recovery: drafts are consumed once so a stale copy
/// can't shadow a live (re-parked or re-captured) buffer later.
pub(crate) fn take(endpoint: &str, root: &str) -> Vec<DraftEntry> {
    let Some(path) = store_path() else { return Vec::new() };
    let drafts = load(&path);
    let (hits, rest): (Vec<DraftEntry>, Vec<DraftEntry>) = drafts
        .into_iter()
        .partition(|entry| entry.endpoint == endpoint && entry.root == root);
    if !hits.is_empty() {
        write(&path, rest);
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(endpoint: &str, root: &str, path: &str, draft: &str) -> DraftEntry {
        DraftEntry {
            endpoint: endpoint.into(),
            root: root.into(),
            path: path.into(),
            base_text: "base".into(),
            base_version: "v1".into(),
            draft: draft.into(),
        }
    }

    #[test]
    fn merged_rewrites_only_owned_surfaces() {
        let stored = vec![
            draft("ep1", "project:/a", "x.rs", "stale"),
            draft("ep2", "project:/b", "y.rs", "other-window"),
        ];
        // ep1's surface now reports one dirty buffer under a new path —
        // its old entry must be fully replaced, ep2's untouched.
        let owned = std::collections::HashSet::from([("ep1".into(), "project:/a".into())]);
        let out = merged(stored, &owned, vec![draft("ep1", "project:/a", "z.rs", "new-draft")]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|e| e.path == "z.rs" && e.draft == "new-draft"));
        assert!(out.iter().any(|e| e.path == "y.rs" && e.endpoint == "ep2"));
    }

    #[test]
    fn merged_with_empty_entries_retires_the_surface() {
        // A surface captured with zero dirty buffers drops its stored
        // drafts — that's how a saved or reverted draft stops restoring.
        let stored = vec![draft("ep1", "project:/a", "x.rs", "gone")];
        let owned = std::collections::HashSet::from([("ep1".into(), "project:/a".into())]);
        assert!(merged(stored, &owned, Vec::new()).is_empty());
    }

    #[test]
    fn store_roundtrips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote-drafts.json");
        let entries = vec![draft("ab12", "session:sess-9", "src/main.rs", "fn edited()")];
        write(&path, entries.clone());
        assert_eq!(load(&path), entries);
    }

    #[test]
    fn missing_or_corrupt_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote-drafts.json");
        assert!(load(&path).is_empty());
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load(&path).is_empty());
    }
}
