//! Capability-based file access: every operation stays below an opened session root.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Component, Path};

use cap_std::fs::{Dir, OpenOptions};
use oximux_remote_proto::files::{
    DirectoryEntryWire, DirectoryWire, FileKindWire, TextFileWire,
    DIRECTORY_PAGE_SIZE, MAX_TEXT_BYTES,
};
use oximux_remote_proto::proto::RpcError;
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, RpcError>;

fn invalid(message: &str) -> RpcError { RpcError::BadRequest(message.into()) }
fn unavailable(_: std::io::Error) -> RpcError { invalid("file access unavailable") }

fn relative(path: &str, root_allowed: bool) -> Result<&Path> {
    if path.len() > 4096 || path.contains('\\') || path.contains('\0') {
        return Err(invalid("invalid relative path"));
    }
    let value = Path::new(path);
    if (path.is_empty() || path == ".") && root_allowed { return Ok(Path::new(".")); }
    if path.is_empty() || value.components().any(|part| !matches!(part, Component::Normal(_))) {
        return Err(invalid("invalid relative path"));
    }
    Ok(value)
}

pub(crate) fn list(root: &Dir, path: &str, after: Option<&str>) -> Result<DirectoryWire> {
    let path = if path == "." { "" } else { path };
    let directory = root.open_dir(relative(path, true)?).map_err(unavailable)?;
    let mut entries = BTreeMap::new();
    for entry in directory.entries().map_err(unavailable)? {
        let entry = entry.map_err(unavailable)?;
        let Ok(name) = entry.file_name().into_string() else { continue; };
        if after.is_some_and(|cursor| name.as_str() <= cursor) { continue; }
        let kind = entry.file_type().map_err(unavailable)?;
        let kind = if kind.is_dir() { FileKindWire::Directory }
            else if kind.is_file() { FileKindWire::File } else { FileKindWire::Unsupported };
        entries.insert(name, kind);
        if entries.len() > DIRECTORY_PAGE_SIZE + 1 { entries.pop_last(); }
    }
    let more = entries.len() > DIRECTORY_PAGE_SIZE;
    if more { entries.pop_last(); }
    let next = more.then(|| entries.last_key_value().unwrap().0.clone());
    Ok(DirectoryWire {
        path: path.into(),
        entries: entries.into_iter().map(|(name, kind)| DirectoryEntryWire { name, kind }).collect(),
        next,
    })
}

fn open_text(root: &Dir, path: &Path) -> Result<(cap_std::fs::File, String)> {
    let mut options = OpenOptions::new();
    options.read(true);
    // A file can be replaced by a FIFO between a metadata check and open.
    #[cfg(unix)] {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = root.open_with(path, &options).map_err(unavailable)?;
    if !file.metadata().map_err(unavailable)?.is_file() { return Err(invalid("not a regular text file")); }
    let mut bytes = Vec::new();
    (&mut file).take(MAX_TEXT_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(unavailable)?;
    if bytes.len() > MAX_TEXT_BYTES { return Err(invalid("text file exceeds 2 MiB")); }
    if bytes.contains(&0) { return Err(invalid("binary files cannot be edited")); }
    let text = String::from_utf8(bytes).map_err(|_| invalid("file is not UTF-8 text"))?;
    Ok((file, text))
}

fn version(text: &str) -> String { format!("{:x}", Sha256::digest(text.as_bytes())) }

pub(crate) fn read(root: &Dir, path: &str) -> Result<TextFileWire> {
    let (_, text) = open_text(root, relative(path, false)?)?;
    Ok(TextFileWire { path: path.into(), version: version(&text), text })
}

pub(crate) fn write(root: &Dir, path: &str, text: &str, expected: &str) -> Result<TextFileWire> {
    let path_value = relative(path, false)?;
    if text.len() > MAX_TEXT_BYTES || text.contains('\0') { return Err(invalid("invalid text contents")); }
    let parent = path_value.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path_value.file_name().ok_or_else(|| invalid("invalid relative path"))?;
    let directory = root.open_dir(parent).map_err(unavailable)?;
    // Replacing a symlink would edit a different object than the one read.
    if !directory.symlink_metadata(name).map_err(unavailable)?.is_file() {
        return Err(invalid("only existing regular files can be saved"));
    }
    let (original, current) = open_text(&directory, Path::new(name))?;
    let permissions = original.metadata().map_err(unavailable)?.permissions();
    if permissions.readonly() { return Err(invalid("file is read-only on the host")); }
    if version(&current) != expected { return Err(invalid("file changed on host; reload before saving")); }
    drop(original);
    let temporary = format!(".oximux-save-{:032x}", rand::random::<u128>());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = directory.open_with(&temporary, &options).map_err(unavailable)?;
    let result = (|| {
        file.set_permissions(permissions).map_err(unavailable)?;
        file.write_all(text.as_bytes()).map_err(unavailable)?;
        file.sync_all().map_err(unavailable)?;
        drop(file);
        // Recheck immediately before replacement; dispatcher writes are serialized.
        if version(&open_text(&directory, Path::new(name))?.1) != expected {
            return Err(invalid("file changed on host; reload before saving"));
        }
        if !directory.symlink_metadata(name).map_err(unavailable)?.is_file() {
            return Err(invalid("only existing regular files can be saved"));
        }
        directory.rename(&temporary, &directory, name).map_err(unavailable)?;
        Ok(TextFileWire { path: path.into(), text: text.into(), version: version(text) })
    })();
    if result.is_err() { let _ = directory.remove_file(&temporary); }
    result
}

#[cfg(test)]
mod tests;
