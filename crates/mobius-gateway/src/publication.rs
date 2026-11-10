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
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::Config(format!("{what} must be a regular file")));
    }
    let too_large = || Error::Config(format!("{what} is too large"));
    if metadata.len() > max_bytes {
        return Err(too_large());
    }
    let mut contents = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut contents)?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(too_large());
    }
    Ok(contents)
}

pub(crate) fn publish(path: &Path, contents: &[u8], create_new: bool) -> Result<()> {
    #[cfg(test)]
    if take_failure(path) {
        return Err(std::io::Error::other("injected replacement failure").into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| crate::Error::Config("publication path has no parent".into()))?;
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
