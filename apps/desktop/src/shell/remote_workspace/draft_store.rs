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
//!
//! Every entry is stamped with the capturing window's persist id:
//! several windows can hold the same remote surface at once, and an
//! empty capture from one must never retire another's drafts.

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
    /// Persist id of the window that captured this draft ("main", "w1", …).
    /// Two windows can open the same `(endpoint, root)` surface with
    /// independent buffers, so retirement is per-window: a window's own
    /// rewrite drops only entries it owns.
    #[serde(default)]
    pub window: String,
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

/// Replace the whole store, atomically: a same-directory temporary file,
/// synced, then renamed over the target — the `HostsFile::save`
/// precedent. A failed or interrupted write leaves the previous store
/// (every window's drafts, including surfaces this caller doesn't own)
/// byte-for-byte intact, and the error propagates to the caller instead
/// of unsaved buffers being dropped silently.
fn write(path: &std::path::Path, drafts: Vec<DraftEntry>) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(&DraftsFile { drafts })
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "draft store path has no parent")
    })?;
    use std::io::Write;
    let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|persist| persist.error)?;
    Ok(())
}

/// The merge `replace_for` persists: `stored` entries owned by a
/// `(endpoint, root, window)` triple in `owned` drop out entirely,
/// `entries` append. Pure so the merge is unit-testable without a
/// config dir.
fn merged(
    stored: Vec<DraftEntry>,
    owned: &std::collections::HashSet<(String, String, String)>,
    entries: Vec<DraftEntry>,
) -> Vec<DraftEntry> {
    let mut drafts: Vec<DraftEntry> = stored
        .into_iter()
        .filter(|entry| {
            !owned.contains(&(
                entry.endpoint.clone(),
                entry.root.clone(),
                entry.window.clone(),
            ))
        })
        .collect();
    drafts.extend(entries);
    drafts
}

/// Replace the store's entries this `(endpoint, root, window)` triple in
/// `owned` owns with `entries` — that window's current dirty set,
/// possibly empty. Entries other windows wrote — even for the same
/// surface — survive untouched, so several windows capturing
/// independently never clobber each other's drafts.
///
/// A window's whole entry set is rewritten even when its dirty count
/// dropped to zero — that's what retires entries for buffers it saved
/// or reverted since the last capture.
///
/// Persistence errors propagate: the close path keeps closing either
/// way, but the caller hears the failure rather than dropping unsaved
/// buffers without a trace.
pub(crate) fn replace_for(
    owned: &std::collections::HashSet<(String, String, String)>,
    entries: Vec<DraftEntry>,
) -> std::io::Result<()> {
    if owned.is_empty() {
        return Ok(());
    }
    let Some(path) = store_path() else { return Ok(()) };
    write(&path, merged(load(&path), owned, entries))
}

