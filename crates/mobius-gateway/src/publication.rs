use std::fs;
use std::io::Write as _;
use std::path::Path;

use crate::{Error, Result};

/// Separates applied writes from failures that left the destination unchanged.
#[must_use]
pub(crate) struct Outcome(Option<Error>);

impl Outcome {
    pub(crate) fn applied(result: Result<()>) -> Result<Self> {
        match result {
            Ok(()) => Ok(Self(None)),
            Err(error @ Error::PublicationApplied { .. }) => Ok(Self(Some(error))),
            Err(error) => Err(error),
        }
    }

    /// Report uncertainty only after the owner has installed visible state and its side effects.
    pub(crate) fn confirm(self) -> Result<()> {
        self.0.map_or(Ok(()), Err)
    }
}

pub(crate) fn applied_error(error: Error) -> Error {
    match error {
        Error::PublicationApplied { .. } => error,
        source => Error::PublicationApplied {
            source: Box::new(source),
        },
    }
}

/// Reads the regular file at `path`, without following a final symlink, failing when it
/// exceeds `max_bytes`. `what` names the file in errors.
pub(crate) fn read_private(path: &Path, max_bytes: u64, what: &str) -> Result<Vec<u8>> {
    read_private_file(path, max_bytes, what, false)
}

pub(crate) fn read_secret(path: &Path, max_bytes: u64, what: &str) -> Result<Vec<u8>> {
    read_private_file(path, max_bytes, what, true).map_err(|error| match error {
        Error::Io(io) if io.raw_os_error() == Some(nix::libc::ELOOP) => {
            Error::Invalid(format!("{what} must be a regular file"))
        }
        error => error,
    })
}

fn read_private_file(path: &Path, max_bytes: u64, what: &str, owner_only: bool) -> Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::Invalid(format!("{what} must be a regular file")));
    }
    if owner_only {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::Invalid(format!(
                "{what} must be accessible only by its owner (use mode 0600)"
            )));
        }
    }
    let too_large = || Error::Invalid(format!("{what} is too large"));
    if metadata.len() > max_bytes {
        return Err(too_large());
    }
    mobius::read_within(file, max_bytes)?.ok_or_else(too_large)
}

pub(crate) fn publish(path: &Path, contents: &[u8], create_new: bool) -> Result<()> {
    #[cfg(test)]
    if take_failure(path) {
        return Err(std::io::Error::other("injected replacement failure").into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| crate::Error::Internal("publication path has no parent".into()))?;
    let parent_file = fs::File::open(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file().set_permissions(mobius::owner_only::file())?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    if create_new {
        file.persist_noclobber(path).map_err(|error| error.error)?;
    } else {
        file.persist(path).map_err(|error| error.error)?;
    }
    parent_file
        .sync_all()
        .map_err(|error| applied_error(error.into()))?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    static FAILURES: std::cell::RefCell<std::collections::VecDeque<std::path::PathBuf>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

#[cfg(test)]
pub(crate) fn fail_next_replacement(path: &Path) {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.into());
    FAILURES.with(|failures| failures.borrow_mut().push_back(path));
}

#[cfg(test)]
fn take_failure(path: &Path) -> bool {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.into());
    FAILURES.with(|failures| {
        let mut failures = failures.borrow_mut();
        if failures.front() == Some(&path) {
            failures.pop_front();
            true
        } else {
            false
        }
    })
}
