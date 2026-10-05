//! Session-scoped, host-relative text file RPCs (v28).
use oximux_remote_proto::files::{DirectoryWire, TextFileWire, FILES_MIN_VERSION};
use oximux_remote_proto::proto::{Request, Response, RpcError};
use super::{RemoteSession, Result};
use crate::error::SessionError;

impl RemoteSession {
    fn require_files(&self) -> Result<()> {
        if self.host_protocol_version().is_some_and(|v| v >= FILES_MIN_VERSION) { Ok(()) }
        else { Err(SessionError::Rpc(RpcError::Unsupported)) }
    }

    pub async fn list_directory(&self, session_id: &str, path: &str, after: Option<&str>) -> Result<DirectoryWire> {
        self.require_files()?;
        match self.call(Request::ListDirectory {
            session_id: session_id.into(), path: path.into(), after: after.map(str::to_owned),
        }).await? {
            Response::Directory(value) => Ok(value),
            Response::Error(error) => Err(SessionError::Rpc(error)),
            _ => Err(SessionError::Unexpected { expected: "Directory" }),
        }
    }

    pub async fn read_text_file(&self, session_id: &str, path: &str) -> Result<TextFileWire> {
        self.require_files()?;
        self.text_file(Request::ReadTextFile { session_id: session_id.into(), path: path.into() }).await
    }

    pub async fn write_text_file(&self, session_id: &str, path: &str, text: &str, version: &str) -> Result<TextFileWire> {
        self.require_files()?;
        self.text_file(Request::WriteTextFile {
            session_id: session_id.into(), path: path.into(), text: text.into(), version: version.into(),
        }).await
    }

    async fn text_file(&self, request: Request) -> Result<TextFileWire> {
        match self.call(request).await? {
            Response::TextFile(value) => Ok(value),
            Response::Error(error) => Err(SessionError::Rpc(error)),
            _ => Err(SessionError::Unexpected { expected: "TextFile" }),
        }
    }
}
