//! One consumer of the terminal stream per connection. All RPCs run off GPUI.
use std::{collections::HashMap, sync::Arc, time::Duration};
use futures::StreamExt;
use oximux_pty::remote_backend::{RemoteTerminalCommand, RemoteTerminalControl, RemoteTerminalFeed, RemoteTerminalSender};
use oximux_remote_session::{RemoteSession, TerminalPush};
use tokio::sync::mpsc;
use super::Update;

pub(super) enum Command {
    Open(String, RemoteTerminalFeed),
    Drive(String, RemoteTerminalCommand),
    Refresh,
}

pub(super) struct TerminalDriver {
    pub tx: mpsc::UnboundedSender<Command>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for TerminalDriver { fn drop(&mut self) { self.task.abort(); } }

pub(super) fn sender(tx: mpsc::UnboundedSender<Command>, id: String) -> RemoteTerminalSender {
    Arc::new(move |command| tx.send(Command::Drive(id.clone(), command))
        .map_err(|_| anyhow::anyhow!("Remote terminal connection ended")))
}

impl TerminalDriver {
    pub fn start(session: Arc<RemoteSession>, initial: Vec<(String, RemoteTerminalControl)>,
        tx: mpsc::UnboundedSender<(u64, Update)>, epoch: u64, revision: u64) -> Self {
        let (commands, mut rx) = mpsc::unbounded_channel();
        let initial: Vec<_> = initial.into_iter().map(|(id, control)| {
            let feed = control.bind(Some(sender(commands.clone(), id.clone())), false);
            (id, feed)
        }).collect();
        let task = tokio::spawn(async move {
            let mut frames = session.take_terminals().expect("one terminal driver per connection");
            let mut controls = HashMap::new();
            // A close/reopen can leave older barriers buffered. Only install the
            // last outstanding replay for that PTY, never an obsolete view's replay.
            let mut barriers = HashMap::<String, usize>::new();
            let report = |error: String| { let _ = tx.send((epoch, Update::ListingError(revision, error))); };
            for (id, control) in initial {
                if !control.is_live() { continue; }
                controls.insert(id.clone(), control);
                match attach(&session, &id).await {
                    Ok(()) => { *barriers.entry(id).or_default() += 1; }
                    Err(e) => report(e),
                }
            }
            list(&session, &tx, epoch, revision).await;
            loop {
                tokio::select! {
                    command = rx.recv() => match command {
                        Some(Command::Open(id, control)) => {
                            controls.insert(id.clone(), control);
                            match attach(&session, &id).await {
                                Ok(()) => { *barriers.entry(id).or_default() += 1; }
                                Err(e) => report(e),
                            }
                        }
                        Some(Command::Refresh) => {
                            list(&session, &tx, epoch, revision).await;
                            for (id, control) in &controls {
                                if !control.is_live() { continue; }
                                control.gap();
                                match attach(&session, id).await {
                                    Ok(()) => { *barriers.entry(id.clone()).or_default() += 1; }
                                    Err(e) => report(e),
                                }
                            }
                        }
                        Some(Command::Drive(id, command)) => {
                            let result = match command {
                                RemoteTerminalCommand::Detach => {
                                    controls.remove(&id);
                                    rpc(session.term_detach(&id)).await
                                }
                                RemoteTerminalCommand::Input(bytes) => {
                                    if !controls.get(&id).is_some_and(|c| c.is_live()) { continue; }
                                    rpc(session.term_input(&id, &bytes)).await
                                }
                                RemoteTerminalCommand::Resize(cols, rows) => {
                                    let Some(control) = controls.get(&id) else { continue; };
                                    if !control.is_live() { continue; }
                                    control.gap();
                                    match rpc(session.term_resize(&id, cols, rows)).await {
                                        Ok(()) => match attach(&session, &id).await {
                                            Ok(()) => { *barriers.entry(id.clone()).or_default() += 1; Ok(()) }
                                            Err(e) => Err(e),
                                        },
                                        Err(e) => Err(e),
                                    }
                                }
                            };
                            if let Err(e) = result { report(format!("Terminal {id}: {e}")); }
                        }
                        None => break,
                    },
                    frame = frames.next() => {
                        let Some(frame) = frame else { break; };
                        match frame {
                            TerminalPush::Attached { pty_id, replay, cols, rows } => {
                                if let Some(pending) = barriers.get_mut(&pty_id) {
                                    *pending = pending.saturating_sub(1);
                                    if *pending != 0 { continue; }
                                }
                                if let Some(control) = controls.get(&pty_id)
                                    && let Err(e) = control.replay(cols, rows, &replay) { report(e.to_string()); }
                            }
                            TerminalPush::Output { pty_id, bytes } => {
                                if let Some(control) = controls.get(&pty_id) { control.output(&bytes); }
                            }
                            TerminalPush::Gapped { pty_id } => {
                                // A pending later snapshot already covers this gap.
                                if barriers.get(&pty_id).is_some_and(|pending| *pending != 0) { continue; }
                                if let Some(control) = controls.get(&pty_id) && control.is_live() {
                                    control.gap();
                                    match attach(&session, &pty_id).await {
                                        Ok(()) => { *barriers.entry(pty_id).or_default() += 1; }
                                        Err(e) => report(e),
                                    }
                                }
                            }
                            TerminalPush::Exited { pty_id, code } => {
                                if let Some(control) = controls.remove(&pty_id) { control.exit(code); }
                            }
                        }
                    }
                }
            }
        });
        Self { tx: commands, task }
    }
}

async fn rpc<T>(future: impl std::future::Future<Output = Result<T, oximux_remote_session::SessionError>>) -> Result<T, String> {
    tokio::time::timeout(Duration::from_secs(15), future).await
        .map_err(|_| "Terminal operation timed out".to_string())?.map_err(|e| e.to_string())
}
async fn attach(session: &RemoteSession, id: &str) -> Result<(), String> {
    // The ordered stream barrier owns replay installation, not this return value.
    rpc(session.term_attach(id)).await.map(|_| ())
}
async fn list(session: &RemoteSession, tx: &mpsc::UnboundedSender<(u64, Update)>, epoch: u64, revision: u64) {
    let result = rpc(session.list_terminals()).await;
    let _ = tx.send((epoch, Update::Terminals(revision, result)));
}

#[cfg(test)]
#[path = "terminal_driver_tests.rs"]
mod tests;
