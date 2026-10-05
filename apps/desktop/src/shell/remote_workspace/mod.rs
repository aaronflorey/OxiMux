//! Remote view library. Server paths never enter the local project registry.
//!
//! The takeover `RemoteWorkspace` shell is gone — remote projects mount in
//! the normal `ProjectPanes` / right-sidebar slots with a `RemoteScope`.
//! This module is the shared library those mounts use: the `Update` event
//! stream and drivers consumed by [`crate::shell::remote_host::RemoteHost`],
//! and the host-backed `RemoteFilesView` / `RemoteGitView` panels the
//! sidebar mounts for remote projects.
use std::{sync::Arc, collections::HashMap};

// The views in this module glob `super::*`; these imports are their
// surface as much as this file's.
#[allow(unused_imports)]
use gpui::{App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, Subscription, Task, Window, div, px};
#[allow(unused_imports)]
use gpui::prelude::FluentBuilder;
#[allow(unused_imports)]
use gpui_component::{Disableable, button::{Button, ButtonVariants}, input::{Input, InputState}};
use oximux_remote_proto::SessionSummary;
use oximux_remote_session::{ConnState, RemoteSession};
use oximux_remote_session::hosts_store::{HostEntry, HostsFile};
#[allow(unused_imports)]
use oximux_settings::{Density, Theme, Typography};

pub(crate) mod connection;
pub(crate) mod resources;
pub(crate) mod chat_driver;
pub(crate) mod terminal_driver;
pub(crate) mod git_rpc;
pub(crate) mod git_view;
pub(crate) mod files_rpc;
pub(crate) mod files_view;
pub(crate) mod draft_store;
pub(crate) mod restore;
pub(crate) use files_view::RemoteFilesView;

/// Where a remote file/git operation is rooted: a session id (the
/// agent-bound surface available since v1) or a project path (the v29 browse
/// surface — same verbs, no session, no agent spawn on the host).
#[derive(Clone)]
pub(crate) enum Root {
    /// Session-rooted surface — mounted by `set_active_remote_session` when
    /// a pairing has no project listing to anchor on (read-only viewers,
    /// session-scoped tickets). Matched against in `sync_git_titles`.
    Session(String),
    Project(String),
}

impl Root {
    /// Stable draft-parking key: `(endpoint, key)` identifies one host
    /// file surface — a session's files or a browsed project's — so parked
    /// buffers re-enter the right view after teardown.
    pub(crate) fn key(&self) -> String {
        match self {
            Self::Session(id) => format!("session:{id}"),
            Self::Project(path) => format!("project:{path}"),
        }
    }
}

/// Host-file editors parked while their owning surface is torn down —
/// keyed by (host endpoint, [`Root::key`]) so drafts from one host or one
/// surface can never appear on another's identically-named session or
/// project. Lives on `WorkspaceRoot` so drafts survive sidebar swaps.
pub(crate) type DraftFiles = HashMap<(String, String), Entity<RemoteFilesView>>;

/// A folded chat state or its open/recovery failure. Boxed at the variant site:
/// `ChatThread` is large enough that an inline `Result` would blow up `Update`.
pub(crate) type ChatSnapshot = Result<(u64, oximux_agents::thread::ChatThread, bool, Option<oximux_remote_proto::proto::SessionChoices>), String>;

pub(crate) enum Update {
    Hosts(Result<HostsFile, String>),
    Enrollment(HostEntry),
    State(ConnState),
    Connected(Arc<RemoteSession>),
    Access(u64, bool, bool),
    Projects(u64, Vec<oximux_remote_proto::ProjectSummaryWire>),
    Sessions(u64, Vec<SessionSummary>),
    Terminals(u64, Result<Vec<oximux_remote_proto::messages::TerminalSummary>, String>),
    Created(u64, Result<String, String>),
    ListingError(u64, String),
    ResourceError(u64, resources::Resource, String),
    Chat(u64, String, u64, Box<ChatSnapshot>),
}
