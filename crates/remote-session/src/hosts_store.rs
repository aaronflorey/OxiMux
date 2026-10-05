//! Named remote hosts shared by desktop and CLI in `hosts.toml`.
//!
//! **Client-side state, deliberately not in any host's data directory.** A
//! laptop pairs with several hosts, so the list of them cannot live under one
//! of their data dirs; and the CLI must work on a machine with no OxiMux host
//! installed at all. `config_local_dir` rather than `config_dir` for the reason
//! the desktop's `app_paths` picks the local variants everywhere: on Windows
//! the roaming profile follows the user between machines, and an enrollment is
//! bound to *this* machine's key.
//!
//! **No secrets here.** An endpoint id is a public key and a name is a label;
//! the client's signing seed lives in its own owner-only file
//! ([`crate::client_identity`]). That split is what lets this file be printed,
//! diffed, and hand-edited without a redaction discipline.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::store_error::StoreError;

/// The file, under [`config_dir`].
const HOSTS_FILE: &str = "hosts.toml";

/// One paired host as the CLI remembers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEntry {
    /// What the user types after `--host`.
    pub name: String,
    /// The host's iroh endpoint id, 64 lowercase hex characters. Public — it is
    /// how the host is *found*, not how it is authenticated.
    pub endpoint_id: String,
    /// Stable endpoint/public-key binding. Missing only for legacy entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrollment: Option<String>,
    /// Whether this enrollment was minted read-only. A hint for `hosts ls`, not
    /// a gate: the host enforces the tier, and a stale `false` here changes
    /// nothing about what the host will serve.
    #[serde(default)]
    pub read_only: bool,
    /// The protocol version this host reported last time it answered. Cached so
    /// the compat gate can warn before dialling; `None` until first contact.
    #[serde(default)]
    pub protocol_version: Option<u32>,
}

/// The whole file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostsFile {
    /// The host used when no `--host` and no `OXIMUX_HOST` say otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, rename = "hosts")]
    pub entries: Vec<HostEntry>,
}

/// Override the config directory. Set by the test suite so a test run never
/// touches the developer's real hosts file — and usable by anyone who wants
/// their fleet somewhere else.
pub const CONFIG_DIR_ENV_VAR: &str = "OXIMUX_CONFIG_DIR";

/// The CLI's config directory, creating nothing.
pub fn config_dir() -> Result<PathBuf, StoreError> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV_VAR).filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    dirs::config_local_dir().map(|d| d.join("oximux")).ok_or_else(|| {
        StoreError::new("no-config-dir", "this platform reports no config directory")
    })
}

fn read_error(what: &str, path: &Path, e: impl std::fmt::Display) -> StoreError {
    StoreError::new("hosts", format!("could not {what} {}: {e}", path.display()))
}

impl HostsFile {
    /// Serialize read/modify/write across desktop and CLI without losing rows
    /// or a concurrent CLI default-host change. Call off the UI thread.
    pub fn update(
        dir: &Path,
        edit: impl FnOnce(&mut Self) -> Result<(), StoreError>,
    ) -> Result<Self, StoreError> {
        Self::locked(dir, |mut hosts| {
            edit(&mut hosts)?;
            hosts.save(dir)?;
            Ok(hosts)
        })
    }

    /// Hold the book lock through post-commit key retirement as well as edits.
    pub(crate) fn locked(
        dir: &Path,
        transaction: impl FnOnce(Self) -> Result<Self, StoreError>,
    ) -> Result<Self, StoreError> {
        oximux_owner_only::prepare_owner_only_dir(dir)
            .map_err(|e| read_error("secure", dir, e))?;
        let path = dir.join("hosts.lock");
        let file = std::fs::OpenOptions::new().read(true).write(true)
            .create(true).truncate(false).open(&path)
            .map_err(|e| read_error("open lock", &path, e))?;
        let mut lock = fd_lock::RwLock::new(file);
        let _guard = lock.write().map_err(|e| read_error("lock", &path, e))?;
        transaction(Self::load(dir)?)
    }

    /// Load the file, or an empty set when it does not exist.
    ///
    /// A missing file is **not** an error: the zero-config local path must work
    /// on a machine that has never paired with anything.
    pub fn load(dir: &Path) -> Result<Self, StoreError> {
        let path = dir.join(HOSTS_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(read_error("read", &path, e)),
        };
        // A malformed file is reported, never silently reset: it holds the
        // user's whole fleet, and quietly starting over would look like the
        // hosts had been forgotten.
        toml::from_str(&text).map_err(|e| {
            read_error("parse", &path, e)
                .with_steps(["fix the file by hand, or delete it to start over".into()])
        })
    }

