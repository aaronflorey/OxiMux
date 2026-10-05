//! Project-rooted browse RPCs (v29): the session-scoped filesystem and git
//! verbs, addressed by project path so a client can open a remote repository
//! before — or without ever — spawning an agent.
//!
//! Paths still cross the wire host-relative inside the named project, and the
//! host re-contains every one against it; replies are the same wire types the
//! session-scoped calls return.

use oximux_remote_proto::files::{DirectoryWire, TextFileWire};
use oximux_remote_proto::messages::{FileDiffWire, GitStatusWire};
use oximux_remote_proto::proto::{BROWSE_MIN_VERSION, BrowseOp, Request, Response, RpcError};

use super::{RemoteSession, Result};
use crate::error::SessionError;

impl RemoteSession {
    /// v29 gate, mirroring `require_files`: a v28 host cannot decode the
    /// ordinal and would answer as if the frame were malformed.
    fn require_browse(&self) -> Result<()> {
        if self.host_protocol_version().is_some_and(|v| v >= BROWSE_MIN_VERSION) { Ok(()) }
        else { Err(SessionError::Rpc(RpcError::Unsupported)) }
    }

    async fn browse(&self, project_path: &str, op: BrowseOp) -> Result<Response> {
        self.require_browse()?;
        self.call(Request::ProjectBrowse { project_path: project_path.into(), op }).await
    }

    /// Directory page under `project_path` — the reply a session-rooted
    /// [`Self::list_directory`] returns, without a session.
    pub async fn browse_list_directory(&self, project_path: &str, path: &str, after: Option<&str>) -> Result<DirectoryWire> {
        match self.browse(project_path, BrowseOp::ListDirectory {
            path: path.into(), after: after.map(str::to_owned),
        }).await? {
            Response::Directory(value) => Ok(value),
            Response::Error(error) => Err(SessionError::Rpc(error)),
            _ => Err(SessionError::Unexpected { expected: "Directory" }),
        }
    }

    pub async fn browse_read_text_file(&self, project_path: &str, path: &str) -> Result<TextFileWire> {
        match self.browse(project_path, BrowseOp::ReadTextFile { path: path.into() }).await? {
            Response::TextFile(value) => Ok(value),
            Response::Error(error) => Err(SessionError::Rpc(error)),
            _ => Err(SessionError::Unexpected { expected: "TextFile" }),
        }
    }

    /// Versioned replace, refusing writes a read-only device could not have
    /// made through a session either (the host's capability gate is the same).
    pub async fn browse_write_text_file(&self, project_path: &str, path: &str, text: &str, version: &str) -> Result<TextFileWire> {
        match self.browse(project_path, BrowseOp::WriteTextFile {
            path: path.into(), text: text.into(), version: version.into(),
        }).await? {
            Response::TextFile(value) => Ok(value),
            Response::Error(error) => Err(SessionError::Rpc(error)),
            _ => Err(SessionError::Unexpected { expected: "TextFile" }),
        }
    }

    pub async fn browse_git_status(&self, project_path: &str) -> Result<GitStatusWire> {
        match self.browse(project_path, BrowseOp::GitStatus).await? {
            Response::GitStatus(status) => Ok(status),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "GitStatus" }),
        }
    }

    /// Diff one path inside the project repository. `staged` picks
    /// index-vs-HEAD; `untracked` selects the read-off-disk path.
    pub async fn browse_git_diff(
        &self,
        project_path: &str,
        path: &str,
        staged: bool,
        untracked: bool,
    ) -> Result<Vec<FileDiffWire>> {
        match self.browse(project_path, BrowseOp::GitDiff { path: path.into(), staged, untracked }).await? {
            Response::GitDiff(files) => Ok(files),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "GitDiff" }),
        }
    }

    pub async fn browse_git_stage(&self, project_path: &str, paths: &[String]) -> Result<()> {
        self.browse_ack(project_path, BrowseOp::GitStage { paths: paths.to_vec() }).await
    }

    pub async fn browse_git_unstage(&self, project_path: &str, paths: &[String]) -> Result<()> {
        self.browse_ack(project_path, BrowseOp::GitUnstage { paths: paths.to_vec() }).await
    }

    /// Commit what is already staged, returning the new HEAD sha. Path-less
    /// for the same reason [`Self::git_commit`] is.
    pub async fn browse_git_commit(&self, project_path: &str, message: &str) -> Result<String> {
        match self.browse(project_path, BrowseOp::GitCommit { message: message.into() }).await? {
            Response::GitCommitted { sha } => Ok(sha),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "GitCommitted" }),
        }
    }

    async fn browse_ack(&self, project_path: &str, op: BrowseOp) -> Result<()> {
        match self.browse(project_path, op).await? {
            Response::Ack => Ok(()),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Ack" }),
        }
    }
}
