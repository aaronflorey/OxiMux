//! Run the desktop's transport actor without a platform GUI/SDK. The included
//! production actor tests use the real dispatcher, authentication, and emulator.
#![allow(dead_code)]

enum Update {
    ListingError(u64, String),
    Terminals(u64, Result<Vec<oximux_remote_proto::messages::TerminalSummary>, String>),
}

#[path = "../../../apps/desktop/src/shell/remote_workspace/terminal_driver.rs"]
mod terminal_driver;
