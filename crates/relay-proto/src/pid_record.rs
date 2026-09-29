//! What the daemon writes to its pid file, and the supervisor reads back.
//!
//! One definition for both ends. Beyond the pid, the record carries what it
//! takes to trust that pid before signalling it (when the process started, so a
//! recycled pid is caught) and which app build the daemon came from (so a daemon
//! left over from an older version can be recognised).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidRecord {
    pub pid: u32,
    /// The daemon's crate version — the app version it shipped with.
    pub version: String,
    /// When the daemon's process started, as the kernel reports it.
    pub started_at_epoch_secs: u64,
    /// The daemon's executable path, for the log. Not used to decide anything.
    pub exe: String,
}
