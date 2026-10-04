//! Only supported Git RPCs. Every path stays repository-relative host data.
use oximux_remote_session::RemoteSession;
use oximux_remote_proto::messages::{GitStatusWire, FileDiffWire, DiffStatusWire, DiffLineKindWire};
use oximux_core::{FileDiff, DiffStatus, DiffHunk, DiffLine, DiffLineKind};

#[derive(Clone)]
pub(super) enum Operation {
    Status,
    Diff { path: String, staged: bool, untracked: bool },
    Stage(String),
    Unstage(String),
    Commit(String),
}
impl Operation {
    pub fn mutates(&self) -> bool { matches!(self, Self::Stage(_) | Self::Unstage(_) | Self::Commit(_)) }
}

pub(super) enum Reply {
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

pub(super) async fn execute(session: &RemoteSession, id: &str, operation: Operation) -> Result<Reply, String> {
    let sha = match operation {
        Operation::Status => return rpc(session.git_status(id)).await.map(Reply::Status),
        Operation::Diff { path, staged, untracked } => return rpc(session.git_diff(id, &path, staged, untracked)).await
            .map(|files| Reply::Diff(files.into_iter().map(from_wire).collect())),
        Operation::Stage(path) => { rpc(session.git_stage(id, &[path])).await?; None }
        Operation::Unstage(path) => { rpc(session.git_unstage(id, &[path])).await?; None }
        Operation::Commit(message) => Some(rpc(session.git_commit(id, &message)).await?),
    };
    Ok(Reply::Mutated { sha, status: rpc(session.git_status(id)).await })
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
