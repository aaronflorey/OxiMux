//! Kill all, the daemon half: end every session the daemon holds while the
//! daemon itself keeps running.
//!
//! The foreground closes the open windows' terminal tabs first
//! (`workspace_root::kill_all`); this sweep then ends whatever the daemon still
//! lists — background projects, terminals inside agent chats, `oximux serve`'s.
//! Every close is awaited here rather than sent through the backend's
//! fire-and-forget close: the daemon replies to a close once its session has
//! ended — including a close of a session a tab close is already ending — so
//! when the sweep's closes have all answered, every session is gone. Older
//! daemons answer a connection's closes one at a time, so the grace is kept
//! at a manual tab close's, and answer a close of a session already closing
//! at once, so there the sweep may finish before the tabs' sessions have.
//!
//! The daemon stays up, so nothing here touches the heartbeat or
//! `expected_death`: a session closed by request ends with `Exit`, never
//! `DaemonLost`. The sweep holds `respawn_lock` so no respawn runs in the
//! middle of it, and — like a restart — it re-checks the epoch once it holds
//! the lock: a daemon replaced meanwhile is not the one the user was shown,
//! so its sessions are left alone.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use futures::future::{BoxFuture, Shared, join_all};
use oximux_relay_client::RelayClient;
use oximux_relay_proto::{ErrCode, Request, Response};

use super::RelayLifecycle;
use crate::shell::terminal_view::APP_QUITTING;

/// Between SIGTERM and the hangup that follows, per session: the same as a
/// manual tab close gives it.
const CLOSE_GRACE_MS: u32 = 500;
/// How long the sweep waits for the daemon to stop listing what it closed.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const DRAIN_POLL: Duration = Duration::from_millis(100);

/// One kill-all, shared by everyone who asked for it while it ran.
pub type KillAllFuture = Shared<BoxFuture<'static, Result<KillAllOutcome, KillAllError>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KillAllOutcome {
    /// Sessions the daemon listed when the sweep began.
    pub before: usize,
    /// Of those, the ones it still listed when the sweep stopped waiting.
    pub after: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KillAllError {
    #[error("the daemon did not answer ({0})")]
    Unreachable(Arc<str>),
    #[error("the app is quitting")]
    Quitting,
    /// The daemon was replaced while the sweep waited its turn; nothing was
    /// killed on the new one.
    #[error("the daemon restarted meanwhile")]
    Replaced,
    /// The sweep itself failed unexpectedly (see the log).
    #[error("kill all failed unexpectedly")]
    Internal,
}

/// Held by the running sweep; however it ends, the slot empties so the next
/// request runs.
struct InFlight(Arc<RelayLifecycle>);

