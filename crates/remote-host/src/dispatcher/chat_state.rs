//! Exact live snapshots for viewers that can join in the middle of a turn.
use oximux_agent_core::thread::ChatThread;
use oximux_remote_proto::proto::{Response, RpcError};
use super::Dispatcher;
use crate::auth::Peer;

impl Dispatcher {
    pub(super) fn fetch_chat_state(&self, peer: &Peer, session_id: &str) -> Response {
        if !self.auth.is_allowed_for(peer, session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let supports_steer = self.registry.get(session_id).is_some_and(|handle| handle.capabilities().supports_steer);
        let (seq, mut thread) = if let Some(handle) = self.registry.get(session_id) {
            let (seq, mut thread) = handle.chat_state_snapshot();
            let meta = handle.meta_snapshot();
            thread.model = meta.model.or(thread.model);
            thread.permission_mode = meta.permission_mode.or(thread.permission_mode);
            (seq, thread)
        } else {
            // A dormant session has no streaming window or answerable requests.
            let snapshot = match self.fetch_transcript(peer, session_id) {
                Response::SessionTranscript(snapshot) => snapshot,
                error => return error,
            };
            let Ok(entries) = serde_json::from_str(&snapshot.entries_json) else {
                return Response::Error(RpcError::Internal("invalid stored transcript".into()));
            };
            (0, ChatThread::rehydrated(Some(session_id.into()), snapshot.model, entries, vec![]))
        };
        thread.session_id = Some(session_id.into());
        // Initial history and live events obey the same capture-redaction policy.
        let result = (|| -> Result<String, serde_json::Error> {
            let entries = serde_json::to_string(&thread.entries)?;
            let (entries, _) = oximux_agent_core::redact::scrub_transcript(&entries);
            let (entries, _) = crate::transcript_budget::fit_images(
                &entries, crate::transcript_budget::IMAGE_BUDGET,
            );
            thread.entries = serde_json::from_str(&entries)?;
            serde_json::to_string(&thread)
        })();
        match result {
            Ok(thread_json) => Response::ChatState { session_id: session_id.into(), seq, thread_json, supports_steer },
            Err(_) => Response::Error(RpcError::Internal("chat snapshot serialization failed".into())),
        }
    }
}