    pub fn save(&self, dir: &Path) -> Result<(), StoreError> {
        std::fs::create_dir_all(dir).map_err(|e| read_error("create", dir, e))?;
        // Owner-only, for the same reason `oximux serve` hardens its data dir:
        // a shared server is exactly where other accounts exist. `create_dir_all`
        // above applies the umask, which on a typical box leaves this 0755.
        //
        // The signing keys beside this file are already 0600 and readback-
        // verified, so what an open directory exposes is not the credential but
        // the fleet: which hosts this account is paired with, under what names,
        // at which endpoint ids. Worth closing, and cheap.
        //
        // Best-effort: a config directory that cannot be restricted must not
        // stop the CLI from working, unlike a key file, where the same failure
        // is fatal by design.
        if let Err(err) = oximux_owner_only::prepare_owner_only_dir(dir) {
            tracing::debug!(%err, dir = %dir.display(), "could not restrict the config directory");
        }
        let path = dir.join(HOSTS_FILE);
        let text = toml::to_string_pretty(self)
            .map_err(|e| read_error("encode", &path, e))?;
        // Readers and a crash see either the previous host book or the full new
        // one, never a truncated file. The temporary file lives on the same disk.
        use std::io::Write;
        let mut temporary = tempfile::NamedTempFile::new_in(dir)
            .map_err(|e| read_error("create temporary file for", &path, e))?;
        oximux_owner_only::restrict_file(temporary.path())
            .map_err(|e| read_error("restrict", &path, e))?;
        temporary.write_all(text.as_bytes()).map_err(|e| read_error("write", &path, e))?;
        temporary.as_file().sync_all().map_err(|e| read_error("sync", &path, e))?;
        temporary.persist(&path).map_err(|e| read_error("replace", &path, e.error))?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&HostEntry> {
        self.entries.iter().find(|h| h.name == name)
    }

    /// Add or replace a host by name. Re-pairing an existing name overwrites it
    /// rather than making a second row with the same label.
    pub fn upsert(&mut self, entry: HostEntry) {
        match self.entries.iter_mut().find(|h| h.name == entry.name) {
            Some(existing) => *existing = entry,
            None => self.entries.push(entry),
        }
    }

    /// Drop a host. Returns whether it was there. Clears the default when it
    /// pointed at the removed host, so a later bare invocation falls back to
    /// the local socket rather than naming something that no longer exists.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|h| h.name != name);
        if self.default.as_deref() == Some(name) {
            self.default = None;
        }
        self.entries.len() != before
    }

    /// Which host a verb should talk to, given the flag and the environment.
    ///
    /// Order: `--host` → `OXIMUX_HOST` → the recorded default → none (the local
    /// socket). A **named** host that is not in the file is an error rather
    /// than a silent fallback to local: a typo'd `--host prod` must not quietly
    /// drive the machine you are sitting at.
    pub fn resolve(&self, flag: Option<&str>, env: Option<&str>) -> Result<Option<&HostEntry>, StoreError> {
        for (named, source) in [(flag, "--host"), (env, HOST_ENV_VAR)] {
            let Some(name) = named.filter(|n| !n.is_empty()) else { continue };
            return self.get(name).map(Some).ok_or_else(|| {
                StoreError::new("unknown-host", format!("no host named `{name}` ({source})"))
                    .with_steps([
                        "list what is paired with `oximux hosts ls`".into(),
                        "pair a new one with `oximux pair <ticket>`".into(),
                    ])
            });
        }
        // A default naming a host that was removed behind our back reads as
        // "no default" rather than an error: nothing the user typed is wrong.
        Ok(self.default.as_deref().and_then(|name| self.get(name)))
    }
}

/// The environment variable naming a host, for callers that do not pass
/// `--host`.
pub const HOST_ENV_VAR: &str = "OXIMUX_HOST";

/// 32 bytes from 64 hex characters.
pub fn parse_endpoint_id(hex: &str) -> Result<[u8; 32], StoreError> {
    let bad = || {
        StoreError::new(
            "endpoint-id",
            format!("`{hex}` is not a 64-character hex endpoint id"),
        )
    };
    if hex.len() != 64 {
        return Err(bad());
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2).ok_or_else(bad)?, 16)
            .map_err(|_| bad())?;
    }
    Ok(out)
}

