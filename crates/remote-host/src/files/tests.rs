use super::*;
use tempfile::TempDir;

fn fixture() -> (TempDir, Dir) {
    let temp = TempDir::new().unwrap();
    let root = Dir::open_ambient_dir(temp.path(), cap_std::ambient_authority()).unwrap();
    (temp, root)
}

#[test]
fn edit_is_versioned_and_failed_save_preserves_contents() {
    let (_temp, root) = fixture();
    root.write("a.txt", "first").unwrap();
    let loaded = read(&root, "a.txt").unwrap();
    let saved = write(&root, "a.txt", "second", &loaded.version).unwrap();
    assert_ne!(saved.version, loaded.version);
    assert!(write(&root, "a.txt", "lost", &loaded.version).is_err());
    assert_eq!(root.read_to_string("a.txt").unwrap(), "second");
    assert_eq!(root.entries().unwrap().count(), 1, "no temporary files remain");
}

#[test]
fn rejects_absolute_parent_binary_large_and_missing_files() {
    let (temp, root) = fixture();
    root.write("binary", b"a\0b").unwrap();
    root.write("utf8", [0xff]).unwrap();
    root.write("large", vec![b'a'; MAX_TEXT_BYTES + 1]).unwrap();
    for path in ["../outside", "/etc/passwd", "a/../../x", "a\\..\\x", "", "binary", "utf8", "large"] {
        assert!(read(&root, path).is_err(), "{path}");
    }
    assert!(list(&root, temp.path().to_str().unwrap(), None).is_err());
    assert!(write(&root, "new", "text", "unknown").is_err());
}

#[test]
fn directory_pages_are_sorted_and_complete() {
    let (_temp, root) = fixture();
    for n in 0..DIRECTORY_PAGE_SIZE + 9 { root.write(format!("{n:04}"), "").unwrap(); }
    root.create_dir("directory").unwrap();
    let first = list(&root, "", None).unwrap();
    assert_eq!(first.entries.len(), DIRECTORY_PAGE_SIZE);
    let second = list(&root, "", first.next.as_deref()).unwrap();
    assert_eq!(second.entries.len(), 10);
    assert!(second.next.is_none());
    assert_eq!(second.entries.last().unwrap().kind, FileKindWire::Directory);
}

#[cfg(unix)]
#[test]
fn symlinks_cannot_escape_and_fifos_do_not_block() {
    use std::os::unix::fs::symlink;
    use std::ffi::CString;
    let (temp, root) = fixture();
    let outside = TempDir::new().unwrap();
    std::fs::write(outside.path().join("secret"), "secret").unwrap();
    symlink(outside.path(), temp.path().join("escape")).unwrap();
    symlink(outside.path().join("secret"), temp.path().join("link")).unwrap();
    assert!(list(&root, "escape", None).is_err());
    assert!(read(&root, "link").is_err());
    assert!(read(&root, "escape/secret").is_err());
    let fifo = CString::new(temp.path().join("pipe").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(read(&root, "pipe").is_err());
    let entries = list(&root, "", None).unwrap();
    assert!(entries.entries.iter().all(|entry| entry.kind == FileKindWire::Unsupported));
}

#[cfg(unix)]
#[test]
fn save_preserves_permissions_and_rejects_read_only_or_symlink_destination() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (temp, root) = fixture();
    root.write("script", "one").unwrap();
    std::fs::set_permissions(temp.path().join("script"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let doc = read(&root, "script").unwrap();
    write(&root, "script", "two", &doc.version).unwrap();
    assert_eq!(std::fs::metadata(temp.path().join("script")).unwrap().permissions().mode() & 0o777, 0o700);
    symlink("script", temp.path().join("alias")).unwrap();
    let alias = read(&root, "alias").unwrap();
    assert!(write(&root, "alias", "wrong", &alias.version).is_err());
    std::fs::set_permissions(temp.path().join("script"), std::fs::Permissions::from_mode(0o400)).unwrap();
    let doc = read(&root, "script").unwrap();
    assert!(write(&root, "script", "wrong", &doc.version).is_err());
}
