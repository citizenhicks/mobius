//! Capability-safe file reads, ranges, and atomic writes.

use std::ffi::OsStr;
use std::io;
use std::io::Read as _;
use std::io::Seek as _;
use std::io::Write as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use cap_std::fs::Dir;
use cap_std::fs::OpenOptions;
use cap_std::fs::Permissions;

use super::super::FileChangeFailure;
use super::super::MAX_FILE_BYTES;
use crate::Error;
use crate::Result;

/// Why a requested path was refused, named so the caller can choose another path.
#[derive(Clone, Copy)]
pub(super) enum Rejected {
    OutsideRoots,
    NoFileName,
    NotRegularFile,
    NotDirectory,
    Changed,
    Protected,
    Exists,
    Stale,
}

impl Rejected {
    pub(super) fn path(self, path: impl std::fmt::Display) -> Error {
        let reason = match self {
            Self::OutsideRoots => {
                "path is outside the workspace, attached folders and private temporary directory"
            }
            Self::NoFileName => "path does not name a file",
            Self::NotRegularFile => {
                "not a regular file; directories, symbolic links and devices are refused"
            }
            Self::NotDirectory => "a parent of the path is not a directory or is a symbolic link",
            Self::Changed => "path changed while it was being opened",
            Self::Protected => "path is protected",
            Self::Exists => "path already exists",
            Self::Stale => "file changed after the edit was prepared",
        };
        Error::Sandbox(format!("{reason}: {path}"))
    }
}

pub(super) fn read_file(root: Dir, relative: &Path, requested: &str) -> Result<String> {
    let bytes = read_limited(
        root,
        relative,
        requested,
        MAX_FILE_BYTES,
        "file exceeds read limit",
    )?;
    String::from_utf8(bytes)
        .map_err(|error| Error::Sandbox(format!("{requested} is not valid UTF-8: {error}")))
}

