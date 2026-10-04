//! Drive the production terminal actor against the real authenticated dispatcher.
use super::*;
use std::sync::{atomic::{AtomicUsize, Ordering}, Mutex};
use oximux_pty::{TerminalBackend, remote_backend::{RemoteTerminalBackend, REMOTE_SESSION}};
use oximux_remote_host::{AttachmentId, AuthStore, Dispatcher, PairingSlot,
    TerminalAttach, TerminalError, TerminalFrame, TerminalSource};
use oximux_remote_proto::{PairingTicket, messages::TerminalSummary, testing::duplex_pair};
use oximux_remote_session::ClientSigner;

struct Terminals {
    streams: Mutex<std::collections::VecDeque<mpsc::Receiver<TerminalFrame>>>,
    attaches: AtomicUsize,
    detaches: AtomicUsize,
}
#[async_trait::async_trait]
impl TerminalSource for Terminals {
    async fn list(&self) -> Result<Vec<TerminalSummary>, TerminalError> {
        Ok(vec![TerminalSummary { pty_id: "server-pty".into(), cwd: "/server/only".into(), cols: 5, rows: 2 }])
    }
    async fn attach(&self, _: &str) -> Result<(TerminalAttach, mpsc::Receiver<TerminalFrame>), TerminalError> {
        let rx = self.streams.lock().unwrap().pop_front().ok_or(TerminalError::Unavailable)?;
        let n = self.attaches.fetch_add(1, Ordering::SeqCst) + 1;
        Ok((TerminalAttach { replay: format!("snap{n}").into_bytes(), cols: 5, rows: 2,
            attachment: AttachmentId(n as u64) }, rx))
    }
    async fn input(&self, _: &str, _: &[u8]) -> Result<(), TerminalError> { Err(TerminalError::Unavailable) }
    async fn resize(&self, _: &str, _: AttachmentId, _: u16, _: u16) -> Result<(), TerminalError> { Ok(()) }
    async fn detach(&self, _: &str, _: AttachmentId) { self.detaches.fetch_add(1, Ordering::SeqCst); }
}

async fn screen(backend: &RemoteTerminalBackend, changes: &mut mpsc::UnboundedReceiver<()>, text: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if backend.snapshot(REMOTE_SESSION).unwrap().rows_text(0, 2).contains(text) { return; }
            changes.recv().await.expect("renderer signal");
        }
    }).await.expect("terminal screen updated");
}

#[tokio::test]
async fn terminal_actor_recovers_gaps_surfaces_rpc_errors_and_detaches() {
    let (first, rx1) = mpsc::channel(8);
    let (_second, rx2) = mpsc::channel(8);
    let (_third, rx3) = mpsc::channel(8);
    let source = Arc::new(Terminals { streams: Mutex::new([rx1, rx2, rx3].into()),
        attaches: AtomicUsize::new(0), detaches: AtomicUsize::new(0) });
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new([2; 16], None, false));
    let dispatcher = Arc::new(Dispatcher::new(Arc::new(oximux_agents::session_registry::SessionRegistry::new()), auth)
        .with_clock(|| 123).with_terminals(source.clone()));
    let (client, server) = duplex_pair();
    let serving = dispatcher.clone();
    let host = tokio::spawn(async move { serving.serve(&server).await; });
    let session = Arc::new(RemoteSession::new(Arc::new(client), ClientSigner::from_seed(&[7; 32])));
    let pump = session.take_pump().unwrap();
    let pump = tokio::spawn(pump.run());
    session.pair(&PairingTicket { endpoint_id: [0; 32], handshake_secret: [2; 16], session_id: None }, "desktop", 123).await.unwrap();
    let (mut backend, control) = RemoteTerminalBackend::new();
    let (changed, mut changes) = mpsc::unbounded_channel();
    backend.set_output_waker(REMOTE_SESSION, Arc::new(move || { let _ = changed.send(()); }));
    let (tx, mut updates) = mpsc::unbounded_channel();
    let driver = TerminalDriver::start(session.clone(), vec![("server-pty".into(), control.clone())], tx, 1, 2);
    screen(&backend, &mut changes, "snap1").await;
    control.set_writable(true);
    first.send(TerminalFrame::Gapped).await.unwrap();
    screen(&backend, &mut changes, "snap2").await;
    assert_eq!(source.attaches.load(Ordering::SeqCst), 2);
    assert_eq!(source.detaches.load(Ordering::SeqCst), 1, "recovery released the old attachment");
    backend.write(REMOTE_SESSION, b"refused").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some((epoch, Update::ListingError(revision, error))) = updates.recv().await {
                assert_eq!((epoch, revision), (1, 2));
                assert!(error.contains("Terminal server-pty"));
                return;
            }
        }
    }).await.expect("RPC failure surfaced");
    // Keep the same emulator/view across a lost connection. A new authenticated
    // session must reattach and install a fresh replay before allowing input.
    control.bind(None, false);
    assert!(backend.write(REMOTE_SESSION, b"offline").is_err());
    drop(driver);
    drop(session);
    pump.await.unwrap().unwrap();
    host.await.unwrap();
    let (client, server) = duplex_pair();
    let host = tokio::spawn(async move { dispatcher.serve(&server).await; });
    let session = Arc::new(RemoteSession::new(Arc::new(client), ClientSigner::from_seed(&[7; 32])));
    let pump = tokio::spawn(session.take_pump().unwrap().run());
    session.connect().await.unwrap();
    let (tx, _updates) = mpsc::unbounded_channel();
    let driver = TerminalDriver::start(session.clone(), vec![("server-pty".into(), control.clone())], tx, 1, 3);
    screen(&backend, &mut changes, "snap3").await;
    assert!(backend.write(REMOTE_SESSION, b"unverified").is_err());
    let (read_only, _) = session.client_access().await.unwrap();
    control.set_writable(!read_only);
    backend.close(REMOTE_SESSION).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while source.detaches.load(Ordering::SeqCst) != 3 { tokio::task::yield_now().await; }
    }).await.expect("view close detached without a termination RPC");
    drop(driver);
    drop(session);
    pump.await.unwrap().unwrap();
    host.await.unwrap();
}
