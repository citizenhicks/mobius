//! Gateway-owned routine creation contributed to an execution's tool catalog.

use std::path::PathBuf;
use std::sync::Arc;

use mobius::agent::AgentRole;
use mobius::backend::model::ToolDefinition;
use mobius::middleware::tools::{
    ApprovalRequirement, Catalog, Tool, ToolContext, render_tool_event,
};
use mobius::middleware::{Middleware, PromptSection, RuntimeContext};
use mobius::protocol::{EventMsg, FrontendBlock, ToolResponse};
use mobius::{BoxFuture, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::bots::BotStore;
use crate::wire::RoutineSchedule;

mod text {
    pub const PROMPT_ROUTINE: &str = "Use `create_routine` to schedule your own work in the current workspace. Ask the user when a requested time zone is ambiguous.";
    pub const TOOL_CREATE_ROUTINE_DESCRIPTION: &str =
        "Create an enabled routine for this Bot in this chat's workspace. Approval is required.";
    pub const TOOL_CREATE_ROUTINE_PARAMETER_ENDS_AT_DESCRIPTION: &str =
        "Optional positive Unix timestamp in seconds after which no run may start.";
    pub const TOOL_CREATE_ROUTINE_PARAMETER_INSTRUCTIONS_DESCRIPTION: &str =
        "Complete instructions to execute on every run.";
    pub const TOOL_CREATE_ROUTINE_PARAMETER_SCHEDULE_DESCRIPTION: &str = "Use only fields matching the kind: once requires at; interval requires every_seconds; cron requires expression and time_zone.";
}

#[derive(Clone)]
pub(crate) struct Routines {
    bots: Arc<BotStore>,
    bot_id: String,
    workspace: PathBuf,
}

impl Routines {
    pub(crate) fn new(bots: Arc<BotStore>, bot_id: String, workspace: PathBuf) -> Self {
        Self {
            bots,
            bot_id,
            workspace,
        }
    }
}

impl Middleware for Routines {
    fn name(&self) -> &'static str {
        "routines"
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        if matches!(runtime.role, AgentRole::Main) {
            catalog.register(Arc::new(CreateRoutine(self.clone())))?;
        }
        Ok(())
    }

    fn prompt_section(&self, runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(matches!(runtime.role, AgentRole::Main)
            .then(|| PromptSection::new(text::PROMPT_ROUTINE)))
    }
    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        render_tool_event(
            event,
            |name| name == "create_routine",
            |name, arguments| mobius::middleware::tools::ToolHeading {
                title: if arguments.is_null() {
                    name
                } else {
                    "Create routine"
                }
                .into(),
                detail: arguments
                    .get("instructions")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            },
        )
    }
}

struct CreateRoutine(Routines);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRoutineArgs {
    instructions: String,
    schedule: RoutineSchedule,
    ends_at: Option<i64>,
}

impl Tool for CreateRoutine {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "create_routine".into(),
            description: text::TOOL_CREATE_ROUTINE_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "instructions": {
                        "type": "string",
                        "description": text::TOOL_CREATE_ROUTINE_PARAMETER_INSTRUCTIONS_DESCRIPTION
                    },
                    "schedule": {
                        "description": text::TOOL_CREATE_ROUTINE_PARAMETER_SCHEDULE_DESCRIPTION,
                        "oneOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "kind": {"const": "once"},
                                    "at": {"type": "integer", "description": "Unix timestamp in seconds."}
                                },
                                "required": ["kind", "at"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "kind": {"const": "interval"},
                                    "every_seconds": {"type": "integer", "minimum": 60, "description": "Cadence in seconds."}
                                },
                                "required": ["kind", "every_seconds"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "kind": {"const": "cron"},
                                    "expression": {"type": "string", "description": "Five-field cron expression."},
                                    "time_zone": {"type": "string", "description": "IANA time zone."}
                                },
                                "required": ["kind", "expression", "time_zone"],
                                "additionalProperties": false
                            }
                        ]
                    },
                    "ends_at": {
                        "type": "integer",
                        "minimum": 1,
                        "description": text::TOOL_CREATE_ROUTINE_PARAMETER_ENDS_AT_DESCRIPTION
                    }
                },
                "required": ["instructions", "schedule"],
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
    ) -> BoxFuture<'a, Result<ToolResponse>> {
        Box::pin(async move {
            let arguments: CreateRoutineArgs = serde_json::from_value(arguments)?;
            let routine = self
                .0
                .bots
                .create_routine(
                    &self.0.bot_id,
                    &self.0.workspace,
                    &arguments.instructions,
                    arguments.schedule,
                    arguments.ends_at,
                )
                .map_err(|error| mobius::Error::Tool(error.to_string()))?;
            Ok(serde_json::to_string(&routine)?.into())
        })
    }
}

#[cfg(test)]
mod tests;
