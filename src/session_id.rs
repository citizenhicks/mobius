//! Checked identity shared by session storage, runtime and gateway boundaries.

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};

use crate::{Error, Result};

/// A nonblank session identity of at most 4096 UTF-8 bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SessionId(Arc<str>);

impl SessionId {
    /// Checks an identity received from a caller or persisted data.
    /// # Errors
    /// Returns an error when the identity is blank or exceeds 4096 bytes.
    pub fn new(value: impl Into<Arc<str>>) -> Result<Self> {
        let value = value.into();
        if value.trim().is_empty() || value.len() > 4_096 {
            return Err(Error::Config("session ID must be 1–4096 bytes".into()));
        }
        Ok(Self(value))
    }

    /// Creates a fresh UUID session identity.
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string().into())
    }

    /// Shares the immutable identity without copying its text.
    #[must_use]
    pub fn share(&self) -> Self {
        Self(Arc::clone(&self.0))
    }

    /// Borrows the identity at a text boundary.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for SessionId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl TryFrom<String> for SessionId {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl std::ops::Deref for SessionId {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for SessionId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::new(Arc::<str>::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_preserve_text_and_reject_invalid_ids() {
        for value in ["", " \t\n", &"é".repeat(2049)] {
            assert!(SessionId::new(value).is_err());
            assert!(serde_json::from_value::<SessionId>(value.into()).is_err());
        }
        let value = "é".repeat(2048);
        let id = SessionId::new(value.as_str()).unwrap();
        assert_eq!(serde_json::to_value(&id).unwrap(), serde_json::json!(value));
        assert_eq!(
            serde_json::from_value::<SessionId>(value.into()).unwrap(),
            id
        );
        assert!(Arc::ptr_eq(&id.0, &id.share().0));
    }
}
