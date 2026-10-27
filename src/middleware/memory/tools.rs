use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::{
    Access, Basis, MemoryOwner, MemoryStore, Reach, WriteOutcome, items, publish_widgets, text,
};
use crate::backend::model::ToolDefinition;
use crate::middleware::FrontendEventSink;
use crate::middleware::tools::{ApprovalRequirement, Tool, ToolContext};
use crate::{BoxFuture, Error, Result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    memory: String,
    #[serde(default)]
    global: bool,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgetArgs {
    id: String,
}

/// Saves or updates one memory for the chat's owner, or a global one.
pub(super) struct WriteMemory {
    pub(super) store: Arc<MemoryStore>,
    pub(super) owner: Arc<MemoryOwner>,
    pub(super) frontend: FrontendEventSink,
}

/// Forgets one memory the chat's owner can see.
pub(super) struct ForgetMemory {
    pub(super) store: Arc<MemoryStore>,
    pub(super) owner: Arc<MemoryOwner>,
    pub(super) frontend: FrontendEventSink,
}

async fn publish(
    store: &MemoryStore,
    frontend: &FrontendEventSink,
    _access: &Access<'_>,
) -> Result<()> {
    publish_widgets(frontend, &items(store.all().await?))
}

impl Tool for WriteMemory {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &text::DEFINITION.write_memory.tool
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
            let outcome = match (&arguments.id, arguments.global) {
                (Some(_), true) => {
                    return Err(Error::Tool(
                        text::DEFINITION.error_global_update.as_str().into(),
                    ));
                }
                (Some(id), false) => {
                    self.store
                        .edit_locked(
                            Reach::Visible(&self.owner),
                            id,
                            &arguments.memory,
                            Basis::AgentObservation,
                            &access,
                        )
                        .await?;
                    WriteOutcome::Updated
                }
                (None, global) => {
                    self.store
                        .write_locked(
                            (!global).then_some(&self.owner),
                            Some(&self.owner),
                            &arguments.memory,
                            Basis::AgentObservation,
                            &access,
                        )
                        .await?
                }
            };
            if outcome != WriteOutcome::Existing {
                publish(&self.store, &self.frontend, &access).await?;
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

impl Tool for ForgetMemory {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &text::DEFINITION.forget_memory.tool
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
            let arguments: ForgetArgs = serde_json::from_value(arguments)?;
            let access = self.store.access.lock().await;
            self.store
                .forget_locked(Reach::Visible(&self.owner), &arguments.id, &access)
                .await?;
            publish(&self.store, &self.frontend, &access).await?;
            drop(access);
            Ok(text::DEFINITION.message_forgot.as_str().into())
        })
    }
}
