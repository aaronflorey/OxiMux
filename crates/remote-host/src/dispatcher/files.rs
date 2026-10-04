//! File RPCs do not materialize an agent or interpret paths on the client.
use cap_std::fs::Dir;
use oximux_remote_proto::proto::{Request, Response, RpcError};
use super::Dispatcher;
use crate::auth::Peer;

impl Dispatcher {
    pub(super) async fn file_request(&self, peer: &Peer, request: Request) -> Response {
        let (session_id, write) = match &request {
            Request::ListDirectory { session_id, .. } | Request::ReadTextFile { session_id, .. } => (session_id, false),
            Request::WriteTextFile { session_id, .. } => (session_id, true),
            _ => return Response::Error(RpcError::Unsupported),
        };
        // Serialize version checks and replacement across all client connections.
        let guard = if write { Some(self.file_writes.clone().lock_owned().await) } else { None };
        let allowed = if write { self.auth.may_write(peer, session_id) }
            else { self.auth.is_allowed_for(peer, session_id) };
        if !allowed { return Response::Error(RpcError::Unauthorized); }
        let cwd = if let Some(handle) = self.registry.get(session_id) {
            handle.meta_snapshot().cwd
        } else if let Some(session) = self.catalog.as_ref().and_then(|catalog| {
            catalog.dormant().into_iter().find(|session| &session.session_id == session_id)
        }) { session.cwd } else { return Response::Error(RpcError::UnknownSession); };
        let Some(cwd) = cwd else {
            return Response::Error(RpcError::BadRequest("session has no working directory".into()));
        };
        match tokio::task::spawn_blocking(move || {
            // A dropped request cannot release the write lock while blocking I/O
            // continues: the owned guard lives until replacement finishes.
            let _guard = guard;
            let root = Dir::open_ambient_dir(cwd, cap_std::ambient_authority())
                .map_err(|_| RpcError::BadRequest("session directory unavailable".into()))?;
            match request {
                Request::ListDirectory { path, after, .. } => crate::files::list(&root, &path, after.as_deref()).map(Response::Directory),
                Request::ReadTextFile { path, .. } => crate::files::read(&root, &path).map(Response::TextFile),
                Request::WriteTextFile { path, text, version, .. } => crate::files::write(&root, &path, &text, &version).map(Response::TextFile),
                _ => Err(RpcError::Unsupported),
            }
        }).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => Response::Error(error),
            Err(_) => Response::Error(RpcError::Internal("file operation failed".into())),
        }
    }
}

#[cfg(test)]
mod tests;
