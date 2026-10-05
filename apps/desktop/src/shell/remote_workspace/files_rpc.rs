//! Paths are opaque host-relative values; this module has no local file access.
use oximux_remote_proto::files::{DirectoryWire, TextFileWire};
use oximux_remote_session::RemoteSession;
use super::Root;

pub(super) enum Operation {
    List { path: String, after: Option<String> },
    Read(String),
    Save { path: String, text: String, version: String },
}
impl Operation {
    pub fn mutates(&self) -> bool { matches!(self, Self::Save { .. }) }
}
pub(super) enum Reply { Directory(DirectoryWire), Loaded(TextFileWire), Saved(TextFileWire) }

pub(super) async fn execute(session: &RemoteSession, root: &Root, operation: Operation) -> Result<Reply, String> {
    let work = async {
        match (root, operation) {
            (Root::Session(id), Operation::List { path, after }) => session.list_directory(id, &path, after.as_deref()).await.map(Reply::Directory),
            (Root::Project(project), Operation::List { path, after }) => session.browse_list_directory(project, &path, after.as_deref()).await.map(Reply::Directory),
            (Root::Session(id), Operation::Read(path)) => session.read_text_file(id, &path).await.map(Reply::Loaded),
            (Root::Project(project), Operation::Read(path)) => session.browse_read_text_file(project, &path).await.map(Reply::Loaded),
            (Root::Session(id), Operation::Save { path, text, version }) => session.write_text_file(id, &path, &text, &version).await.map(Reply::Saved),
            (Root::Project(project), Operation::Save { path, text, version }) => session.browse_write_text_file(project, &path, &text, &version).await.map(Reply::Saved),
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), work).await
        .map_err(|_| "File operation timed out. Keep your draft and reload the host file before retrying a save.".to_string())?
        .map_err(|error| error.to_string())
}