impl Drop for InFlight {
    fn drop(&mut self) {
        *self.0.kill_all_in_flight.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

impl RelayLifecycle {
    /// The ids of every session the daemon lists. Runs on the relay runtime,
    /// so any executor may await it.
    pub async fn list_pty_ids(&self) -> Result<Vec<String>, KillAllError> {
        let client = self.client();
        self.handle
            .spawn(async move { list_ptys(&client).await })
            .await
            .unwrap_or_else(|err| {
                Err(if err.is_panic() { KillAllError::Internal } else { KillAllError::Quitting })
            })
    }

    /// End one session and wait until it has. `false` if the daemon refused
    /// or could not be asked. Runs on the relay runtime.
    pub async fn close_session(&self, pty_id: String) -> bool {
        let client = self.client();
        self.handle.spawn(async move { close_pty(&client, &pty_id).await }).await.unwrap_or(false)
    }

    /// End every session the daemon holds, or join the sweep already running.
    ///
    /// `shown` are the sessions the closed tabs showed. Their closes are
    /// already under way, so the daemon no longer lists them; the sweep closes
    /// them too so that it finishes only once they have ended. A joining
    /// caller's `shown` is not added to the running sweep.
    pub fn kill_all_sessions(self: &Arc<Self>, shown: Vec<String>) -> KillAllFuture {
        let mut slot = self.kill_all_in_flight.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(running) = slot.as_ref() {
            return running.clone();
        }
        let lifecycle = Arc::clone(self);
        // Spawned for the same reasons as a restart: the sweep needs this
        // runtime, and the slot is still locked, so the task's clear cannot
        // run before the slot is filled.
        let task = self.handle.spawn(async move {
            let _in_flight = InFlight(Arc::clone(&lifecycle));
            lifecycle.run_kill_all(shown).await
        });
        let shared = async move {
            task.await.unwrap_or_else(|err| {
                tracing::error!(?err, "relay kill-all task ended abnormally");
                Err(if err.is_panic() { KillAllError::Internal } else { KillAllError::Quitting })
            })
        }
        .boxed()
        .shared();
        *slot = Some(shared.clone());
        shared
    }

    async fn run_kill_all(&self, shown: Vec<String>) -> Result<KillAllOutcome, KillAllError> {
        if APP_QUITTING.load(Ordering::SeqCst) {
            return Err(KillAllError::Quitting);
        }
        // Snapshot BEFORE the lock, as a restart does: a respawn parked on it
        // may win it and bring up a daemon the user was never shown.
        let session = self.current_session();
        let _held = self.respawn_lock.lock().await;
        if APP_QUITTING.load(Ordering::SeqCst) {
            return Err(KillAllError::Quitting);
        }
        if self.current_session() != session {
            tracing::info!("relay was replaced while kill all waited; nothing killed");
            return Err(KillAllError::Replaced);
        }
        let client = self.client();
        let ids = list_ptys(&client).await?;
        let before = ids.len();
        let listed: HashSet<&String> = ids.iter().collect();
        let already_closing = shown.iter().filter(|id| !listed.contains(id));
        // All at once: each close waits out its own grace.
        join_all(ids.iter().chain(already_closing).map(|id| close_pty(&client, id))).await;
        let after = drain(&client, &ids, Instant::now() + DRAIN_TIMEOUT).await?;
        tracing::info!(before, after, "relay kill all");
        Ok(KillAllOutcome { before, after })
    }
}

async fn list_ptys(client: &RelayClient) -> Result<Vec<String>, KillAllError> {
    match client.request(Request::ListPtys).await {
        Ok(Response::PtyList(ptys)) => Ok(ptys.into_iter().map(|p| p.pty_id).collect()),
        Ok(other) => {
            tracing::warn!(?other, "kill all: unexpected reply to ListPtys");
            Err(KillAllError::Internal)
        }
        Err(err) => Err(KillAllError::Unreachable(err.to_string().into())),
    }
}

/// Close one session, answered once it has ended. One that already ended is
/// not a failure; any other miss shows up in the final count.
async fn close_pty(client: &RelayClient, pty_id: &str) -> bool {
    let request = Request::Close { pty_id: pty_id.to_owned(), grace_ms: CLOSE_GRACE_MS };
    match client.request(request).await {
        Ok(Response::Ok | Response::Err { code: ErrCode::PtyNotFound, .. }) => true,
        Ok(other) => {
            tracing::warn!(pty_id, ?other, "kill all: close refused");
            false
        }
        Err(err) => {
            tracing::warn!(pty_id, %err, "kill all: close failed");
            false
        }
    }
}

/// How many of `ids` the daemon still lists once they are gone or `deadline`
/// passes — the closes it refused or never answered. Only these: a session
/// opened meanwhile is not one the sweep failed to end.
async fn drain(client: &RelayClient, ids: &[String], deadline: Instant) -> Result<usize, KillAllError> {
    loop {
        let live: HashSet<String> = list_ptys(client).await?.into_iter().collect();
        let left = ids.iter().filter(|id| live.contains(*id)).count();
        if left == 0 || Instant::now() >= deadline {
            return Ok(left);
        }
        tokio::time::sleep(DRAIN_POLL).await;
    }
}
