//! Transactional, append-only delivery of middleware guidance.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::Value;

use crate::identifier::{AsciiCase, valid_ascii_identifier};
use crate::{Error, Result};

pub(crate) const FIELD: &str = "_mobius_delivery_once";
pub(crate) type Receipts = Arc<BTreeMap<String, BTreeSet<String>>>;

pub(crate) struct DeliveryOnce<'a> {
    receipts: &'a Receipts,
    pub(crate) owner: &'static str,
}

impl<'a> DeliveryOnce<'a> {
    pub(crate) fn new(receipts: &'a Receipts) -> Self {
        Self {
            receipts,
            owner: "",
        }
    }

    #[cfg(test)]
    pub(crate) fn testing() -> Self {
        static EMPTY: std::sync::LazyLock<Receipts> = std::sync::LazyLock::new(Receipts::default);
        Self {
            receipts: &EMPTY,
            owner: "test",
        }
    }

    pub(crate) fn deliver<T: std::borrow::Borrow<Value> + From<Value>>(
        &self,
        input: &mut Vec<T>,
        key: Option<&str>,
        make: impl FnOnce() -> Value,
    ) -> Result<bool> {
        let Some(key) = key else {
            input.push(make().into());
            return Ok(true);
        };
        if self.owner.is_empty() || !valid_ascii_identifier(key, 384, AsciiCase::Any, b"_-.:/") {
            return Err(Error::Config("invalid once-only guidance key".into()));
        }
        if contains(self.receipts, self.owner, key)
            || input
                .iter()
                .any(|item| identity(item.borrow()) == Some((self.owner, key)))
        {
            return Ok(false);
        }
        let mut item = make();
        if item.get("role").and_then(Value::as_str) != Some("user")
            || crate::protocol::internal_message_kind(&item).is_none()
        {
            return Err(Error::Config(
                "once-only guidance requires an internal user message".into(),
            ));
        }
        item[FIELD] = serde_json::json!({"owner": self.owner, "key": key});
        input.push(item.into());
        Ok(true)
    }
}

fn contains(receipts: &Receipts, owner: &str, key: &str) -> bool {
    receipts.get(owner).is_some_and(|keys| keys.contains(key))
}

fn identity(item: &Value) -> Option<(&str, &str)> {
    let receipt = item.get(FIELD)?;
    Some((
        receipt.get("owner")?.as_str()?,
        receipt.get("key")?.as_str()?,
    ))
}

/// Accept only the first occurrence into the same transaction as its context item.
pub(crate) fn accept(receipts: &mut Receipts, item: &Value) -> bool {
    let Some((owner, key)) = identity(item) else {
        return true;
    };
    if contains(receipts, owner, key) {
        return false;
    }
    Arc::make_mut(receipts)
        .entry(owner.into())
        .or_default()
        .insert(key.into());
    true
}

/// Preserve inherited receipts and accept newly appended session-start guidance.
pub(crate) fn record<T: std::borrow::Borrow<Value>>(receipts: &mut Receipts, input: &[T]) -> bool {
    let mut changed = false;
    for item in input {
        let item = item.borrow();
        if identity(item).is_some() {
            changed |= accept(receipts, item);
        }
    }
    changed
}

/// Undo only receipts inserted by the context suffix being rolled back.
pub(crate) fn rollback<T: std::borrow::Borrow<Value>>(receipts: &mut Receipts, input: &[T]) {
    for item in input {
        let item = item.borrow();
        let Some((owner, key)) = identity(item) else {
            continue;
        };
        let receipts = Arc::make_mut(receipts);
        if let Some(keys) = receipts.get_mut(owner) {
            keys.remove(key);
            if keys.is_empty() {
                receipts.remove(owner);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        checkpoint::Checkpoint,
        model::{internal_user_message, user_message},
    };

    #[test]
    fn guidance_keeps_order_scopes_receipts_and_survives_context_compaction() {
        let mut receipts = Receipts::default();
        let mut input = vec![user_message("existing prefix")];
        {
            let mut delivery = DeliveryOnce::new(&receipts);
            delivery.owner = "first";
            assert!(
                delivery
                    .deliver(&mut input, Some("intro"), || internal_user_message(
                        "guidance", "first"
                    ))
                    .unwrap()
            );
            assert!(
                !delivery
                    .deliver(&mut input, Some("intro"), || panic!(
                        "repeat must stay lazy"
                    ))
                    .unwrap()
            );
            delivery.owner = "second";
            assert!(
                delivery
                    .deliver(&mut input, Some("intro"), || internal_user_message(
                        "guidance", "second"
                    ))
                    .unwrap()
            );
        }
        assert!(receipts.is_empty(), "staging cannot consume a receipt");
        assert_eq!(input[0], user_message("existing prefix"));
        assert_eq!(input[1]["content"][0]["text"], "first");
        assert_eq!(input[2]["content"][0]["text"], "second");
        input.retain(|item| accept(&mut receipts, item));
        assert_eq!(receipts.len(), 2);
        rollback(&mut receipts, &input[1..]);
        assert!(receipts.is_empty(), "failed acceptance remains retryable");
        assert!(record(&mut receipts, &input));
        let unchanged = Arc::clone(&receipts);
        assert!(!record(&mut receipts, &input));
        assert!(
            Arc::ptr_eq(&unchanged, &receipts),
            "repeated notices cannot copy receipts"
        );
        let mut checkpoint = Checkpoint::empty("session");
        checkpoint.context = std::sync::Arc::new(
            vec![user_message("compacted context")]
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
        );
        checkpoint.delivered_once = receipts;
        let mut encoded = serde_json::to_value(checkpoint).unwrap();
        let checkpoint = <Checkpoint as serde::Deserialize>::deserialize(&encoded).unwrap();
        encoded.as_object_mut().unwrap().remove("delivered_once");
        assert!(
            serde_json::from_value::<Checkpoint>(encoded)
                .unwrap()
                .delivered_once
                .is_empty()
        );
        let mut delivery = DeliveryOnce::new(&checkpoint.delivered_once);
        delivery.owner = "first";
        assert!(
            !delivery
                .deliver(&mut Vec::<Value>::new(), Some("intro"), || panic!(
                    "resume must not reissue guidance"
                ))
                .unwrap()
        );
    }
}
