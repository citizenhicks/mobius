use serde::Deserialize;
use serde_json::Value;

use super::{Basis, MAX_NOTE_BYTES, ScratchpadStore, WriteOutcome, publish_widgets, text};
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
    pub(super) store: ScratchpadStore,
    pub(super) frontend: FrontendEventSink,
}

impl Tool for WriteScratchpad {
    fn definition(&self) -> ToolDefinition {
        let mut tool = text::DEFINITION.write_scratchpad.tool.clone();
        tool.parameters["properties"]["note"]["maxLength"] = MAX_NOTE_BYTES.into();
        tool
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: WriteArgs = serde_json::from_value(arguments)?;
            let access = self.store.lock_access().await;
            let outcome = self
                .store
                .write_locked(&arguments.note, Basis::AgentObservation, &access)
                .await?;
            if outcome != WriteOutcome::Existing {
                let snapshot = self.store.snapshot_locked(&access).await?;
                publish_widgets(&self.frontend, &snapshot)?;
            }
            Ok(match outcome {
                WriteOutcome::Added => text::DEFINITION.message_added.as_str(),
                WriteOutcome::Updated => text::DEFINITION.message_updated.as_str(),
                WriteOutcome::Existing => text::DEFINITION.message_existing.as_str(),
            }
            .into())
        })
    }
}
