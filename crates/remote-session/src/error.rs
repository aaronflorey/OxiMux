//! The client session's failure taxonomy — distinguishes a transport drop from a
//! codec fault from a protocol-level rejection the host reported.

use oximux_remote_proto::RpcError;

/// A remote-session call failure.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The transport failed (closed mid-call, I/O error).
    #[error("transport error: {0}")]
    Transport(String),
    /// A wire encode/decode fault (never expected on a healthy connection).
    #[error("wire codec error: {0}")]
    Wire(String),
    /// The host closed the connection (a clean `None` on receive).
    #[error("host closed the connection")]
    Closed,
    /// The host's reply was too large for one frame, so it could not be
    /// assembled. Distinct from [`Self::Transport`] because the connection
    /// survives it — only this one call failed — and distinct from
    /// [`Self::Closed`], which is what this used to surface as, sending everyone
    /// hunting for a dropped link that never happened.
    #[error("the reply was {len} bytes, over the {cap}-byte limit for one message")]
    OversizeReply { len: usize, cap: usize },
    /// The host reported a protocol-level failure.
    #[error("{}", rpc_message(.0))]
    Rpc(RpcError),
    /// The host sent a response that doesn't fit the request — a protocol
    /// desync, so the field names which reply was expected.
    #[error("unexpected response (expected {expected})")]
    Unexpected { expected: &'static str },
    /// The two ends cannot understand each other. Distinct from
    /// [`Self::Rpc`]`(RpcError::IncompatibleVersion)`, which is the host
    /// refusing *us*; this is the client refusing the *host*. Both numbers are
    /// carried so the UI can say which side needs updating rather than showing a
    /// generic connection failure — the actionable difference for the user.
    #[error("incompatible protocol (this build speaks {ours}, host speaks {theirs})")]
    IncompatibleVersion { ours: u32, theirs: u32 },
}

fn rpc_message(error: &RpcError) -> String {
    match error {
        RpcError::BadRequest(message) | RpcError::Internal(message) => message.clone(),
        RpcError::Unauthorized => "This enrollment does not have permission for this action.".into(),
        RpcError::UnknownSession => "This session is no longer available on the host.".into(),
        RpcError::AlreadyDecided => "This request has already been resolved.".into(),
        RpcError::Unsupported => "This host does not support this action.".into(),
        RpcError::IncompatibleVersion { host_version, host_min_compatible } =>
            format!("Update this client: the host uses protocol v{host_version} and requires v{host_min_compatible} or newer."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_errors_use_plain_messages_without_debug_wrappers() {
        assert_eq!(SessionError::Rpc(RpcError::BadRequest("session has no working directory".into())).to_string(),
            "session has no working directory");
        assert_eq!(SessionError::Rpc(RpcError::Internal("the session could not be started".into())).to_string(),
            "the session could not be started");
        assert!(SessionError::Rpc(RpcError::Unauthorized).to_string().contains("permission"));
    }
}
