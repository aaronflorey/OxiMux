//! Where a pane group's content executes.
//!
//! `PaneGroup::cwd` always carries the project path — for a remote scope it
//! is the *host* path, kept as a `PathBuf` for labels and ids but never
//! exec'd or stat'd locally. Every process-spawning site branches on
//! `remote.is_some()` and routes through the host's RPC instead, so the same
//! tab strip runs local shells beside remote ones.

use gpui::{Entity, WeakEntity};

use crate::shell::remote_host::RemoteHost;

/// A pane group bound to one paired host's project path.
#[derive(Clone)]
pub struct RemoteScope {
    /// The host entity that owns the project path. Weak: dropping the host
    /// disconnects the session and turns every exec site into a no-op.
    pub(crate) host: WeakEntity<RemoteHost>,
    /// Lowercase-hex endpoint id — namespaces `SurfaceIds` as
    /// `remote:{endpoint_tag}` the way `RemoteWorkspace` did, and namespaces
    /// this scope's workspace key away from any local path.
    pub(crate) endpoint_tag: String,
}

impl RemoteScope {
    pub(crate) fn new(host: &Entity<RemoteHost>, endpoint_tag: String) -> Self {
        Self {
            host: host.downgrade(),
            endpoint_tag,
        }
    }

    /// Point this scope at a REPLACEMENT host entity (forget-and-re-pair,
    /// or an enrollment switch on the same endpoint). The endpoint tag
    /// carries over — it namespaces surfaces, not enrollments.
    pub(crate) fn rebind(&mut self, host: &Entity<RemoteHost>) {
        self.host = host.downgrade();
    }

    /// Whether the scope's weak handle resolves to THIS entity — the check
    /// every cached-surface rebind gate uses so the common "same host,
    /// still alive" path stays a no-op.
    pub(crate) fn host_is(&self, host: &Entity<RemoteHost>) -> bool {
        self.host
            .upgrade()
            .is_some_and(|h| h.entity_id() == host.entity_id())
    }
}

impl RemoteScope {
    /// `SurfaceIds` workspace tag — shared across every tab in every pane of
    /// this remote project, matching the one-session-per-endpoint scheme the
    /// old workspace used.
    pub(crate) fn surface_tag(&self) -> String {
        format!("remote:{}", self.endpoint_tag)
    }

    /// The host's endpoint id — namespaces `ProjectKey`s and workspace ids.
    /// Read by the remote tab-routing slice; the field it wraps is already
    /// live for `RemoteExecTarget`.
    #[allow(dead_code)]
    pub(crate) fn endpoint_id(&self) -> &str {
        &self.endpoint_tag
    }
}
