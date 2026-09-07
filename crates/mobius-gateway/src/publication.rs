use std::fs;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use crate::Result;

pub(crate) fn publish(path: &Path, contents: &[u8], create_new: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| crate::Error::Config("publication path has no parent".into()))?;
    let parent_file = fs::File::open(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    if create_new {
        file.persist_noclobber(path).map_err(|error| error.error)?;
    } else {
        file.persist(path).map_err(|error| error.error)?;
    }
    parent_file.sync_all()?;
    Ok(())
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
