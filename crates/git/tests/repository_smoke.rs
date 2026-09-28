//! Integration smoke tests for `Repository::open` + `Repository::status`.
//!
//! These require `git` on PATH (host runner + CI provide it). They build a
//! throwaway repo in a tempdir, perform a few mutations, and verify the
//! observable state.

use oximux_git::{GitError, Repository};
use std::fs;
use tempfile::tempdir;

async fn git(repo: &std::path::Path, args: &[&str]) {
    let _ = oximux_git::GitCmd::new(repo)
        .args(args.iter().copied())
        .run()
        .await
        .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
}

#[tokio::test]
async fn open_rejects_non_repo() {
    let tmp = tempdir().unwrap();
    let err = Repository::open(tmp.path())
        .await
        .expect_err("non-repo should reject");
    match err {
        GitError::NotARepo { .. } => {}
        other => panic!("expected NotARepo, got {other:?}"),
    }
}

#[tokio::test]
async fn open_rejects_missing_path() {
    let err = Repository::open("/no/such/path/oximux/test")
        .await
        .expect_err("missing path should reject");
    assert!(matches!(err, GitError::NotARepo { .. }));
}

#[tokio::test]
async fn empty_repo_reports_initial() {
    let tmp = tempdir().unwrap();
    git(tmp.path(), &["init", "-b", "main"]).await;
    let repo = Repository::open(tmp.path()).await.expect("open");
    let state = repo.status().await.expect("status");
    assert_eq!(state.branch.as_deref(), Some("main"));
    // Initial repo: no commits yet → no head OID.
    assert_eq!(state.head_oid, None);
    assert!(state.files.is_empty());
}

#[tokio::test]
async fn empty_repo_log_is_an_empty_history_not_an_error() {
    // `git log` exits 128 on an unborn branch ("does not have any commits
    // yet"); the commit graph printed that raw instead of "No commits yet".
    let tmp = tempdir().unwrap();
    git(tmp.path(), &["init", "-b", "main"]).await;
    let repo = Repository::open(tmp.path()).await.expect("open");
    assert!(repo.log_recent(20).await.expect("log_recent").is_empty());
    assert!(repo.log_page(20, 20).await.expect("log_page").is_empty());

    // A real failure still surfaces once HEAD has a commit.
    fs::write(tmp.path().join("a.txt"), b"a\n").unwrap();
    git(tmp.path(), &["add", "a.txt"]).await;
    git(
        tmp.path(),
        &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-m", "first"],
    )
    .await;
    assert_eq!(repo.log_recent(20).await.expect("log_recent").len(), 1);
}

#[tokio::test]
async fn a_vanished_repo_log_is_an_error_not_an_empty_history() {
    // Only an unborn HEAD may read as "No commits yet": a repository whose
    // `.git` went away after it was opened must still report the failure.
    let tmp = tempdir().unwrap();
    git(tmp.path(), &["init", "-b", "main"]).await;
    let repo = Repository::open(tmp.path()).await.expect("open");
    fs::rename(tmp.path().join(".git"), tmp.path().join("gone")).unwrap();
    assert!(repo.log_recent(20).await.is_err());
}

#[tokio::test]
async fn untracked_file_appears_in_status() {
    let tmp = tempdir().unwrap();
    git(tmp.path(), &["init", "-b", "main"]).await;
    fs::write(tmp.path().join("hello.txt"), b"hi\n").unwrap();
    let repo = Repository::open(tmp.path()).await.expect("open");
    let state = repo.status().await.expect("status");
    assert_eq!(state.files.len(), 1);
    assert_eq!(state.files[0].path.to_str(), Some("hello.txt"));
}

#[tokio::test]
async fn open_from_subdirectory_walks_to_root() {
    let tmp = tempdir().unwrap();
    git(tmp.path(), &["init", "-b", "main"]).await;
    let nested = tmp.path().join("a/b/c");
    fs::create_dir_all(&nested).unwrap();
    let repo = Repository::open(&nested).await.expect("open from subdir");
    // workdir() should resolve to the toplevel, not the subdir we opened from.
    let canon_tmp = fs::canonicalize(tmp.path()).unwrap();
    let canon_repo = fs::canonicalize(repo.workdir()).unwrap();
    assert_eq!(canon_repo, canon_tmp);
}
