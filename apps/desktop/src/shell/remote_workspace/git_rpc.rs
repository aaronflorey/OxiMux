//! Only supported Git RPCs. Every path stays repository-relative host data.
use oximux_remote_session::RemoteSession;
use oximux_remote_proto::messages::{GitStatusWire, FileDiffWire, DiffStatusWire, DiffLineKindWire};
use oximux_core::{FileDiff, DiffStatus, DiffHunk, DiffLine, DiffLineKind};
use super::Root;

#[derive(Clone)]
pub(crate) enum Operation {
    Status,
    Diff { path: String, staged: bool, untracked: bool },
    Stage(String),
    Unstage(String),
    Commit(String),
}
impl Operation {
    pub fn mutates(&self) -> bool { matches!(self, Self::Stage(_) | Self::Unstage(_) | Self::Commit(_)) }
}

pub(crate) enum Reply {
    Status(GitStatusWire),
    Diff(Vec<FileDiff>),
    // A successful mutation remains successful even if its status refresh fails.
    Mutated { sha: Option<String>, status: Result<GitStatusWire, String> },
}

async fn rpc<T>(future: impl std::future::Future<Output = Result<T, oximux_remote_session::SessionError>>) -> Result<T, String> {
    tokio::time::timeout(std::time::Duration::from_secs(30), future).await
        .map_err(|_| "Git operation timed out; refresh before retrying a mutation".to_string())?
        .map_err(|e| e.to_string())
}

async fn status_of(session: &RemoteSession, root: &Root) -> Result<GitStatusWire, oximux_remote_session::SessionError> {
    match root {
        Root::Session(id) => session.git_status(id).await,
        Root::Project(project) => session.browse_git_status(project).await,
    }
}

pub(crate) async fn execute(session: &RemoteSession, root: &Root, operation: Operation) -> Result<Reply, String> {
    let sha = match (root, operation) {
        (_, Operation::Status) => return rpc(status_of(session, root)).await.map(Reply::Status),
        (Root::Session(id), Operation::Diff { path, staged, untracked }) => return rpc(session.git_diff(id, &path, staged, untracked)).await
            .map(|files| Reply::Diff(files.into_iter().map(from_wire).collect())),
        (Root::Project(project), Operation::Diff { path, staged, untracked }) => return rpc(session.browse_git_diff(project, &path, staged, untracked)).await
            .map(|files| Reply::Diff(files.into_iter().map(from_wire).collect())),
        (Root::Session(id), Operation::Stage(path)) => { rpc(session.git_stage(id, &[path])).await?; None }
        (Root::Project(project), Operation::Stage(path)) => { rpc(session.browse_git_stage(project, &[path])).await?; None }
        (Root::Session(id), Operation::Unstage(path)) => { rpc(session.git_unstage(id, &[path])).await?; None }
        (Root::Project(project), Operation::Unstage(path)) => { rpc(session.browse_git_unstage(project, &[path])).await?; None }
        (Root::Session(id), Operation::Commit(message)) => Some(rpc(session.git_commit(id, &message)).await?),
        (Root::Project(project), Operation::Commit(message)) => Some(rpc(session.browse_git_commit(project, &message)).await?),
    };
    Ok(Reply::Mutated { sha, status: rpc(status_of(session, root)).await })
}

fn from_wire(file: FileDiffWire) -> FileDiff {
    FileDiff { path: file.path.into(), large: file.large, mode: None,
        status: match file.status {
            DiffStatusWire::Added => DiffStatus::Added,
            DiffStatusWire::Modified => DiffStatus::Modified,
            DiffStatusWire::Deleted => DiffStatus::Deleted,
            DiffStatusWire::Renamed { from, similarity } => DiffStatus::Renamed { from: from.into(), similarity },
            DiffStatusWire::Copied { from, similarity } => DiffStatus::Copied { from: from.into(), similarity },
            DiffStatusWire::ModeChanged { old_mode, new_mode } => DiffStatus::ModeChanged { old_mode, new_mode },
            DiffStatusWire::Binary => DiffStatus::Binary,
        },
        hunks: file.hunks.into_iter().map(|h| DiffHunk { old_start: h.old_start, old_lines: h.old_lines,
            new_start: h.new_start, new_lines: h.new_lines, header_suffix: h.header_suffix,
            lines: h.lines.into_iter().map(|l| DiffLine { content: l.content, kind: match l.kind {
                DiffLineKindWire::Context => DiffLineKind::Context,
                DiffLineKindWire::Added => DiffLineKind::Added,
                DiffLineKindWire::Removed => DiffLineKind::Removed,
                DiffLineKindWire::NoNewlineHint => DiffLineKind::NoNewlineHint,
            }}).collect() }).collect(),
    }
}

#[cfg(test)]
#[path = "git_rpc_tests.rs"]
mod tests;
