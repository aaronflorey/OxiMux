//! Stale-record cleanup. Older builds persisted a remote-workspace
//! selection under `remote_workspace:{window_id}`; the takeover shell is
//! gone and remote projects mount in the panes area, so the record is only
//! ever cleared — writing `null` so nothing resurrects it on boot.
use oximux_storage::SettingsRepo;

fn key(window_id: &str) -> String { format!("remote_workspace:{window_id}") }

/// Clear a stale selection record written by a takeover-era build.
pub(crate) fn clear(repo: &SettingsRepo, window_id: &str) {
    if let Err(error) = repo.set(&key(window_id), "null").map_err(|e| e.to_string()) {
        tracing::warn!(%error, "Could not clear remote tab selection");
    }
}
