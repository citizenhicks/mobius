//! Unix owner-only permission values for native path or descriptor operations.
//!
//! The permission factories do not open paths or modify filesystem entries. Callers
//! retain their existing descriptor, synchronous path, or asynchronous path operation.
//! [`open`] is the one opener, for lock and record files shared across processes.

use std::fs::{File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

/// Allows only the owner to read and write a file.
#[must_use]
pub fn file() -> Permissions {
    Permissions::from_mode(0o600)
}

/// Opens or creates a read-write file only the owner can access, keeping its content.
///
/// A new file is created owner-only; an existing file is narrowed to owner-only.
/// # Errors
/// Returns the I/O error from opening the file or changing its permissions.
pub fn open(path: &Path) -> io::Result<File> {
    let opened = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(file().mode())
        .open(path)?;
    opened.set_permissions(file())?;
    Ok(opened)
}

/// Allows only the owner to read, write, and traverse a directory.
#[must_use]
pub fn dir() -> Permissions {
    Permissions::from_mode(0o700)
}