/// Remove and return every stored draft for `(endpoint, root)` — the
/// mount-time half of recovery: drafts are consumed once so a stale copy
/// can't shadow a live (re-parked or re-captured) buffer later. Entries
/// from every window that captured the surface return together; when two
/// windows edited the same path, the later capture wins at restore.
pub(crate) fn take(endpoint: &str, root: &str) -> Vec<DraftEntry> {
    let Some(path) = store_path() else { return Vec::new() };
    let drafts = load(&path);
    let (hits, rest): (Vec<DraftEntry>, Vec<DraftEntry>) = drafts
        .into_iter()
        .partition(|entry| entry.endpoint == endpoint && entry.root == root);
    if !hits.is_empty() {
        // A failed removal write just leaves the entries stored — the
        // drafts came back, and a later take can replay them again.
        if let Err(error) = write(&path, rest) {
            tracing::warn!(?error, "persisting remote-draft removal failed");
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(endpoint: &str, root: &str, window: &str, path: &str, draft: &str) -> DraftEntry {
        DraftEntry {
            endpoint: endpoint.into(),
            root: root.into(),
            path: path.into(),
            window: window.into(),
            base_text: "base".into(),
            base_version: "v1".into(),
            draft: draft.into(),
        }
    }

    fn owned(endpoint: &str, root: &str, window: &str) -> std::collections::HashSet<(String, String, String)> {
        std::collections::HashSet::from([(
            endpoint.to_string(),
            root.to_string(),
            window.to_string(),
        )])
    }

    #[test]
    fn merged_rewrites_only_owned_surfaces() {
        let stored = vec![
            draft("ep1", "project:/a", "w1", "x.rs", "stale"),
            draft("ep2", "project:/b", "w1", "y.rs", "other-surface"),
        ];
        // ep1's surface now reports one dirty buffer under a new path —
        // its old entry must be fully replaced, ep2's untouched.
        let out = merged(stored, &owned("ep1", "project:/a", "w1"), vec![
            draft("ep1", "project:/a", "w1", "z.rs", "new-draft"),
        ]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|e| e.path == "z.rs" && e.draft == "new-draft"));
        assert!(out.iter().any(|e| e.path == "y.rs" && e.endpoint == "ep2"));
    }

    #[test]
    fn merged_with_empty_entries_retires_the_surface() {
        // A window captured with zero dirty buffers drops ITS stored
        // drafts — that's how a saved or reverted draft stops restoring.
        let stored = vec![draft("ep1", "project:/a", "w1", "x.rs", "gone")];
        assert!(merged(stored, &owned("ep1", "project:/a", "w1"), Vec::new()).is_empty());
    }

    /// The cross-window case: the same `(endpoint, root)` surface open in
    /// windows w1 and w2. w2's clean (empty) capture must not retire
    /// w1's dirty entry, and two dirty windows editing different files
    /// keep both drafts.
    #[test]
    fn same_surface_captures_from_two_windows_keep_both_drafts() {
        let stored = vec![draft("ep1", "project:/a", "w1", "a.txt", "w1 draft")];
        // w2 captures the same surface with no dirty buffers — w1's
        // entry survives because ownership is per-window.
        let out = merged(stored.clone(), &owned("ep1", "project:/a", "w2"), Vec::new());
        assert_eq!(out, stored, "w2's empty capture must not retire w1's draft");

        // w2 then captures its own dirty file — both windows' drafts land.
        let out = merged(out, &owned("ep1", "project:/a", "w2"), vec![
            draft("ep1", "project:/a", "w2", "b.txt", "w2 draft"),
        ]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|e| e.path == "a.txt" && e.window == "w1"));
        assert!(out.iter().any(|e| e.path == "b.txt" && e.window == "w2"));

        // w1 saves and captures clean — only its own entries retire.
        let out = merged(out, &owned("ep1", "project:/a", "w1"), Vec::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "b.txt");
    }

    #[test]
    fn store_roundtrips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote-drafts.json");
        let entries = vec![draft("ab12", "session:sess-9", "main", "src/main.rs", "fn edited()")];
        write(&path, entries.clone()).expect("atomic store write");
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

    /// A write that cannot complete must leave the previous recovery file
    /// byte-for-byte intact — the old code truncated the store before
    /// writing, so any mid-write failure destroyed every window's drafts.
    /// Read-only directory fails the temporary-file creation, the same
    /// stage a disk-full/quota failure hits.
    #[cfg(unix)]
    #[test]
    fn failed_write_keeps_the_previous_store() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote-drafts.json");
        let old = vec![draft("ep1", "project:/a", "w1", "x.rs", "keep-me")];
        write(&path, old.clone()).expect("initial store write");
        let before = std::fs::read(&path).unwrap();

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = write(&path, vec![draft("ep1", "project:/a", "w1", "y.rs", "lost")]);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err(), "a read-only directory must fail the write");
        assert_eq!(std::fs::read(&path).unwrap(), before, "the previous store survives a failed write");
        assert_eq!(load(&path), old);
    }
}
