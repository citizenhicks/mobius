//! Operator policy merged into a stopped gateway's configuration, so a hosting operator states
//! what it wants and the gateway owns its format, defaults and validation.

use std::io::Read;

use serde_json::{Map, Value};

use crate::config::GatewayConfig;
use crate::{Error, Result};

/// Far above any policy an operator sends; bounds what is read from stdin.
const MAX_POLICY_BYTES: u64 = 64 * 1024;

/// `config` with a JSON merge patch (RFC 7386) from `policy` applied: objects merge, other
/// values replace, and `null` removes a setting so its default applies.
pub(super) fn apply(config: &GatewayConfig, policy: impl Read) -> Result<GatewayConfig> {
    let bytes = mobius::read_within(policy, MAX_POLICY_BYTES)?.ok_or_else(|| {
        Error::Invalid(format!("operator policy exceeds {MAX_POLICY_BYTES} bytes"))
    })?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let patch: Value = serde_json::from_str(text)?;
    if !patch.is_object() {
        return Err(Error::Invalid(
            "operator policy must be a JSON object".into(),
        ));
    }
    let mut merged = serde_json::to_value(config)?;
    merge(&mut merged, patch);
    serde_json::from_value(merged)
        .map_err(|error| Error::Invalid(format!("operator policy is invalid: {error}")))
}

fn merge(target: &mut Value, patch: Value) {
    let Value::Object(patch) = patch else {
        *target = patch;
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    if let Value::Object(target) = target {
        for (key, value) in patch {
            if value.is_null() {
                target.remove(&key);
            } else {
                merge(target.entry(key).or_insert(Value::Null), value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_changes_only_what_it_names() {
        let root = tempfile::tempdir().unwrap();
        let listen = "127.0.0.1:8742".parse().unwrap();
        let (_, config) =
            crate::config::ConfigStore::initialize(root.path().join("state"), listen, None)
                .unwrap();
        assert_eq!(apply(&config, "{}".as_bytes()).unwrap(), config);
        let policy = r#"{"runtime":{"require_access_lease":true,"access_grace_seconds":0}}"#;
        let updated = apply(&config, policy.as_bytes()).unwrap();
        assert!(updated.runtime.require_access_lease);
        assert_eq!(updated.runtime.access_grace_seconds, 0);
        assert_eq!(updated.listen, config.listen);
        assert!(apply(&config, r#"{"runtime":{"unknown":1}}"#.as_bytes()).is_err());
    }
}
