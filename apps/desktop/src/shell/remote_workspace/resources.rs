//! Resource loading and manual refresh share the same visible outcomes.
use super::*;

#[derive(Clone, Copy)]
pub(crate) enum Resource { Projects, Sessions }

pub(super) enum LoadState { Loading, Ready, Failed(String) }

impl RemoteWorkspace {
    pub(super) fn start_listings(&mut self, session: Arc<RemoteSession>, revision: u64) {
        let tx = self.tx.clone();
        let epoch = self.epoch;
        self.listings = Some(tokio::spawn(async move {
            use futures::StreamExt;
            let mut changes = session.take_sessions().expect("one listing task per connection");
            let access = publish_access(&session, &tx, epoch, revision).await;
            if !access.is_some_and(|(read_only, _)| read_only) {
                publish_projects(&session, &tx, epoch, revision).await;
            }
            match session.subscribe_sessions().await {
                Ok(sessions) => { let _ = tx.send((epoch, Update::Sessions(revision, sessions))); }
                Err(error) => { let _ = tx.send((epoch, Update::ResourceError(revision, Resource::Sessions, error.to_string()))); }
            }
            while let Some(sessions) = changes.next().await {
                let _ = tx.send((epoch, Update::Sessions(revision, sessions)));
            }
        }));
    }

    pub(super) fn refresh_resources(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else { return; };
        if self.refresh_task.as_ref().is_some_and(|task| !task.is_finished()) { return; }
        self.resource_states = std::array::from_fn(|_| LoadState::Loading);
        self.error = None;
        if let Some(driver) = &self.terminal_driver { let _ = driver.tx.send(terminal_driver::Command::Refresh); }
        let tx = self.tx.clone();
        let epoch = self.epoch;
        let revision = self.listing_revision;
        self.refresh_task = Some(tokio::spawn(async move {
            let access = publish_access(&session, &tx, epoch, revision).await;
            if !access.is_some_and(|(read_only, _)| read_only) {
                publish_projects(&session, &tx, epoch, revision).await;
            }
            // Subscribe again also repairs a previously failed initial subscription.
            match session.subscribe_sessions().await {
                Ok(sessions) => { let _ = tx.send((epoch, Update::Sessions(revision, sessions))); }
                Err(error) => { let _ = tx.send((epoch, Update::ResourceError(revision, Resource::Sessions, error.to_string()))); }
            }
        }));
        cx.notify();
    }
}

async fn publish_projects(session: &RemoteSession, tx: &mpsc::UnboundedSender<(u64, Update)>, epoch: u64, revision: u64) {
    let update = match session.list_projects().await {
        Ok(projects) => Update::Projects(revision, projects),
        Err(error) => Update::ResourceError(revision, Resource::Projects, error.to_string()),
    };
    let _ = tx.send((epoch, update));
}

async fn publish_access(session: &RemoteSession, tx: &mpsc::UnboundedSender<(u64, Update)>, epoch: u64, revision: u64) -> Option<(bool, bool)> {
    let access = if session.host_protocol_version().is_some_and(|version| version >= 27) {
        session.client_access().await.map_err(|error| error.to_string())
    } else {
        Err("Update the host to protocol v27 or newer to verify desktop access.".into())
    };
    let update = match &access {
        Ok((read_only, can_create)) => Update::Access(revision, *read_only, *can_create),
        Err(error) => Update::ListingError(revision, format!("Cannot verify host access: {error}")),
    };
    let _ = tx.send((epoch, update));
    access.ok()
}
