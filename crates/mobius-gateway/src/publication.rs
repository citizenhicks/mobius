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

pub(crate) fn publish(path: &Path, contents: &[u8], create_new: bool) -> Result<()> {
    #[cfg(test)]
    if take_failure(path, false) {
        return Err(std::io::Error::other("injected replacement failure").into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| crate::Error::Config("publication path has no parent".into()))?;
    let parent_file = fs::File::open(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    file.as_file().set_permissions(mobius::owner_only::file())?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    if create_new {
        file.persist_noclobber(path).map_err(|error| error.error)?;
    } else {
        file.persist(path).map_err(|error| error.error)?;
    }
    #[cfg(test)]
    if take_failure(path, true) {
        return Err(applied_error(
            std::io::Error::other("injected directory sync failure").into(),
        ));
    }
    parent_file
        .sync_all()
        .map_err(|error| applied_error(error.into()))?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    static FAILURES: std::cell::RefCell<std::collections::VecDeque<(std::path::PathBuf, bool)>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

#[cfg(test)]
pub(crate) fn fail_next_directory_sync(path: &Path) {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.into());
    FAILURES.with(|failures| failures.borrow_mut().push_back((path, true)));
}

#[cfg(test)]
pub(crate) fn fail_next_replacement(path: &Path) {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.into());
    FAILURES.with(|failures| failures.borrow_mut().push_back((path, false)));
}

#[cfg(test)]
fn take_failure(path: &Path, after_rename: bool) -> bool {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.into());
    FAILURES.with(|failures| {
        let mut failures = failures.borrow_mut();
        if failures
            .front()
            .is_some_and(|(target, after)| target == &path && *after == after_rename)
        {
            failures.pop_front();
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_replaces_existing_file() {
        let directory = tempfile::tempdir().expect("publication directory");
        let path = directory.path().join("state");
        fs::write(&path, b"old").expect("old state");

        publish(&path, b"new", false).expect("replace state");

        assert_eq!(fs::read(path).expect("read state"), b"new");
    }

    #[test]
    fn publish_create_new_rejects_existing_file() {
        let directory = tempfile::tempdir().expect("publication directory");
        let path = directory.path().join("state");
        fs::write(&path, b"old").expect("old state");

        assert!(publish(&path, b"new", true).is_err());
        assert_eq!(fs::read(path).expect("read state"), b"old");
    }

    #[test]
    fn publish_propagates_parent_errors() {
        let directory = tempfile::tempdir().expect("publication directory");
        let path = directory.path().join("missing").join("state");

        assert!(publish(&path, b"state", false).is_err());
    }
}
