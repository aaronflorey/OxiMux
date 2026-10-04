use super::*;

fn entry(name: &str, endpoint: u8) -> HostEntry {
    HostEntry { name: name.into(), endpoint_id: format!("{endpoint:02x}").repeat(32),
        enrollment: None, read_only: false, protocol_version: None }
}

fn legacy(dir: &Path, name: &str, endpoint: u8) -> ClientSigner {
    let signer = client_identity::load_or_generate(dir, name).unwrap();
    HostsFile::update(dir, |hosts| { hosts.upsert(entry(name, endpoint)); Ok(()) }).unwrap();
    signer
}

#[test]
fn migration_preserves_exact_key_and_rename_preserves_enrollment() {
    let dir = tempfile::tempdir().unwrap();
    let original = legacy(dir.path(), "old", 1);
    let (mut saved, signer) = load_host(dir.path(), &entry("old", 1)).unwrap();
    assert_eq!(original.public_key(), signer.public_key());
    assert!(!client_identity::seed_path(dir.path(), "old").exists());
    saved.name = "new".into();
    HostsFile::update(dir.path(), |hosts| {
        hosts.remove("old"); hosts.upsert(saved.clone()); Ok(())
    }).unwrap();
    let (renamed, signer) = load_host(dir.path(), &saved).unwrap();
    assert_eq!(original.public_key(), signer.public_key());
    assert_eq!(renamed.enrollment, saved.enrollment);
    assert!(load_host(dir.path(), &entry("old", 1)).is_err());
}

#[test]
fn concurrent_cli_and_desktop_migrate_one_binding() {
    let dir = tempfile::tempdir().unwrap();
    let original = legacy(dir.path(), "server", 1).public_key();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let workers: Vec<_> = (0..8).map(|_| {
        let path = dir.path().to_owned(); let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait(); load_host(&path, &entry("server", 1)).unwrap()
        })
    }).collect();
    for worker in workers {
        let (saved, signer) = worker.join().unwrap();
        assert!(saved.enrollment.is_some());
        assert_eq!(signer.public_key(), original);
    }
}

#[test]
fn interrupted_migration_retries_without_rotating_identity() {
    let dir = tempfile::tempdir().unwrap();
    let signer = legacy(dir.path(), "server", 1);
    let seed = read_seed(dir.path(), "server").unwrap();
    let key = reference(&entry("server", 1).endpoint_id, &signer).unwrap();
    // Crash after writing the stable seed and before committing the book.
    client_identity::persist(&client_identity::seed_path(dir.path(), &key), &seed).unwrap();
    let (saved, reloaded) = load_host(dir.path(), &entry("server", 1)).unwrap();
    assert_eq!(saved.enrollment.as_deref(), Some(key.as_str()));
    assert_eq!(reloaded.public_key(), signer.public_key());
}

#[test]
fn missing_or_corrupt_enrolled_keys_fail_without_replacing_them() {
    let dir = tempfile::tempdir().unwrap();
    legacy(dir.path(), "server", 1);
    let (saved, _) = load_host(dir.path(), &entry("server", 1)).unwrap();
    let path = client_identity::seed_path(dir.path(), saved.enrollment.as_ref().unwrap());
    std::fs::write(&path, b"broken").unwrap();
    assert!(load_host(dir.path(), &saved).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"broken");
    std::fs::remove_file(&path).unwrap();
    assert!(load_host(dir.path(), &saved).is_err());
    assert!(!path.exists());
}

#[test]
fn aliases_with_same_key_share_binding_and_last_reference_owns_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    legacy(dir.path(), "first", 1);
    let seed = read_seed(dir.path(), "first").unwrap();
    client_identity::persist(&client_identity::seed_path(dir.path(), "second"), &seed).unwrap();
    HostsFile::update(dir.path(), |hosts| { hosts.upsert(entry("second", 1)); Ok(()) }).unwrap();
    let (removed, last) = remove_alias(dir.path(), "first", |_, _| panic!("shared enrollment must not unpair")).unwrap();
    assert!(!last);
    let (other, signer) = load_host(dir.path(), &entry("second", 1)).unwrap();
    assert_eq!(removed.enrollment, other.enrollment);
    assert_eq!(signer.public_key(), ClientSigner::from_seed(&seed).public_key());
    let (removed, last) = remove_alias(dir.path(), "second", |_, signer| assert_eq!(signer.public_key(), ClientSigner::from_seed(&seed).public_key())).unwrap();
    assert!(last);
    assert!(migrate(dir.path(), &mut removed.clone()).is_err());
}

#[test]
fn legacy_aliases_with_distinct_keys_remain_separate_enrollments() {
    let dir = tempfile::tempdir().unwrap();
    let first = legacy(dir.path(), "first", 1);
    let second = legacy(dir.path(), "second", 1);
    assert_ne!(first.public_key(), second.public_key());
    let (_removed, last) = remove_alias(dir.path(), "first", |_, signer| assert_eq!(signer.public_key(), first.public_key())).unwrap();
    assert!(last);
    assert_eq!(load_host(dir.path(), &entry("second", 1)).unwrap().1.public_key(), second.public_key());
}

