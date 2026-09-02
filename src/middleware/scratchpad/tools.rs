use serde::Deserialize;
use serde_json::Value;

use super::presentation::publish_current_widgets;
use super::{MAX_NOTE_BYTES, PromotionTarget, ScratchpadStore, SwarmScope, WriteOutcome, text};
use crate::backend::model::ToolDefinition;
use crate::middleware::FrontendEventSink;
use crate::middleware::tools::{ApprovalRequirement, Tool, ToolContext};
use crate::{BoxFuture, Result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoteArgs {
    note: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromoteArgs {
    note: String,
    target: PromotionTarget,
}

pub(super) struct WriteScratchpad {
    pub(super) store: ScratchpadStore,
    pub(super) swarm: SwarmScope,
    pub(super) session_id: String,
    pub(super) frontend: FrontendEventSink,
}

impl Tool for WriteScratchpad {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_scratchpad".into(),
            description: text::TOOL_WRITE_SCRATCHPAD_DESCRIPTION.into(),
            parameters: note_schema(text::TOOL_WRITE_SCRATCHPAD_PARAMETER_NOTE_DESCRIPTION),
        }
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: NoteArgs = serde_json::from_value(arguments)?;
            let swarm_id = self.swarm.resolve().await?;
            let outcome = self
                .store
                .write_session(&self.session_id, &arguments.note)
                .await?;
            if outcome != WriteOutcome::Existing {
                publish_current_widgets(
                    &self.store,
                    &self.session_id,
                    swarm_id.as_deref(),
                    &self.frontend,
                )
                .await?;
            }
            Ok(match outcome {
                WriteOutcome::Added => text::MESSAGE_ADDED_SESSION.into(),
                WriteOutcome::Updated => text::MESSAGE_UPDATED_SESSION.into(),
                WriteOutcome::Existing => text::MESSAGE_EXISTING_SESSION.into(),
            })
        })
    }
}

pub(super) struct PromoteScratchpad {
    pub(super) store: ScratchpadStore,
    pub(super) swarm: SwarmScope,
    pub(super) session_id: String,
    pub(super) frontend: FrontendEventSink,
}

impl Tool for PromoteScratchpad {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "promote_scratchpad".into(),
            description: text::TOOL_PROMOTE_SCRATCHPAD_DESCRIPTION.into(),
            parameters: promote_schema(),
        }
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: PromoteArgs = serde_json::from_value(arguments)?;
            let access = self.store.lock_access().await;
            let swarm_id = self.swarm.resolve().await?;
            let outcome = self
                .store
                .promote_note_locked(
                    &self.session_id,
                    swarm_id.as_deref(),
                    &arguments.note,
                    arguments.target,
                    &access,
                )
                .await?;
            drop(access);
            if outcome != WriteOutcome::Existing {
                publish_current_widgets(
                    &self.store,
                    &self.session_id,
                    swarm_id.as_deref(),
                    &self.frontend,
                )
                .await?;
            }
            Ok(match (arguments.target, outcome) {
                (PromotionTarget::Global, WriteOutcome::Added) => {
                    text::MESSAGE_PROMOTED_GLOBAL.into()
                }
                (PromotionTarget::Global, WriteOutcome::Updated) => {
                    text::MESSAGE_UPGRADED_GLOBAL.into()
                }
                (PromotionTarget::Global, WriteOutcome::Existing) => {
                    text::MESSAGE_EXISTING_GLOBAL.into()
                }
                (PromotionTarget::Swarm, WriteOutcome::Added) => {
                    text::MESSAGE_PROMOTED_SWARM.into()
                }
                (PromotionTarget::Swarm, WriteOutcome::Updated) => {
                    text::MESSAGE_UPGRADED_SWARM.into()
                }
                (PromotionTarget::Swarm, WriteOutcome::Existing) => {
                    text::MESSAGE_EXISTING_SWARM.into()
                }
            })
        })
    }
}

fn note_schema(description: &str) -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "note": {
                "type": "string",
                "description": description,
                "maxLength": MAX_NOTE_BYTES
            }
        },
        "required": ["note"],
        "additionalProperties": false
    })
}

fn promote_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "note": {
                "type": "string",
                "description": text::TOOL_PROMOTE_SCRATCHPAD_PARAMETER_NOTE_DESCRIPTION,
                "maxLength": MAX_NOTE_BYTES
            },
            "target": {
                "type": "string",
                "enum": ["global", "swarm"],
                "description": text::TOOL_PROMOTE_SCRATCHPAD_PARAMETER_TARGET_DESCRIPTION
            }
        },
        "required": ["note", "target"],
        "additionalProperties": false
    })
}
