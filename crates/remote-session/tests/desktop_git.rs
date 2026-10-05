//! Exercise the desktop Git executor without a platform GUI, using the actual
//! authenticated dispatcher and a server-owned temporary repository.
#![allow(dead_code)]

/// The executor addresses ops by `super::Root` — in the app that is
/// `remote_workspace::Root`; in this harness the crate root plays that role,
/// so the same-shaped enum is declared here.
#[derive(Clone)]
enum Root {
    Session(String),
    Project(String),
}

#[path = "../../../apps/desktop/src/shell/remote_workspace/git_rpc.rs"]
mod git_rpc;
