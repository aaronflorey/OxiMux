//! Background connection owner. Cancellation closes the client transport only.
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::channel::oneshot;
use oximux_remote_proto::PairingTicket;
use oximux_remote_session::{Bootstrap, Sleeper, Connector, ConnectError, maintain_connection};
use oximux_remote_session::hosts_store::{HostEntry, HostsFile, config_dir, parse_endpoint_id};
use tokio::sync::mpsc;

use super::Update;

struct TokioSleeper;
#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) { tokio::time::sleep(duration).await; }
}

struct DialDeadline(oximux_remote_iroh::IrohConnector);
#[async_trait]
impl Connector for DialDeadline {
    async fn connect(&self) -> Result<Arc<dyn oximux_remote_proto::Transport>, ConnectError> {
        tokio::time::timeout(Duration::from_secs(20), self.0.connect()).await
            .map_err(|_| ConnectError::Unreachable("dial timed out; check the host and network".into()))?
    }
}

pub(crate) struct ConnectionJob {
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for ConnectionJob {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() { let _ = shutdown.send(()); }
        // Also cancels a pending endpoint bind before the driver owns shutdown.
        self.task.abort();
    }
}

pub(crate) fn start(host: HostEntry, ticket: Option<PairingTicket>, epoch: u64,
    tx: mpsc::UnboundedSender<(u64, Update)>) -> ConnectionJob {
    let (shutdown, stop) = oneshot::channel();
    let task = tokio::spawn(async move {
        let result = async {
            let endpoint_id = parse_endpoint_id(&host.endpoint_id)?;
            let selected = host.clone();
            let is_pairing = ticket.is_some();
            let (host, signer) = tokio::task::spawn_blocking(move || {
                let dir = config_dir()?;
                if is_pairing {
                    oximux_remote_session::enrollment::prepare_pairing(&dir, selected)
                } else {
                    oximux_remote_session::enrollment::load_host(&dir, &selected)
                }
            }).await??;
            let _ = tx.send((epoch, Update::Enrollment(host.clone())));
            let endpoint = tokio::time::timeout(Duration::from_secs(20),
                oximux_remote_iroh::bind_client()).await??;
            let connector = oximux_remote_iroh::IrohConnector::new(endpoint, endpoint_id)?;
            let mut pairing = ticket.is_some();
            let bootstrap = match ticket {
                Some(ticket) => Bootstrap::Pair { ticket, device_name: "OxiMux desktop".into(),
                    at_secs: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() },
                None => Bootstrap::Resume,
            };
            let state_tx = tx.clone();
            let live_tx = tx.clone();
            maintain_connection(Arc::new(DialDeadline(connector)), Arc::new(TokioSleeper), signer, None,
                bootstrap, stop,
                move |state| { let _ = state_tx.send((epoch, Update::State(state))); },
                move |session| {
                    let _ = live_tx.send((epoch, Update::Connected(session)));
                    if pairing {
                        pairing = false;
                        let host = host.clone();
                        let tx = live_tx.clone();
                        tokio::task::spawn_blocking(move || {
                            let saved = (|| {
                                let dir = config_dir()?;
                                let hosts = HostsFile::update(&dir, |hosts| {
                                    // Desktop selection never changes the CLI default.
                                    hosts.upsert(host.clone());
                                    Ok(())
                                })?;
                                oximux_remote_session::enrollment::finish_pairing(&dir, &host)?;
                                Ok::<_, oximux_remote_session::StoreError>(hosts)
                            })().map_err(|e| e.to_string());
                            let _ = tx.send((epoch, Update::Hosts(saved)));
                        });
                    }
                }).await;
            Ok::<_, anyhow::Error>(())
        }.await;
        if let Err(error) = result {
            let _ = tx.send((epoch, Update::State(oximux_remote_session::ConnState::Unreachable {
                cause: error.to_string(),
            })));
        }
    });
    ConnectionJob { shutdown: Some(shutdown), task }
}
