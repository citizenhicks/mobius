//! Capability-safe file reads, ranges, and atomic writes.

use std::io::Read as _;
use std::io::Seek as _;
use std::io::Write as _;
use std::path::Component;
use std::path::Path;

use cap_std::fs::Dir;
use cap_std::fs::OpenOptions;

use super::super::MAX_FILE_BYTES;
use crate::Error;
use crate::Result;

pub(super) fn read_file(root: Dir, relative: &Path, requested: &str) -> Result<String> {
    let file = open_regular_file(root, relative, requested)?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(Error::Sandbox("file exceeds read limit".into()));
    }
    String::from_utf8(bytes).map_err(|_| Error::Sandbox(format!("{requested} is not valid UTF-8")))
}

pub(super) fn read_binary_file(
    root: Dir,
    relative: &Path,
    requested: &str,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    let file = open_regular_file(root, relative, requested)?;
    let mut bytes = Vec::new();
    file.take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(Error::Sandbox("file exceeds binary read limit".into()));
    }
    Ok(bytes)
}

pub(super) fn read_file_range(
    root: Dir,
    relative: &Path,
    requested: &str,
    offset: u64,
    max_bytes: usize,
) -> Result<(Vec<u8>, Option<u64>)> {
    let mut file = open_regular_file(root, relative, requested)?;
    let size = file.metadata()?.len();
    if offset > size {
        return Err(Error::Sandbox("file offset exceeds its size".into()));
    }
    file.seek(std::io::SeekFrom::Start(offset))?;
    let length = usize::try_from(size.saturating_sub(offset).min(max_bytes as u64))
        .map_err(|_| Error::Sandbox("file range is unsupported".into()))?;
    let mut data = vec![0; length];
    file.read_exact(&mut data)?;
    let end = offset.saturating_add(length as u64);
    Ok((data, (end < size).then_some(end)))
}

fn open_regular_file(root: Dir, relative: &Path, requested: &str) -> Result<cap_std::fs::File> {
    let name = relative
        .file_name()
        .ok_or_else(|| Error::Sandbox(requested.to_string()))?;
    let parent = open_parent(root, relative.parent().unwrap_or(Path::new("")), requested)?;
    let before = parent
        .symlink_metadata(name)
        .map_err(|_| Error::Sandbox(requested.to_string()))?;
    if before.is_symlink() || !before.is_file() {
        return Err(Error::Sandbox(requested.to_string()));
    }
    let file = parent
        .open(name)
        .map_err(|_| Error::Sandbox(requested.to_string()))?;
    let opened = file.metadata()?;
    let current = parent
        .symlink_metadata(name)
        .map_err(|_| Error::Sandbox(requested.to_string()))?;
    if !opened.is_file()
        || current.is_symlink()
        || !same_cap_file(&before, &opened)
        || !same_cap_file(&opened, &current)
    {
        return Err(Error::Sandbox(requested.to_string()));
    }
    Ok(file)
}

pub(super) fn remove_file(root: Dir, relative: &Path, requested: &str) -> Result<()> {
    let name = relative
        .file_name()
        .ok_or_else(|| Error::Sandbox(requested.into()))?;
    let parent = open_parent(root, relative.parent().unwrap_or(Path::new("")), requested)?;
    let metadata = parent.symlink_metadata(name)?;
    if metadata.is_symlink() {
        return Err(Error::Sandbox(format!(
            "cannot remove symbolic link: {requested}"
        )));
    }
    if !metadata.is_file() {
        return Err(Error::Sandbox(format!(
            "cannot remove non-regular file: {requested}"
        )));
    }
    parent.remove_file(name)?;
    sync_directory(&parent)
}

pub(super) fn atomic_write(
    root: Dir,
    relative: &Path,
    content: &[u8],
    requested: &str,
) -> Result<()> {
    let target = relative
        .file_name()
        .ok_or_else(|| Error::Sandbox(requested.to_string()))?;
    let parent = open_parent(root, relative.parent().unwrap_or(Path::new("")), requested)?;
    let permissions = match parent.symlink_metadata(target) {
        Ok(metadata) if metadata.is_symlink() || !metadata.is_file() => {
            return Err(Error::Sandbox(requested.to_string()));
        }
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let temporary = format!(".mobius-write-{}.tmp", uuid::Uuid::new_v4());
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = parent.open_with(&temporary, &options)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        parent.rename(&temporary, &parent, target)?;
        sync_directory(&parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = parent.remove_file(&temporary);
    }
    result
}

fn sync_directory(directory: &Dir) -> Result<()> {
    directory.open(".")?.sync_all()?;
    Ok(())
}
fn open_parent(mut parent: Dir, path: &Path, requested: &str) -> Result<Dir> {
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let before = parent
            .symlink_metadata(name)
            .map_err(|_| Error::Sandbox(requested.to_string()))?;
        if before.is_symlink() || !before.is_dir() {
            return Err(Error::Sandbox(requested.to_string()));
        }
        let next = parent
            .open_dir(name)
            .map_err(|_| Error::Sandbox(requested.to_string()))?;
        if !same_cap_file(&before, &next.dir_metadata()?) {
            return Err(Error::Sandbox(requested.to_string()));
        }
        parent = next;
    }
    Ok(parent)
}

#[cfg(unix)]
fn same_cap_file(left: &cap_std::fs::Metadata, right: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_cap_file(_left: &cap_std::fs::Metadata, _right: &cap_std::fs::Metadata) -> bool {
    false
}
