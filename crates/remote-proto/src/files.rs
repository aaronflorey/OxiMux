//! Bounded filesystem replies. Paths never name the host's absolute root.
use serde::{Deserialize, Serialize};

pub const FILES_MIN_VERSION: u32 = 28;
pub const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
pub const DIRECTORY_PAGE_SIZE: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKindWire { Directory, File, Unsupported }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntryWire {
    pub name: String,
    pub kind: FileKindWire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryWire {
    pub path: String,
    pub entries: Vec<DirectoryEntryWire>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextFileWire {
    pub path: String,
    pub text: String,
    /// SHA-256 of the loaded UTF-8 bytes; saves require this content version.
    pub version: String,
}