pub fn endpoint_id_hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> HostEntry {
        HostEntry {
            name: name.into(),
            endpoint_id: "aa".repeat(32),
            enrollment: None,
            read_only: false,
            protocol_version: None,
        }
    }

    #[test]
    fn a_missing_file_is_an_empty_set_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = HostsFile::load(dir.path()).expect("a fresh machine has no hosts");
        assert!(hosts.entries.is_empty());
        assert_eq!(hosts.default, None);
    }

    #[test]
    fn hosts_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosts = HostsFile::default();
        hosts.upsert(entry("server"));
        hosts.default = Some("server".into());
        hosts.save(dir.path()).expect("save");

        let read = HostsFile::load(dir.path()).expect("load");
        assert_eq!(read.default.as_deref(), Some("server"));
        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.entries[0].endpoint_id, "aa".repeat(32));
    }

    #[test]
    fn concurrent_enrollments_preserve_all_hosts_and_the_cli_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut initial = HostsFile::default();
        initial.upsert(entry("default"));
        initial.default = Some("default".into());
        initial.save(dir.path()).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8).map(|i| {
            let path = dir.path().to_owned();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                HostsFile::update(&path, |hosts| {
                    hosts.upsert(entry(&format!("host-{i}")));
                    Ok(())
                }).unwrap();
            })
        }).collect();
        for worker in workers { worker.join().unwrap(); }
        let saved = HostsFile::load(dir.path()).unwrap();
        assert_eq!(saved.entries.len(), 9);
        assert_eq!(saved.default.as_deref(), Some("default"));
    }

    /// Re-pairing a name replaces its row rather than making a duplicate that
    /// `--host` could not disambiguate.
    #[test]
    fn upsert_replaces_rather_than_duplicating() {
        let mut hosts = HostsFile::default();
        hosts.upsert(entry("server"));
        let mut updated = entry("server");
        updated.endpoint_id = "bb".repeat(32);
        hosts.upsert(updated);
        assert_eq!(hosts.entries.len(), 1);
        assert_eq!(hosts.entries[0].endpoint_id, "bb".repeat(32));
    }

    #[test]
    fn removing_the_default_host_clears_the_default() {
        let mut hosts = HostsFile::default();
        hosts.upsert(entry("server"));
        hosts.default = Some("server".into());
        assert!(hosts.remove("server"));
        assert_eq!(hosts.default, None, "a dangling default would name nothing");
        assert!(!hosts.remove("server"), "removing it again reports absent");
    }

    /// The resolution order, and the one case that must NOT fall back.
    #[test]
    fn resolution_prefers_flag_then_env_then_default_then_local() {
        let mut hosts = HostsFile::default();
        hosts.upsert(entry("a"));
        hosts.upsert(entry("b"));
        hosts.default = Some("b".into());

        assert_eq!(hosts.resolve(Some("a"), Some("b")).unwrap().unwrap().name, "a", "flag wins");
        assert_eq!(hosts.resolve(None, Some("a")).unwrap().unwrap().name, "a", "env beats default");
        assert_eq!(hosts.resolve(None, None).unwrap().unwrap().name, "b", "then the default");

        let bare = HostsFile::default();
        assert!(bare.resolve(None, None).unwrap().is_none(), "no hosts → the local socket");
    }

    /// A typo'd `--host` must be an error, never a silent fallback to driving
    /// the machine the user is sitting at.
    #[test]
    fn a_named_host_that_does_not_exist_is_a_usage_error() {
        let hosts = HostsFile::default();
        let err = hosts.resolve(Some("prod"), None).expect_err("no such host");
        assert_eq!(err.code, "unknown-host");
        assert!(err.message.contains("prod"));
    }

    /// An empty value reads as unset, so `OXIMUX_HOST=` behaves like not
    /// setting it rather than erroring on a host named "".
    #[test]
    fn an_empty_name_is_treated_as_unset() {
        let mut hosts = HostsFile::default();
        hosts.upsert(entry("b"));
        hosts.default = Some("b".into());
        assert_eq!(hosts.resolve(Some(""), None).unwrap().unwrap().name, "b");
    }

    /// A default pointing at a host that is gone reads as "no default" — the
    /// user typed nothing wrong, so nothing should fail.
    #[test]
    fn a_dangling_default_falls_back_to_local() {
        let hosts = HostsFile { default: Some("ghost".into()), ..Default::default() };
        assert!(hosts.resolve(None, None).unwrap().is_none());
    }

    #[test]
    fn endpoint_ids_round_trip_through_hex() {
        let bytes = [0x0au8; 32];
        let hex = endpoint_id_hex(&bytes);
        assert_eq!(hex.len(), 64);
        assert_eq!(parse_endpoint_id(&hex).unwrap(), bytes);
    }

    #[test]
    fn a_malformed_endpoint_id_is_refused() {
        assert!(parse_endpoint_id("abc").is_err(), "too short");
        assert!(parse_endpoint_id(&"zz".repeat(32)).is_err(), "not hex");
    }
}
