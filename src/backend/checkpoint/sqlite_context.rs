//! SQLite's split checkpoint header and append-only active context.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use rusqlite::{OptionalExtension, Transaction, params};
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use serde_json::Value;

use super::super::Checkpoint;
use crate::{Error, Result};

/// An identity hint for a live session on one SQLite connection.
/// Weak item handles neither retain payloads nor force context-vector copy-on-write.
pub(super) struct ContextCache {
    sequence: u64,
    epoch: u64,
    data_version: i64,
    items: Vec<Weak<Value>>,
}

impl ContextCache {
    pub(super) fn record(
        contexts: &mut HashMap<String, Self>,
        checkpoint: &Checkpoint,
        data_version: i64,
    ) {
        contexts.retain(|_, cache| cache.items.iter().any(|item| item.strong_count() != 0));
        if checkpoint.context.is_empty() {
            contexts.remove(checkpoint.session_id.as_str());
            return;
        }
        // Reuse the owned key and Weak allocation on a hit; do not copy the session ID per save.
        let (session_id, mut cache) = contexts
            .remove_entry(checkpoint.session_id.as_str())
            .unwrap_or_else(|| {
                (
                    checkpoint.session_id.to_owned(),
                    Self {
                        sequence: checkpoint.sequence,
                        epoch: checkpoint.context_epoch,
                        data_version,
                        items: Vec::new(),
                    },
                )
            });
        cache.sequence = checkpoint.sequence;
        cache.epoch = checkpoint.context_epoch;
        cache.data_version = data_version;
        cache.items.clear();
        cache
            .items
            .extend(checkpoint.context.iter().map(Arc::downgrade));
        contexts.insert(session_id, cache);
    }
}

/// Borrowed storage projection; the public checkpoint JSON still includes context.
pub(super) struct Header<'a>(pub(super) &'a Checkpoint);

impl Serialize for Header<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        // Exhaustive matching makes a newly added checkpoint field a compile-time decision.
        let Checkpoint {
            #[cfg(test)]
                clone_count: _,
            context: _,
            version,
            session_id,
            session_context,
            metadata,
            catalog_visible,
            first_user_message,
            model_route,
            context_model_route,
            sequence,
            delivered_once,
            context_epoch,
            last_context_rewrite,
            total_usage,
            last_usage,
            pending_messages,
            active_execution,
            active_model_step,
            execution_stats,
            pending_tools,
            pending_approval,
        } = self.0;
        let mut header = serializer.serialize_struct("Checkpoint", 20)?;
        header.serialize_field("version", version)?;
        header.serialize_field("session_id", session_id)?;
        header.serialize_field("session_context", session_context)?;
        header.serialize_field("metadata", metadata)?;
        header.serialize_field("catalog_visible", catalog_visible)?;
        header.serialize_field("first_user_message", first_user_message)?;
        header.serialize_field("model_route", model_route)?;
        header.serialize_field("context_model_route", context_model_route)?;
        header.serialize_field("sequence", sequence)?;
        header.serialize_field("delivered_once", delivered_once)?;
        header.serialize_field("context_epoch", context_epoch)?;
        header.serialize_field("last_context_rewrite", last_context_rewrite)?;
        header.serialize_field("total_usage", total_usage)?;
        header.serialize_field("last_usage", last_usage)?;
        header.serialize_field("pending_messages", pending_messages)?;
        header.serialize_field("active_execution", active_execution)?;
        header.serialize_field("active_model_step", active_model_step)?;
        header.serialize_field("execution_stats", execution_stats)?;
        header.serialize_field("pending_tools", pending_tools)?;
        header.serialize_field("pending_approval", pending_approval)?;
        header.end()
    }
}

pub(super) fn sqlite_integer(value: u64, name: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Checkpoint(format!("{name} exceeds SQLite INTEGER")))
}

pub(super) fn context_count(checkpoint: &Checkpoint) -> Result<i64> {
    i64::try_from(checkpoint.context.len())
        .map_err(|_| Error::Checkpoint("context length exceeds SQLite INTEGER".into()))
}

pub(super) const CONTEXT_PREFIX_SQL: &str = "SELECT item_index, item_json FROM context_items
     WHERE session_id = ?1 AND epoch = ?2 AND item_index < ?3 ORDER BY item_index";

