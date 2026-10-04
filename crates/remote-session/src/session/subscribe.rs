//! Opening a session's live stream. The pushed frames are read off the demux
//! event stream ([`RemoteSession::take_events`]); the fold lives in
//! [`crate::subscription`].

use oximux_remote_proto::HostEvent;
use oximux_remote_proto::proto::{Request, Response};

use super::{RemoteSession, Result};
use crate::error::SessionError;

impl RemoteSession {
    /// Fetch a live fold before subscribing from its exact cursor (v27 host).
    /// Unlike cold transcript restore, pending requests and streaming windows
    /// remain active. Unsupported older hosts return their normal RPC error.
    pub async fn fetch_chat_state(&self, session_id: &str) -> Result<crate::SessionSubscription> {
        match self.call(Request::FetchChatState { session_id: session_id.into() }).await? {
            Response::ChatState { session_id: returned, seq, thread_json, supports_steer } if returned == session_id => {
                let thread: oximux_agent_core::thread::ChatThread = serde_json::from_str(&thread_json)
                    .map_err(|e| SessionError::Wire(e.to_string()))?;
                if !thread.valid_live_snapshot() || thread.session_id.as_deref() != Some(session_id) {
                    return Err(SessionError::Wire("invalid live chat snapshot".into()));
                }
                let mut subscription = crate::SessionSubscription::from_snapshot(session_id, thread, seq);
                subscription.set_supports_steer(supports_steer);
                Ok(subscription)
            }
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "ChatState for requested session" }),
        }
    }

    /// Current server-enforced enrollment tier and creation access.
    pub async fn client_access(&self) -> Result<(bool, bool)> {
        match self.call(Request::ClientAccess).await? {
            Response::ClientAccess { read_only, can_create_sessions } => Ok((read_only, can_create_sessions)),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "ClientAccess" }),
        }
    }

    /// Snapshot first, then extend from its cursor. Never fold a partial ring as
    /// though it were the full transcript.
    pub async fn open_subscription(&self, session_id: &str) -> Result<crate::SessionSubscription> {
        let mut subscription = self.fetch_chat_state(session_id).await?;
        self.resume_subscription(&mut subscription).await?;
        Ok(subscription)
    }

    /// Re-establish the live stream, replacing expired history with a fresh fold.
    pub async fn resume_subscription(&self, sub: &mut crate::SessionSubscription) -> Result<()> {
        let frames = self.subscribe(sub.session_id(), sub.last_seq()).await?;
        if matches!(sub.apply_batch(&frames)?, crate::FoldOutcome::Gap { .. }) {
            *sub = self.fetch_chat_state(sub.session_id()).await?;
            let frames = self.subscribe(sub.session_id(), sub.last_seq()).await?;
            if matches!(sub.apply_batch(&frames)?, crate::FoldOutcome::Gap { .. }) {
                return Err(SessionError::Wire("host history expired while resubscribing".into()));
            }
        }
        Ok(())
    }

    /// Backfill a dropped live frame; an aged-out ring heals from an exact fold.
    pub async fn apply_live_frame(&self, sub: &mut crate::SessionSubscription, frame: &HostEvent) -> Result<()> {
        if matches!(sub.apply(frame)?, crate::FoldOutcome::Gap { .. }) {
            let frames = self.events_since(sub.session_id(), sub.last_seq()).await?;
            if matches!(sub.apply_batch(&frames)?, crate::FoldOutcome::Gap { .. })
                || matches!(sub.apply(frame)?, crate::FoldOutcome::Gap { .. }) {
                *sub = self.fetch_chat_state(sub.session_id()).await?;
                // A frame already received must be represented by the snapshot.
                if matches!(sub.apply(frame)?, crate::FoldOutcome::Gap { .. }) {
                    return Err(SessionError::Wire("host snapshot predates its live event".into()));
                }
            }
        }
        Ok(())
    }

    /// Release this viewer without cancelling the server session (v27).
    pub async fn unsubscribe(&self, session_id: &str) -> Result<()> {
        self.expect_ack(Request::Unsubscribe { session_id: session_id.into() }).await
    }

    /// Subscribe to a session's live stream. Returns the backlog after `after_seq`
    /// (the immediate `Events` reply); each subsequent live frame arrives on the
    /// event stream taken with [`Self::take_events`].
    ///
    /// The host awaits the backlog send before it forwards any live frame, so the
    /// backlog is complete as of the subscribe. Because every RPC rides the demux
    /// pump, other requests stay safe to issue on this connection while the stream
    /// runs — the pump keeps pushed events and RPC replies from colliding.
    pub async fn subscribe(&self, session_id: &str, after_seq: u64) -> Result<Vec<HostEvent>> {
        let req =
            Request::Subscribe { session_id: session_id.to_string(), after_seq: Some(after_seq) };
        match self.call(req).await? {
            Response::Events(backlog) => Ok(backlog),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Events" }),
        }
    }
}
