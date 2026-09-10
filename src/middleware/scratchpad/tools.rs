use serde::Deserialize;
use serde_json::Value;

use super::{
    Basis, MAX_NOTE_BYTES, Scope, ScratchpadStore, SwarmScope, WriteOutcome, publish_widgets, text,
};
use crate::backend::model::ToolDefinition;
use crate::middleware::FrontendEventSink;
use crate::middleware::tools::{ApprovalRequirement, Tool, ToolContext};
use crate::{BoxFuture, Result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    scope: Scope,
    note: String,
}

pub(super) struct WriteScratchpad {
    pub(super) store: ScratchpadStore,
    pub(super) swarm: SwarmScope,
    pub(super) frontend: FrontendEventSink,
}

impl Tool for WriteScratchpad {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_scratchpad".into(),
            description: text::TOOL_WRITE_SCRATCHPAD_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "scope": {
                        "type": "string",
                        "enum": ["swarm", "global"],
                        "description": text::TOOL_WRITE_SCRATCHPAD_PARAMETER_SCOPE_DESCRIPTION
                    },
                    "note": {
                        "type": "string",
                        "description": text::TOOL_WRITE_SCRATCHPAD_PARAMETER_NOTE_DESCRIPTION,
                        "maxLength": MAX_NOTE_BYTES
                    }
                },
                "required": ["scope", "note"],
                "additionalProperties": false
            }),
        }
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
            let swarm_id = self.swarm.resolve().await?;
            let outcome = self
                .store
                .write_locked(
                    swarm_id.as_deref(),
                    arguments.scope,
                    &arguments.note,
                    Basis::AgentObservation,
                    &access,
                )
                .await?;
            if outcome != WriteOutcome::Existing {
                let snapshot = self
                    .store
                    .snapshot_locked(swarm_id.as_deref(), &access)
                    .await?;
                publish_widgets(&self.frontend, &snapshot)?;
            }
            Ok(match outcome {
                WriteOutcome::Added => text::MESSAGE_ADDED,
                WriteOutcome::Updated => text::MESSAGE_UPDATED,
                WriteOutcome::Existing => text::MESSAGE_EXISTING,
            }
            .into())
        })
    }
}
