//! Paths are opaque host-relative values; this module has no local file access.
use oximux_remote_proto::files::{DirectoryWire, TextFileWire};
use oximux_remote_session::RemoteSession;

pub(super) enum Operation {
    List { path: String, after: Option<String> },
    Read(String),
    Save { path: String, text: String, version: String },
}
impl Operation {
    pub fn mutates(&self) -> bool { matches!(self, Self::Save { .. }) }
}
pub(super) enum Reply { Directory(DirectoryWire), Loaded(TextFileWire), Saved(TextFileWire) }

pub(super) async fn execute(session: &RemoteSession, id: &str, operation: Operation) -> Result<Reply, String> {
    let work = async {
        match operation {
            Operation::List { path, after } => session.list_directory(id, &path, after.as_deref()).await.map(Reply::Directory),
            Operation::Read(path) => session.read_text_file(id, &path).await.map(Reply::Loaded),
            Operation::Save { path, text, version } => session.write_text_file(id, &path, &text, &version).await.map(Reply::Saved),
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), work).await
        .map_err(|_| "File operation timed out. Keep your draft and reload the host file before retrying a save.".to_string())?
        .map_err(|error| error.to_string())
}
