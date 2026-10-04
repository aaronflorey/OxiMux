//! Stable signing-key references shared by the desktop and CLI.
//! Host-book transactions precede key locks. Legacy keys are retained for
//! recovery; a missing enrolled key fails closed instead of rotating pairing.
use std::path::Path;
use sha2::{Digest, Sha256};
use crate::{ClientSigner, StoreError, client_identity};
use crate::hosts_store::{HostEntry, HostsFile, parse_endpoint_id};

fn reference(endpoint: &str, signer: &ClientSigner) -> Result<String, StoreError> {
    let mut digest = Sha256::new();
    digest.update(parse_endpoint_id(endpoint)?);
    digest.update(signer.public_key());
    Ok(format!("enrollment-{:x}", digest.finalize()))
}

fn read_seed(dir: &Path, key: &str) -> Result<[u8; 32], StoreError> {
    let path = client_identity::seed_path(dir, key);
    let error = |e| StoreError::new("identity", format!("could not read enrolled client key: {e}"));
    oximux_owner_only::restrict_file(&path).map_err(error)?;
    if !oximux_owner_only::is_restricted_to_owner(&path).map_err(error)? {
        return Err(StoreError::new("identity", "the enrolled client key is not owner-only"));
    }
    let bytes = std::fs::read(&path).map_err(error)?;
    bytes.as_slice().try_into().map_err(|_| StoreError::new("identity", "the enrolled client key is corrupt; pair again"))
}

fn migrate(dir: &Path, entry: &mut HostEntry) -> Result<ClientSigner, StoreError> {
    if let Some(key) = &entry.enrollment {
        let signer = ClientSigner::from_seed(&read_seed(dir, key)?);
        if reference(&entry.endpoint_id, &signer)? != *key {
            return Err(StoreError::new("identity", "enrollment does not belong to this endpoint; pair again"));
        }
        return Ok(signer);
    }
    // Do not mint a new key for a saved enrollment with missing/corrupt seed.
    let seed = read_seed(dir, &entry.name)?;
    let signer = ClientSigner::from_seed(&seed);
    let key = reference(&entry.endpoint_id, &signer)?;
    client_identity::persist(&client_identity::seed_path(dir, &key), &seed)?;
    entry.enrollment = Some(key);
    Ok(signer)
}

/// Migrate the saved name/endpoint association while holding the shared book
/// lock. A stale selection cannot authenticate after that alias is replaced.
pub fn load_host(dir: &Path, selected: &HostEntry) -> Result<(HostEntry, ClientSigner), StoreError> {
    let mut result = None;
    HostsFile::locked(dir, |mut hosts| {
        let entry = hosts.entries.iter_mut().find(|entry| entry.name == selected.name
            && entry.endpoint_id.eq_ignore_ascii_case(&selected.endpoint_id)
            && selected.enrollment.as_ref().is_none_or(|key| entry.enrollment.as_ref() == Some(key)))
            .ok_or_else(|| StoreError::new("unknown-host", "host selection changed; select it again"))?;
        let signer = migrate(dir, entry)?;
        result = Some((entry.clone(), signer));
        hosts.save(dir)?;
        retire_recovery_keys(dir, &selected.name, &selected.endpoint_id);
        Ok(hosts)
    })?;
    Ok(result.expect("transaction assigned enrollment"))
}

/// Prepare a key before spending a ticket. Re-pairing the same association
/// keeps its key; replacing an alias with another endpoint never uses the old
/// alias seed. An unsuccessful pair creates no host-book entry.
pub fn prepare_pairing(dir: &Path, mut candidate: HostEntry) -> Result<(HostEntry, ClientSigner), StoreError> {
    parse_endpoint_id(&candidate.endpoint_id)?;
    let mut result = None;
    HostsFile::update(dir, |hosts| {
        if let Some(entry) = hosts.entries.iter_mut().find(|entry| entry.name == candidate.name
            && entry.endpoint_id.eq_ignore_ascii_case(&candidate.endpoint_id))
            && let Ok(signer) = migrate(dir, entry) {
            result = Some((entry.clone(), signer));
            return Ok(());
        }
        // Only an explicit ticket-based pair may recover a damaged identity.
        let pending = format!("pair:{}:{}", candidate.endpoint_id, candidate.name);
        let signer = client_identity::load_or_generate(dir, &pending)?;
        let seed = read_seed(dir, &pending)?;
        let key = reference(&candidate.endpoint_id, &signer)?;
        client_identity::persist(&client_identity::seed_path(dir, &key), &seed)?;
        candidate.enrollment = Some(key);
        result = Some((candidate, signer));
        Ok(())
    })?;
    Ok(result.expect("transaction assigned pairing key"))
}

fn retire_recovery_keys(dir: &Path, name: &str, endpoint: &str) {
    client_identity::forget(dir, name);
    client_identity::forget(dir, &format!("pair:{endpoint}:{name}"));
}

/// Called only after successfully persisting a paired host. Retain ticket
/// retry seeds until that commit, then erase them under the shared book lock.
pub fn finish_pairing(dir: &Path, selected: &HostEntry) -> Result<(), StoreError> {
    HostsFile::locked(dir, |hosts| {
        if hosts.get(&selected.name) == Some(selected) && selected.enrollment.is_some() {
            retire_recovery_keys(dir, &selected.name, &selected.endpoint_id);
        }
        Ok(hosts)
    })?;
    Ok(())
}

/// Remove an alias transactionally. The last reference alone owns revocation.
/// The bounded unpair callback runs under the host-book lock, preventing a
/// concurrent pairing from acquiring an enrollment being revoked. Run off the
/// UI thread; the callback must not recursively access the host book.
pub fn remove_alias(
    dir: &Path,
    name: &str,
    unpair: impl FnOnce(&HostEntry, ClientSigner),
) -> Result<(HostEntry, bool), StoreError> {
    let mut result = None;
    HostsFile::locked(dir, |mut hosts| {
        let entry = hosts.entries.iter_mut().find(|entry| entry.name == name)
            .ok_or_else(|| StoreError::new("unknown-host", format!("no host named `{name}`")))?;
        // Forget remains available when a key was lost or damaged. Such an
        // entry cannot authenticate; never mint a replacement just to unpair.
        let signer = migrate(dir, entry).ok();
        let entry = entry.clone();
        // Legacy aliases must first acquire their binding before counting
        // references, otherwise a shared legacy seed could be revoked here.
        for other in &mut hosts.entries {
            if other.endpoint_id.eq_ignore_ascii_case(&entry.endpoint_id) { let _ = migrate(dir, other); }
        }
        let last = entry.enrollment.is_none() || !hosts.entries.iter().any(|other| other.name != name
            && other.enrollment == entry.enrollment);
        if last && let Some(signer) = signer { unpair(&entry, signer); }
        hosts.remove(name);
        hosts.save(dir)?;
        // Persist the removal before retiring recovery/pending seeds, while
        // still excluding concurrent migrations and ticket preparation.
        retire_recovery_keys(dir, &entry.name, &entry.endpoint_id);
        if last && let Some(key) = &entry.enrollment { client_identity::forget(dir, key); }
        result = Some((entry, last));
        Ok(hosts)
    })?;
    Ok(result.expect("transaction removed alias"))
}

#[cfg(test)]
mod tests;
