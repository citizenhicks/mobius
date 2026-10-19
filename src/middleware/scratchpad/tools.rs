use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::{Basis, ScratchpadStore, WriteOutcome, publish_widgets, text};
use crate::backend::model::ToolDefinition;
use crate::middleware::FrontendEventSink;
use crate::middleware::tools::{ApprovalRequirement, Tool, ToolContext};
use crate::{BoxFuture, Result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    note: String,
}

pub(super) struct WriteScratchpad {
    pub(super) store: Arc<ScratchpadStore>,
    pub(super) frontend: FrontendEventSink,
}

impl Tool for WriteScratchpad {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &text::DEFINITION.write_scratchpad.tool
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
            let arguments: WriteArgs = serde_json::from_value(arguments)?;
            let access = self.store.access.lock().await;
            let outcome = self
                .store
                .write_locked(&arguments.note, Basis::AgentObservation, &access)
                .await?;
            if outcome != WriteOutcome::Existing {
                let snapshot = self.store.snapshot_locked(&access).await?;
                publish_widgets(&self.frontend, &snapshot)?;
            }
            drop(access);
            Ok(match outcome {
                WriteOutcome::Added => text::DEFINITION.message_added.as_str(),
                WriteOutcome::Updated => text::DEFINITION.message_updated.as_str(),
                WriteOutcome::Existing => text::DEFINITION.message_existing.as_str(),
            }
            .into())
        })
    }
}
