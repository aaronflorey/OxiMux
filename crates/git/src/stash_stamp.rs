//! A subprocess-free fingerprint of the stash stack, carried on every status
//! poll so the Stashes section notices a stash pushed, popped, dropped or
//! cleared outside the app.
//!
//! The stack is shared by every worktree of the repo and written by the
//! user's terminal and by agents, none of which this process can observe —
//! and dropping a stash changes nothing `git status` reports, so the poll
//! itself cannot tell. Git rewrites the stash's reflog on each of those
//! writes, so the reflog's size and mtime change with it: two stats, no
//! `git` spawn, cheap enough for the poll loop.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::repository::Repository;

impl Repository {
    /// Fingerprint of the stash stack: `(size, mtime in ns)` of the file git
    /// rewrites on every stash write, or `None` when there is none (no stash
    /// yet, or `git stash clear` removed it). Compare two stamps; never read
    /// meaning into one.
    ///
    /// The files backend keeps the stack in `logs/refs/stash`. Under the
    /// reftable backend (git 2.45+) there is no `logs/`; its `tables.list`
    /// changes on *any* ref update, which over-reports but never misses.
    pub fn stash_stamp(&self) -> Option<(u64, u64)> {
        let common = common_dir(&self.git_dir);
        let meta = std::fs::metadata(common.join("logs/refs/stash"))
            .or_else(|_| std::fs::metadata(common.join("reftable/tables.list")))
            .ok()?;
        let mtime_ns = meta
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos();
        Some((meta.len(), u64::try_from(mtime_ns).unwrap_or(u64::MAX)))
    }
}

/// The directory shared by every worktree of the repo that owns `git_dir`.
/// A linked worktree's own git dir names it in `commondir` (usually relative,
/// `../..`); a primary worktree's git dir is the shared one.
fn common_dir(git_dir: &Path) -> PathBuf {
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(pointer) => git_dir.join(pointer.trim()),
        Err(_) => git_dir.to_path_buf(),
    }
}