#[test]
fn endpoint_hex_case_does_not_split_shared_legacy_enrollments() {
    let dir = tempfile::tempdir().unwrap();
    legacy(dir.path(), "first", 0xab);
    let seed = read_seed(dir.path(), "first").unwrap();
    client_identity::persist(&client_identity::seed_path(dir.path(), "second"), &seed).unwrap();
    let mut alias = entry("second", 0xab);
    alias.endpoint_id.make_ascii_uppercase();
    HostsFile::update(dir.path(), |hosts| { hosts.upsert(alias.clone()); Ok(()) }).unwrap();

    let (removed, last) = remove_alias(dir.path(), "first", |_, _| panic!("same endpoint and key must not unpair")).unwrap();
    assert!(!last);
    alias.endpoint_id.make_ascii_lowercase();
    let (saved, signer) = load_host(dir.path(), &alias).unwrap();
    assert_eq!(saved.enrollment, removed.enrollment);
    assert_eq!(signer.public_key(), ClientSigner::from_seed(&seed).public_key());
    assert_eq!(prepare_pairing(dir.path(), alias).unwrap().1.public_key(), signer.public_key());
}

#[test]
fn pairing_retry_and_repair_reuse_key_but_endpoint_replacement_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let (saved, first) = prepare_pairing(dir.path(), entry("server", 1)).unwrap();
    assert!(HostsFile::load(dir.path()).unwrap().entries.is_empty());
    assert_eq!(prepare_pairing(dir.path(), entry("server", 1)).unwrap().1.public_key(), first.public_key());
    HostsFile::update(dir.path(), |hosts| { hosts.upsert(saved.clone()); Ok(()) }).unwrap();
    assert_eq!(prepare_pairing(dir.path(), entry("server", 1)).unwrap().1.public_key(), first.public_key());
    let (replacement, next) = prepare_pairing(dir.path(), entry("server", 2)).unwrap();
    assert_ne!(first.public_key(), next.public_key());
    HostsFile::update(dir.path(), |hosts| { hosts.upsert(replacement); Ok(()) }).unwrap();
    assert!(load_host(dir.path(), &saved).is_err());
}

#[test]
fn changing_endpoint_on_bound_entry_cannot_reuse_its_identity() {
    let dir = tempfile::tempdir().unwrap();
    let (mut saved, _) = prepare_pairing(dir.path(), entry("server", 1)).unwrap();
    saved.endpoint_id = entry("server", 2).endpoint_id;
    HostsFile::update(dir.path(), |hosts| { hosts.upsert(saved.clone()); Ok(()) }).unwrap();
    assert!(load_host(dir.path(), &saved).is_err());
}

#[test]
fn stale_selection_cannot_adopt_replacement_enrollment_on_same_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let (saved, original) = prepare_pairing(dir.path(), entry("server", 1)).unwrap();
    HostsFile::update(dir.path(), |hosts| { hosts.upsert(saved.clone()); Ok(()) }).unwrap();
    finish_pairing(dir.path(), &saved).unwrap();
    std::fs::remove_file(client_identity::seed_path(dir.path(), saved.enrollment.as_ref().unwrap())).unwrap();

    let (replacement, signer) = prepare_pairing(dir.path(), entry("server", 1)).unwrap();
    assert_ne!(original.public_key(), signer.public_key());
    HostsFile::update(dir.path(), |hosts| { hosts.upsert(replacement.clone()); Ok(()) }).unwrap();
    assert!(load_host(dir.path(), &saved).is_err(), "old selection must not acquire new grants");
    assert_eq!(load_host(dir.path(), &replacement).unwrap().1.public_key(), signer.public_key());
}

#[test]
fn forgetting_a_missing_key_removes_alias_without_minting_or_unpairing() {
    let dir = tempfile::tempdir().unwrap();
    HostsFile::update(dir.path(), |hosts| { hosts.upsert(entry("server", 1)); Ok(()) }).unwrap();
    remove_alias(dir.path(), "server", |_, _| panic!("no key to authenticate")).unwrap();
    assert!(HostsFile::load(dir.path()).unwrap().entries.is_empty());
    assert!(!client_identity::seed_path(dir.path(), "server").exists());
}

#[test]
fn explicit_pair_can_recover_a_lost_key_without_changing_book_before_success() {
    let dir = tempfile::tempdir().unwrap();
    legacy(dir.path(), "server", 1);
    let (saved, _) = load_host(dir.path(), &entry("server", 1)).unwrap();
    std::fs::remove_file(client_identity::seed_path(dir.path(), saved.enrollment.as_ref().unwrap())).unwrap();
    assert!(load_host(dir.path(), &saved).is_err());
    let (replacement, signer) = prepare_pairing(dir.path(), entry("server", 1)).unwrap();
    assert_ne!(replacement.enrollment, saved.enrollment);
    assert_eq!(HostsFile::load(dir.path()).unwrap().get("server").unwrap().enrollment, saved.enrollment);
    HostsFile::update(dir.path(), |hosts| { hosts.upsert(replacement.clone()); Ok(()) }).unwrap();
    assert_eq!(load_host(dir.path(), &replacement).unwrap().1.public_key(), signer.public_key());
}
