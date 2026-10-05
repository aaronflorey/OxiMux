//! Synchronous AgentConnection seam, called only by the remote view's blocking worker.
use std::{sync::Arc, time::Duration};
use anyhow::Result;
use oximux_agents::thread::{AgentCapabilities, AgentConnection, AskQuestion, ChatImage,
    ModeChoice, ModelChoice, PermissionDecision, QuestionAnswers};
use oximux_remote_proto::proto::SessionChoices;
use oximux_remote_session::{RemoteSession, SessionError};

pub(super) struct RemoteAgentConnection {
    pub session: Arc<RemoteSession>,
    pub id: String,
    pub choices: SessionChoices,
    pub read_only: bool,
    pub supports_steer: bool,
    pub runtime: tokio::runtime::Handle,
}

impl RemoteAgentConnection {
    fn rpc<T>(&self, future: impl std::future::Future<Output = Result<T, SessionError>>) -> Result<T> {
        anyhow::ensure!(!self.read_only, "this enrollment is read-only");
        // The UI awaits the worker's actual result; queueing it is never an ack.
        self.runtime.block_on(async { tokio::time::timeout(Duration::from_secs(30), future).await })
            .map_err(|_| anyhow::anyhow!("host did not acknowledge within 30 seconds; check the server transcript before retrying"))?
            .map_err(Into::into)
    }
}

impl AgentConnection for RemoteAgentConnection {
    fn send_user_message(&self, text: &str) -> Result<()> { self.send_user_message_with_images(text, &[]) }
    fn send_user_message_with_images(&self, text: &str, images: &[ChatImage]) -> Result<()> {
        self.rpc(self.session.send_prompt(&self.id, text, images, 0))
    }
    fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        self.rpc(self.session.resolve_permission(&self.id, request_id, &decision)).map(|_| ())
    }
    fn answer_question(&self, request_id: &str, questions: &[AskQuestion], answers: &QuestionAnswers) -> Result<()> {
        self.rpc(self.session.answer_question(&self.id, request_id, questions, answers)).map(|_| ())
    }
    fn cancel(&self) -> Result<()> { self.rpc(self.session.cancel(&self.id)) }
    fn steer(&self, text: &str) -> Result<()> { self.rpc(self.session.steer(&self.id, text)) }
    fn set_model(&self, model: &str) -> Result<()> { self.rpc(self.session.set_model(&self.id, model)) }
    fn set_mode(&self, mode: &str) -> Result<()> { self.rpc(self.session.set_permission_mode(&self.id, mode)) }
    // A viewer never owns the remote process. Stop is the explicit cancel RPC.
    fn shutdown(&self) {}
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities { supports_modes: !self.read_only && !self.choices.modes.is_empty(),
            supports_slash: !self.read_only, supports_steer: !self.read_only && self.supports_steer, emits_usage: true, ..Default::default() }
    }
    fn models(&self) -> Vec<ModelChoice> {
        if self.read_only { return vec![]; }
        self.choices.models.iter().map(|c| ModelChoice { wire: c.id.clone(), label: c.label.clone(), description: c.description.clone() }).collect()
    }
    fn permission_modes(&self) -> Vec<ModeChoice> {
        if self.read_only { return vec![]; }
        self.choices.modes.iter().map(|c| ModeChoice { wire: c.id.clone(), label: c.label.clone() }).collect()
    }
    fn default_model(&self) -> Option<String> { self.choices.current_model.clone() }
    fn default_mode(&self) -> Option<String> { self.choices.current_mode.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::{session_registry::SessionRegistry, thread::StubConnection};
    use oximux_remote_host::{AuthStore, Dispatcher, PairingSlot};
    use oximux_remote_proto::{PairingTicket, testing::duplex_pair};
    use oximux_remote_session::ClientSigner;

    struct TrackedAgent { inner: StubConnection, stops: Arc<std::sync::atomic::AtomicUsize> }
    impl AgentConnection for TrackedAgent {
        fn send_user_message(&self, text: &str) -> Result<()> { self.inner.send_user_message(text) }
        fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()> {
            self.inner.resolve_permission(request_id, decision)
        }
        fn shutdown(&self) { self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst); }
        fn cancel(&self) -> Result<()> {
            self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn rpc_result_is_real_and_shutdown_does_not_cancel_server_work() {
        let registry = Arc::new(SessionRegistry::new());
        let (stub, _, _) = StubConnection::new();
        let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        registry.register("server-session".into(), Arc::new(TrackedAgent { inner: stub, stops: stops.clone() }));
        let auth = Arc::new(AuthStore::new());
        auth.set_pairing(PairingSlot::new([4; 16], None, false));
        let dispatcher = Dispatcher::new(registry.clone(), auth.clone()).with_clock(|| 10);
        let (transport, server) = duplex_pair();
        let serving = tokio::spawn(async move { dispatcher.serve(&server).await });
        let signer = ClientSigner::from_seed(&[3; 32]);
        let pubkey = signer.public_key();
        let session = Arc::new(RemoteSession::new(Arc::new(transport), signer));
        let pump = session.take_pump().unwrap();
        let pumping = tokio::spawn(pump.run());
        session.pair(&PairingTicket { endpoint_id: [0; 32], handshake_secret: [4; 16], session_id: None }, "desktop", 10).await.unwrap();
        let connection = Arc::new(RemoteAgentConnection { id: "server-session".into(), session: session.clone(),
            choices: session.list_choices("server-session").await.unwrap(), read_only: false, supports_steer: false, runtime: tokio::runtime::Handle::current() });
        let worker = connection.clone();
        tokio::task::spawn_blocking(move || worker.send_user_message("accepted")).await.unwrap().unwrap();
        assert_eq!(session.fetch_chat_state("server-session").await.unwrap().thread().entries.len(), 1);
        auth.set_read_only(&pubkey, true);
        let worker = connection.clone();
        assert!(tokio::task::spawn_blocking(move || worker.send_user_message("refused")).await.unwrap().is_err());
        assert_eq!(session.fetch_chat_state("server-session").await.unwrap().thread().entries.len(), 1, "no phantom send");
        connection.shutdown();
        drop(connection);
        assert!(registry.get("server-session").is_some(), "viewer teardown does not unregister the host session");
        assert!(session.fetch_chat_state("server-session").await.unwrap().thread().turn_active, "turn keeps running");
        drop(session);
        pumping.await.unwrap().unwrap();
        serving.await.unwrap();
        assert_eq!(stops.load(std::sync::atomic::Ordering::SeqCst), 0,
            "neither adapter teardown nor transport close sends Cancel or shutdown to the host agent");
        assert!(registry.get("server-session").unwrap().chat_state_snapshot().1.turn_active,
            "the host retains its active turn after the whole client connection closes");
    }
}
