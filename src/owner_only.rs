//! Unix owner-only permission values for native path or descriptor operations.
//!
//! These factories do not open paths or modify filesystem entries. Callers retain
//! their existing descriptor, synchronous path, or asynchronous path operation.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt as _;

/// Allows only the owner to read and write a file.
#[must_use]
pub fn file() -> Permissions {
    Permissions::from_mode(0o600)
}

/// Allows only the owner to read, write, and traverse a directory.
#[must_use]
pub fn dir() -> Permissions {
    Permissions::from_mode(0o700)
}