/// Verifies immutable same-epoch prefixes before any metadata can advance.
/// Live sessions keep independent hints; warm saves compare identities without reading rows.
/// Cold or changed identities use one ordered query, never one query per item.
/// SQLite data_version also rejects hints invalidated by another connection, including
/// deletion/recreation of the same session and sequence. Capture it before reading
/// the transaction snapshot, not after commit, when another writer could have run.
pub(super) fn prepare_append(
    transaction: &Transaction<'_>,
    checkpoint: &Checkpoint,
    cached: Option<&ContextCache>,
    data_version: i64,
) -> Result<(usize, bool)> {
    let durable = transaction.prepare_cached(
        "SELECT latest_sequence, context_epoch, context_count FROM sessions WHERE session_id = ?1")?.query_row(
        [&checkpoint.session_id],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
    ).optional()?;
    let Some((sequence, epoch, count)) = durable else {
        return Ok((0, false));
    };
    let next_sequence = sqlite_integer(checkpoint.sequence, "checkpoint sequence")?;
    let next_epoch = sqlite_integer(checkpoint.context_epoch, "context epoch")?;
    if next_sequence <= sequence {
        return Err(Error::Checkpoint(
            "checkpoint sequence did not advance".into(),
        ));
    }
    if next_epoch < epoch {
        return Err(Error::Checkpoint(
            "context epoch cannot move backwards".into(),
        ));
    }
    if next_epoch != epoch {
        transaction.execute(
            "DELETE FROM context_items WHERE session_id = ?1",
            [&checkpoint.session_id],
        )?;
        return Ok((0, true));
    }
    let stored_count = count;
    let count = usize::try_from(count)
        .map_err(|_| Error::Checkpoint("stored context length is invalid".into()))?;
    if checkpoint.context.len() < count {
        return Err(Error::Checkpoint(
            "context prefix was truncated without advancing its epoch".into(),
        ));
    }
    let cached = cached.filter(|cache| {
        i64::try_from(cache.sequence).ok() == Some(sequence)
            && cache.epoch == checkpoint.context_epoch
            && cache.data_version == data_version
            && cache.items.len() == count
    });
    if count == 0
        || cached.is_some_and(|cache| {
            cache
                .items
                .iter()
                .zip(checkpoint.context.iter())
                .all(|(old, item)| old.as_ptr() == Arc::as_ptr(item))
        })
    {
        return Ok((count, false));
    }
    let mut statement = transaction.prepare_cached(CONTEXT_PREFIX_SQL)?;
    let mut rows = statement.query(params![checkpoint.session_id, epoch, stored_count])?;
    for (index, item) in checkpoint.context.iter().take(count).enumerate() {
        let row = rows
            .next()?
            .ok_or_else(|| Error::Checkpoint("stored context item is missing".into()))?;
        if usize::try_from(row.get::<_, i64>(0)?).ok() != Some(index) {
            return Err(Error::Checkpoint("stored context item is missing".into()));
        }
        if cached.is_some_and(|cache| cache.items[index].as_ptr() == Arc::as_ptr(item)) {
            continue;
        }
        let json = row
            .get_ref(1)?
            .as_str()
            .map_err(|_| Error::Checkpoint("stored context item is not text".into()))?;
        let previous: Value = serde_json::from_str(json)?;
        if previous != **item {
            return Err(Error::Checkpoint(
                "context prefix changed without advancing its epoch".into(),
            ));
        }
    }
    Ok((count, false))
}

pub(super) fn append_context(
    transaction: &Transaction<'_>,
    checkpoint: &Checkpoint,
    start: usize,
) -> Result<()> {
    let epoch = sqlite_integer(checkpoint.context_epoch, "context epoch")?;
    let mut statement = transaction.prepare_cached(
        "INSERT INTO context_items (session_id, epoch, item_index, item_json) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (index, item) in checkpoint.context.iter().enumerate().skip(start) {
        let index = i64::try_from(index)
            .map_err(|_| Error::Checkpoint("context index exceeds SQLite INTEGER".into()))?;
        statement.execute(params![
            checkpoint.session_id,
            epoch,
            index,
            serde_json::to_string(item)?
        ])?;
    }
    Ok(())
}

pub(super) fn load_context(
    transaction: &Transaction<'_>,
    session_id: &str,
    epoch: i64,
    count: i64,
) -> Result<Arc<Vec<Arc<Value>>>> {
    let count = usize::try_from(count)
        .map_err(|_| Error::Checkpoint("stored context length is invalid".into()))?;
    let mut statement = transaction.prepare_cached(
        "SELECT epoch, item_index, item_json FROM context_items WHERE session_id = ?1 ORDER BY epoch, item_index",
    )?;
    let mut rows = statement.query([session_id])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        let row_epoch: i64 = row.get(0)?;
        let index: i64 = row.get(1)?;
        if row_epoch != epoch
            || usize::try_from(index).ok() != Some(items.len())
            || items.len() >= count
        {
            return Err(Error::Checkpoint(
                "stored context rows do not match their header".into(),
            ));
        }
        let json: String = row.get(2)?;
        items.push(Arc::new(serde_json::from_str(&json)?));
    }
    if items.len() != count {
        return Err(Error::Checkpoint(
            "stored context length does not match its header".into(),
        ));
    }
    Ok(Arc::new(items))
}