pub(super) fn read_limited(
    root: Dir,
    relative: &Path,
    requested: &str,
    max_bytes: usize,
    limit_error: &'static str,
) -> Result<Vec<u8>> {
    let file = open_regular_file(root, relative, requested)?;
    let mut bytes = Vec::new();
    file.take((max_bytes as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(Error::Sandbox(limit_error.into()));
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
        .map_err(|error| Error::Sandbox(format!("file range is unsupported: {error}")))?;
    let mut data = vec![0; length];
    file.read_exact(&mut data)?;
    let end = offset.saturating_add(length as u64);
    Ok((data, (end < size).then_some(end)))
}

fn open_regular_file(root: Dir, relative: &Path, requested: &str) -> Result<cap_std::fs::File> {
    let (parent, name) = open_named_parent(root, relative, requested)?;
    let before = regular_metadata(&parent, name, requested)?;
    open_checked_file(&parent, name, &before, requested)
}

fn open_checked_file(
    parent: &Dir,
    name: &OsStr,
    before: &cap_std::fs::Metadata,
    requested: &str,
) -> Result<cap_std::fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    {
        use cap_std::fs::OpenOptionsExt as _;

        // A checked regular file may become a FIFO before open; reject it without blocking.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = parent
        .open_with(name, &options)
        .map_err(|error| Error::Sandbox(format!("{requested}: {error}")))?;
    let opened = file.metadata()?;
    let current = parent
        .symlink_metadata(name)
        .map_err(|error| Error::Sandbox(format!("{requested}: {error}")))?;
    if !opened.is_file()
        || current.is_symlink()
        || !same_cap_file(before, &opened)
        || !same_cap_file(&opened, &current)
    {
        return Err(Rejected::Changed.path(requested));
    }
    Ok(file)
}

pub(super) fn remove_file(root: Dir, relative: &Path, requested: &str) -> Result<()> {
    let (parent, name) = open_named_parent(root, relative, requested)?;
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
    let (parent, target) = open_named_parent(root, relative, requested)?;
    let permissions = match parent.symlink_metadata(target) {
        Ok(metadata) if metadata.is_symlink() || !metadata.is_file() => {
            return Err(Rejected::NotRegularFile.path(requested));
        }
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    write_through_temporary(&parent, content, permissions, |temporary| {
        parent.rename(temporary, &parent, target)?;
        Ok(())
    })
    .map_err(Error::from)
}

/// A resolved file-tool path: its pinned root, the path below it and the path as requested.
pub(super) struct Location {
    pub(super) root: Dir,
    pub(super) relative: PathBuf,
    pub(super) requested: String,
}

/// Fails unless `location` could be created: its existing parents are real directories and
/// the file is absent.
pub(super) fn check_create(location: Location) -> Result<()> {
    let (parent, name) = split_name(&location.relative, &location.requested)?;
    if let Some(parent) = open_parent(
        location.root,
        parent,
        &location.requested,
        MissingParents::Allow,
    )? {
        ensure_absent(&parent, name, &location.requested)?;
    }
    Ok(())
}

/// Fails unless `location` is a regular file reached through real directories.
pub(super) fn check_regular(location: Location) -> Result<()> {
    let (parent, name) = open_named_parent(location.root, &location.relative, &location.requested)?;
    regular_metadata(&parent, name, &location.requested).map(drop)
}

/// Creates a new file, and any missing parent directories, without replacing an existing one.
pub(super) fn create_file(location: Location, content: &[u8]) -> ChangeResult {
    create_file_with(location, content, None).map(drop)
}

/// Atomically replaces a file that still holds `expected`, keeping its permissions. The
/// contents are compared once, just before the rename publishes the new file.
pub(super) fn replace_file(location: Location, expected: &[u8], content: &[u8]) -> ChangeResult {
    let requested = &location.requested;
    let (parent, name) = open_named_parent(location.root, &location.relative, requested)?;
    let permissions = regular_metadata(&parent, name, requested)?.permissions();
    write_through_temporary(&parent, content, Some(permissions), |temporary| {
        // ponytail: the file can still change between this check and the rename; closing
        // that gap needs a platform-specific exchanging rename.
        ensure_unchanged(&parent, name, expected, requested)?;
        parent.rename(temporary, &parent, name)?;
        Ok(())
    })
}

/// Writes `content` as a new `destination` with the source's permissions, then removes a
/// source that still holds `expected`. The two directories may differ, so this is not atomic.
pub(super) fn move_file(
    source: Location,
    destination: Location,
    expected: &[u8],
    content: &[u8],
) -> ChangeResult {
    let requested = &source.requested;
    let (parent, name) = open_named_parent(source.root, &source.relative, requested)?;
    let metadata = ensure_unchanged(&parent, name, expected, requested)?;
    let destination = create_file_with(destination, content, Some(metadata.permissions()))?;
    let removed = ensure_unchanged(&parent, name, expected, requested).and_then(|_| {
        parent.remove_file(name)?;
        Ok(())
    });
    if let Err(error) = removed {
        return Err(FileChangeFailure::SourceRemains { destination, error });
    }
    sync_directory(&parent).map_err(|error| unsynced(&error))
}

/// Removes a regular file that still holds `expected`.
pub(super) fn delete_file(location: Location, expected: &[u8]) -> ChangeResult {
    let requested = &location.requested;
    let (parent, name) = open_named_parent(location.root, &location.relative, requested)?;
    ensure_unchanged(&parent, name, expected, requested)?;
    parent.remove_file(name).map_err(Error::from)?;
    sync_directory(&parent).map_err(|error| unsynced(&error))
}

type ChangeResult = std::result::Result<(), FileChangeFailure>;

/// A change that is visible but whose directory could not be made durable.
fn unsynced(error: &Error) -> FileChangeFailure {
    FileChangeFailure::Uncertain(Error::Sandbox(format!(
        "the change was made, but its directory could not be synced: {error}"
    )))
}

/// Creates the file and returns its requested path.
fn create_file_with(
    location: Location,
    content: &[u8],
    permissions: Option<Permissions>,
) -> std::result::Result<String, FileChangeFailure> {
    let Location {
        root,
        relative,
        requested,
    } = location;
    let (parent, name) = split_name(&relative, &requested)?;
    // ponytail: directories created here stay if the file then fails; they hold nothing.
    let parent = open_parent(root, parent, &requested, MissingParents::Create)?
        .ok_or_else(|| Rejected::NotDirectory.path(&requested))?;
    write_through_temporary(&parent, content, permissions, |temporary| {
        // A hard link publishes the complete file and fails if the name already exists.
        parent
            .hard_link(temporary, &parent, name)
            .map_err(|error| match error.kind() {
                io::ErrorKind::AlreadyExists => Rejected::Exists.path(&requested),
                _ => error.into(),
            })?;
        if let Err(error) = parent.remove_file(temporary) {
            tracing::debug!(%error, "created file could not remove its temporary link");
        }
        Ok(())
    })?;
    Ok(requested)
}

/// Writes `content` to a new temporary file in `parent`, then lets `publish` move it into
/// place. The temporary file is removed when publishing fails.
fn write_through_temporary(
    parent: &Dir,
    content: &[u8],
    permissions: Option<Permissions>,
    publish: impl FnOnce(&str) -> Result<()>,
) -> ChangeResult {
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
        publish(&temporary)
    })();
    if let Err(error) = result {
        if let Err(error) = parent.remove_file(&temporary)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::debug!(%error, "failed write could not remove its temporary file");
        }
        return Err(error.into());
    }
    sync_directory(parent).map_err(|error| unsynced(&error))
}

fn ensure_absent(parent: &Dir, name: &OsStr, requested: &str) -> Result<()> {
    match parent.symlink_metadata(name) {
        Ok(_) => Err(Rejected::Exists.path(requested)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Sandbox(format!("{requested}: {error}"))),
    }
}

