//! The desktop and CLI app-signing identity, **one key per host**.
//!
//! Mirrors `oximux_remote_host::identity`'s file+0600+readback pattern rather
//! than the phone's storage: `mobile-core` keeps its seed in the OS keystore
//! and rebuilds via `ClientSigner::from_seed`, which has no portable
//! equivalent here.
//!
//! **The weakening is real and is not papered over.** A raw Ed25519 seed in a
//! file is a weaker threat model than a hardware-backed keystore, and a CLI is
//! more likely than a phone to be sitting on a shared server. Three things
//! bound it: the file is owner-only and *verified so by readback* (a write that
//! cannot be restricted fails rather than proceeding), keys are per host so a
//! compromised enrollment does not transfer to the rest of the fleet, and
//! `oximux hosts rm` unpairs host-side so revocation does not depend on the
//! file. If OS-keyring integration is ever wanted for laptop installs, this is
//! the module it replaces.

use std::path::{Path, PathBuf};

use crate::ClientSigner;
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

use super::store_error::StoreError;

/// Load this machine's signing identity for `host_name`, generating and
/// persisting one if it has none yet.
pub fn load_or_generate(dir: &Path, host_name: &str) -> Result<ClientSigner, StoreError> {
    let path = seed_path(dir, host_name);
    let io = |e| StoreError::new("identity", format!("could not load the client key {}: {e}", path.display()));
    // Desktop and CLI may enroll concurrently. Serialize generation so both
    // authenticate with the seed that survives on disk.
    oximux_owner_only::prepare_owner_only_dir(dir).map_err(io)?;
    let lock_file = std::fs::OpenOptions::new().read(true).write(true)
        .create(true).truncate(false).open(path.with_extension("lock")).map_err(io)?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock.write().map_err(io)?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            // Existing keys must retain the same protection as newly written keys.
            oximux_owner_only::restrict_file(&path).map_err(io)?;
            if !oximux_owner_only::is_restricted_to_owner(&path).map_err(io)? {
                return Err(StoreError::new("identity", "the client key is not owner-only"));
            }
            if let Ok(seed) = <[u8; 32]>::try_from(bytes.as_slice()) {
                return Ok(ClientSigner::from_seed(&seed));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    // A short or corrupt file is regenerated rather than failing closed. The
    // consequence is honest and recoverable: the new public key is not the one
    // the host paired, so the next call is refused and the user re-pairs —
    // which beats a CLI that cannot start at all.
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    persist(&path, &seed)?;
    Ok(ClientSigner::from_seed(&seed))
}

/// Forget a host's key, so `hosts rm` leaves nothing behind that could
/// authenticate. Absent is success.
pub fn forget(dir: &Path, host_name: &str) {
    let _ = std::fs::remove_file(seed_path(dir, host_name));
}

/// `client-<sha256(host_name)[..16]>.key`. Hashed rather than using the name
/// directly so a host called `../../etc/passwd` cannot pick the path, and
/// SHA-256 rather than `DefaultHasher` because that one's output is not stable
/// across Rust versions — a rotating filename would silently mint a new
/// identity and un-pair the user on a toolchain bump.
pub(super) fn seed_path(dir: &Path, host_name: &str) -> PathBuf {
    let digest = Sha256::digest(host_name.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    dir.join(format!("client-{hex}.key"))
}

/// Write the seed owner-only, and **prove** it. The restriction is asserted by
/// readback rather than assumed from the create flags: on Windows there is no
/// create-time equivalent of `mode`, and on unix a pre-existing file keeps its
/// old permissions. A signing key other accounts can read is worse than no
/// remote access at all, so a failure here propagates.
pub(super) fn persist(path: &Path, seed: &[u8; 32]) -> Result<(), StoreError> {
    let io = |what: &str, e: std::io::Error| {
        StoreError::new(
            "identity",
            format!("could not {what} the client key {}: {e}", path.display()),
        )
    };
    if let Some(parent) = path.parent() {
        oximux_owner_only::prepare_owner_only_dir(parent)
            .map_err(|e| io("secure the directory for", e))?;
    }
    // Atomic replacement also makes interrupted legacy migration retryable.
    use std::io::Write;
    let parent = path.parent().ok_or_else(|| StoreError::new("identity", "key has no directory"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|e| io("create", e))?;
    oximux_owner_only::restrict_file(temporary.path()).map_err(|e| io("restrict", e))?;
    temporary.write_all(seed).map_err(|e| io("write", e))?;
    temporary.as_file().sync_all().map_err(|e| io("sync", e))?;
    temporary.persist(path).map_err(|e| io("replace", e.error))?;
    oximux_owner_only::restrict_file(path).map_err(|e| io("restrict", e))?;
    // The receipt. `restrict_file` succeeding is not the same as the file being
    // restricted — this is the only check that actually looks.
    match oximux_owner_only::is_restricted_to_owner(path) {
        Ok(true) => Ok(()),
        Ok(false) => Err(StoreError::new(
            "identity",
            format!("{} is readable by other accounts", path.display()),
        )
        .with_steps(["a signing key must be owner-only; fix the directory's permissions".into()])),
        Err(e) => Err(io("verify the permissions of", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_persists_and_reloads_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_generate(dir.path(), "server").expect("generate");
        let again = load_or_generate(dir.path(), "server").expect("reload");
        assert_eq!(first.public_key(), again.public_key(), "same identity across runs");
    }

    #[test]
    fn concurrent_clients_share_one_identity() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8).map(|_| {
            let path = dir.path().to_owned();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                load_or_generate(&path, "server").unwrap().public_key()
            })
        }).collect();
        let keys: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
        assert!(keys.iter().all(|key| key == &keys[0]));
        assert_eq!(load_or_generate(dir.path(), "server").unwrap().public_key(), keys[0]);
    }

    /// Per host, so a compromised enrollment does not transfer across the fleet.
    #[test]
    fn each_host_gets_its_own_key() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_generate(dir.path(), "a").expect("a");
        let b = load_or_generate(dir.path(), "b").expect("b");
        assert_ne!(a.public_key(), b.public_key());
    }

    #[test]
    fn the_seed_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        load_or_generate(dir.path(), "server").expect("generate");
        let path = seed_path(dir.path(), "server");
        assert!(
            oximux_owner_only::is_restricted_to_owner(&path).unwrap(),
            "a signing seed must not be readable by other accounts"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reloading_restores_owner_only_permissions_without_rotating_the_key() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_generate(dir.path(), "server").unwrap();
        let path = seed_path(dir.path(), "server");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let again = load_or_generate(dir.path(), "server").unwrap();
        assert_eq!(first.public_key(), again.public_key());
        assert!(oximux_owner_only::is_restricted_to_owner(&path).unwrap());
    }

    /// A truncated file regenerates rather than failing the whole CLI. The user
    /// re-pairs; they are not locked out.
    #[test]
    fn a_corrupt_seed_regenerates_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_path(dir.path(), "server");
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(&path, b"short").unwrap();
        let signer = load_or_generate(dir.path(), "server").expect("regenerate");
        assert_eq!(signer.public_key().len(), 32);
    }

    /// A host name that looks like a path must not select the path.
    #[test]
    fn a_traversing_host_name_cannot_escape_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_path(dir.path(), "../../etc/passwd");
        assert_eq!(path.parent(), Some(dir.path()), "the name is hashed, not joined");
    }

    #[test]
    fn forgetting_a_key_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        load_or_generate(dir.path(), "server").expect("generate");
        forget(dir.path(), "server");
        forget(dir.path(), "server");
        assert!(!seed_path(dir.path(), "server").exists());
    }
}
