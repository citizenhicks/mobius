use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::{
    Basis, Change, MAX_OPS, MemoryOwner, MemoryStore, Op, Writer, items, pages, publish_widgets,
    text,
};
use crate::backend::model::ToolDefinition;
use crate::middleware::FrontendEventSink;
use crate::middleware::tools::{ApprovalRequirement, Tool, ToolContext};
use crate::{BoxFuture, Error, Result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchArgs {
    ops: Vec<OpArgs>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpArgs {
    page: String,
    #[serde(default)]
    global: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    delete: bool,
    #[serde(default)]
    base_updated_at: Option<u64>,
}

/// Validates `patch_memory` arguments; one bad op rejects the whole call before any write.
pub(super) fn parse_ops(arguments: Value) -> Result<Vec<Op>> {
    let PatchArgs { ops } = serde_json::from_value(arguments)?;
    if ops.is_empty() || ops.len() > MAX_OPS {
        return Err(Error::Tool(format!("ops holds 1–{MAX_OPS} changes")));
    }
    ops.into_iter()
        .map(|op| {
            let page = pages::validate_name(&op.page).map_err(Error::Tool)?;
            let change = match (op.delete, op.description, op.body) {
                (true, None, None) => Change::Delete,
                (false, Some(description), Some(body)) => Change::Set {
                    description: pages::validate_description(&description).map_err(Error::Tool)?,
                    body: pages::canonical_body(&body).map_err(Error::Tool)?,
                },
                _ => {
                    return Err(Error::Tool(format!(
                        "{page}: an op either sets description and body, or has delete: true alone"
                    )));
                }
            };
            Ok(Op {
                page,
                global: op.global,
                change,
                base_updated_at: op.base_updated_at,
            })
        })
        .collect()
}

/// Creates, rewrites and deletes the chat owner's pages, or global ones.
pub(super) struct PatchMemory {
    pub(super) store: Arc<MemoryStore>,
    pub(super) owner: Arc<MemoryOwner>,
    pub(super) frontend: FrontendEventSink,
}

impl Tool for PatchMemory {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &text::DEFINITION.patch_memory.tool
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call(
        &self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let ops = parse_ops(arguments)?;
            let access = self.store.access.lock().await;
            let results = self
                .store
                .apply_locked(
                    &self.owner,
                    ops,
                    Basis::AgentObservation,
                    Writer::Chat,
                    &access,
                )
                .await?;
            publish_widgets(&self.frontend, &items(&self.store.pages.all().await?)?)?;
            drop(access);
            Ok(format!(
                "{}\n{}",
                text::DEFINITION.message_patched,
                results.join("\n")
            )
            .into())
        })
    }
}