fn regular_metadata(parent: &Dir, name: &OsStr, requested: &str) -> Result<cap_std::fs::Metadata> {
    let metadata = parent
        .symlink_metadata(name)
        .map_err(|error| Error::Sandbox(format!("{requested}: {error}")))?;
    if metadata.is_symlink() || !metadata.is_file() {
        return Err(Rejected::NotRegularFile.path(requested));
    }
    Ok(metadata)
}

/// Fails unless the regular file `name` still holds exactly `expected`, comparing as it
/// reads rather than copying the file.
fn ensure_unchanged(
    parent: &Dir,
    name: &OsStr,
    expected: &[u8],
    requested: &str,
) -> Result<cap_std::fs::Metadata> {
    let before = regular_metadata(parent, name, requested)?;
    if before.len() != expected.len() as u64 {
        return Err(Rejected::Stale.path(requested));
    }
    let mut file = open_checked_file(parent, name, &before, requested)?;
    let mut buffer = [0_u8; 16 * 1024];
    let mut remaining = expected;
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        remaining = buffer
            .get(..read)
            .and_then(|chunk| remaining.strip_prefix(chunk))
            .ok_or_else(|| Rejected::Stale.path(requested))?;
    }
    if !remaining.is_empty() {
        return Err(Rejected::Stale.path(requested));
    }
    Ok(before)
}

fn sync_directory(directory: &Dir) -> Result<()> {
    directory.open(".")?.sync_all()?;
    Ok(())
}
/// Opens the directory holding `relative` and returns it with the final name.
fn open_named_parent<'path>(
    root: Dir,
    relative: &'path Path,
    requested: &str,
) -> Result<(Dir, &'path OsStr)> {
    let (parent, name) = split_name(relative, requested)?;
    let parent = open_parent(root, parent, requested, MissingParents::Fail)?
        .ok_or_else(|| Rejected::NotDirectory.path(requested))?;
    Ok((parent, name))
}

fn split_name<'path>(
    relative: &'path Path,
    requested: &str,
) -> Result<(&'path Path, &'path OsStr)> {
    let name = relative
        .file_name()
        .ok_or_else(|| Rejected::NoFileName.path(requested))?;
    Ok((relative.parent().unwrap_or_else(|| Path::new("")), name))
}

/// What opening a parent chain does at a missing directory.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MissingParents {
    /// Fails with the filesystem error.
    Fail,
    /// Returns no directory: everything below it would be created.
    Allow,
    /// Creates the directory and continues.
    Create,
}

fn open_parent(
    mut parent: Dir,
    path: &Path,
    requested: &str,
    missing: MissingParents,
) -> Result<Option<Dir>> {
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let before = match parent.symlink_metadata(name) {
            Err(error)
                if error.kind() == io::ErrorKind::NotFound && missing != MissingParents::Fail =>
            {
                if missing == MissingParents::Allow {
                    return Ok(None);
                }
                match parent.create_dir(name) {
                    Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
                        return Err(Error::Sandbox(format!("{requested}: {error}")));
                    }
                    _ => sync_directory(&parent)?,
                }
                parent.symlink_metadata(name)
            }
            metadata => metadata,
        }
        .map_err(|error| Error::Sandbox(format!("{requested}: {error}")))?;
        if before.is_symlink() || !before.is_dir() {
            return Err(Rejected::NotDirectory.path(requested));
        }
        let next = parent
            .open_dir(name)
            .map_err(|error| Error::Sandbox(format!("{requested}: {error}")))?;
        if !same_cap_file(&before, &next.dir_metadata()?) {
            return Err(Rejected::Changed.path(requested));
        }
        parent = next;
    }
    Ok(Some(parent))
}

fn same_cap_file(left: &cap_std::fs::Metadata, right: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::time::Duration;

    #[test]
    fn sandbox_file_open_is_nonblocking() {
        let directory = tempfile::tempdir().expect("workspace");
        let path = directory.path().join("file");
        std::fs::write(&path, b"regular").expect("regular file");
        let parent = Dir::open_ambient_dir(directory.path(), cap_std::ambient_authority())
            .expect("workspace capability");
        let before = parent.symlink_metadata("file").expect("checked metadata");
        std::fs::remove_file(&path).expect("replace regular file");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .expect("create FIFO")
                .success()
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        let _reader = std::thread::spawn(move || {
            let rejected =
                open_checked_file(&parent, std::ffi::OsStr::new("file"), &before, "file").is_err();
            sender.send(rejected).expect("report FIFO result");
        });
        match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(rejected) => assert!(rejected, "a replaced FIFO must be rejected"),
            Err(error) => {
                // Release a regressed blocking open before failing, without blocking this test.
                let _rescue = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&path);
                let _released = receiver.recv_timeout(Duration::from_secs(2));
                panic!("file open did not reject the FIFO promptly: {error}");
            }
        }
    }
}
