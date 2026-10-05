//! One event consumer per host connection; cursors survive reconnects.
use std::{collections::{HashMap, HashSet}, sync::Arc};
use futures::{FutureExt, StreamExt};
use oximux_remote_session::{RemoteSession, SessionSubscription};
use tokio::sync::{Mutex, mpsc};
use super::Update;

pub(super) type Subscriptions = Arc<Mutex<HashMap<String, SessionSubscription>>>;
pub(super) enum Command { Open(String, u64), Close(String), Refresh(String) }
pub(super) struct ChatDriver { pub tx: mpsc::UnboundedSender<Command>, task: tokio::task::JoinHandle<()> }
impl Drop for ChatDriver { fn drop(&mut self) { self.task.abort(); } }

impl ChatDriver {
    pub fn start(session: Arc<RemoteSession>, subscriptions: Subscriptions, initial: Vec<(String, u64)>,
        tx: mpsc::UnboundedSender<(u64, Update)>, epoch: u64, revision: u64) -> Self {
        let (commands, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut events = session.take_events().expect("one chat driver per connection");
            let mut generations = HashMap::new();
            {
                let ids: HashSet<_> = initial.iter().map(|(id, _)| id.clone()).collect();
                subscriptions.lock().await.retain(|id, _| ids.contains(id));
            }
            for (id, generation) in initial {
                open(&session, &subscriptions, &tx, epoch, revision, &id, generation).await;
                generations.insert(id, generation);
            }
            loop {
                tokio::select! {
                    command = rx.recv() => match command {
                        Some(Command::Open(id, generation)) => {
                            open(&session, &subscriptions, &tx, epoch, revision, &id, generation).await;
                            generations.insert(id, generation);
                        }
                        Some(Command::Refresh(id)) => {
                            if let Some(generation) = generations.get(&id).copied() {
                                // A command can settle a request without a backend
                                // event; refresh the shared cursor/fold as well as UI.
                                subscriptions.lock().await.remove(&id);
                                open(&session, &subscriptions, &tx, epoch, revision, &id, generation).await;
                            }
                        }
                        Some(Command::Close(id)) => {
                            generations.remove(&id);
                            subscriptions.lock().await.remove(&id);
                            if let Err(error) = tokio::time::timeout(std::time::Duration::from_secs(10), session.unsubscribe(&id)).await
                                .map_err(|_| "Viewer detach timed out".to_string()).and_then(|r| r.map_err(|e| e.to_string())) {
                                let _ = tx.send((epoch, Update::ListingError(revision, error.to_string())));
                            }
                        }
                        None => break,
                    },
                    frame = events.next() => {
                        let Some(frame) = frame else { break; };
                        let mut batch = vec![frame];
                        // Publish once per drained batch, rather than clone the whole
                        // transcript for every streaming delta.
                        while batch.len() < 128 {
                            match events.next().now_or_never() { Some(Some(frame)) => batch.push(frame), _ => break }
                        }
                        let mut dirty = HashSet::new();
                        let mut states = subscriptions.lock().await;
                        for frame in batch {
                            let Some(generation) = generations.get(&frame.session_id).copied() else { continue; };
                            let Some(sub) = states.get_mut(&frame.session_id) else { continue; };
                            match tokio::time::timeout(std::time::Duration::from_secs(30), session.apply_live_frame(sub, &frame)).await
                                .map_err(|_| "Chat gap recovery timed out".to_string()).and_then(|r| r.map_err(|e| e.to_string())) {
                                Ok(()) => { dirty.insert(frame.session_id); }
                                Err(error) => { let _ = tx.send((epoch, Update::Chat(revision, frame.session_id, generation, Box::new(Err(error.to_string()))))); }
                            }
                        }
                        for id in dirty {
                            let thread = states[&id].thread().clone();
                            let _ = tx.send((epoch, Update::Chat(revision, id.clone(), generations[&id], Box::new(Ok((states[&id].last_seq(), thread, states[&id].supports_steer(), None))))));
                        }
                    }
                }
            }
        });
        Self { tx: commands, task }
    }
}

async fn open(session: &RemoteSession, subscriptions: &Subscriptions, tx: &mpsc::UnboundedSender<(u64, Update)>,
    epoch: u64, revision: u64, id: &str, generation: u64) {
    let update = tokio::time::timeout(std::time::Duration::from_secs(30), async {
    let mut states = subscriptions.lock().await;
    let result = if let Some(sub) = states.get_mut(id) {
        match session.resume_subscription(sub).await {
            Ok(()) => match session.fetch_chat_state(id).await { Ok(fresh) => { *sub = fresh; Ok(()) }, Err(error) => Err(error) },
            Err(error) => Err(error),
        }
    } else {
        match session.open_subscription(id).await {
            Ok(sub) => { states.insert(id.into(), sub); Ok(()) }, Err(error) => Err(error),
        }
    };
    match result {
        Ok(()) => Ok((states[id].last_seq(), states[id].thread().clone(), states[id].supports_steer(), session.list_choices(id).await.ok())),
        Err(error) => Err(format!("Cannot open remote chat: {error}. Update hosts older than protocol v27.")),
    }
    }).await.unwrap_or_else(|_| Err("Remote chat snapshot timed out".into()));
    let _ = tx.send((epoch, Update::Chat(revision, id.into(), generation, Box::new(update))));
}
