//! Conversational recurring-task setup.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::manifest::MiddlewareManifest;
use super::tools::{
    ApprovalRequirement, Catalog, Tool, ToolContext, labeled_tool_heading, render_tool_event,
};
use super::{Middleware, PromptSection, RuntimeContext};
use crate::backend::model::ToolDefinition;
use crate::protocol::{EventMsg, FrontendBlock, FrontendCommand, FrontendContribution};
use crate::{BoxFuture, Result};

mod text {
    include!(concat!(env!("OUT_DIR"), "/src_middleware_cron_text.rs"));
}

/// Configuration and presentation metadata for scheduled work.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "cron",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: false,
    default_enabled: true,
    settings: &[],
};

type TaskWriter = dyn Fn(&str, &str, &str) -> Result<String> + Send + Sync;

/// Lets the model turn a confirmed conversation into a recurring task.
pub struct Cron {
    write: Arc<TaskWriter>,
}

impl Cron {
    /// Creates recurring-task middleware backed by the host's task writer.
    pub fn new(write: impl Fn(&str, &str, &str) -> Result<String> + Send + Sync + 'static) -> Self {
        Self {
            write: Arc::new(write),
        }
    }

    fn section(&self) -> PromptSection {
        PromptSection::new(text::PROMPT_MAIN)
    }
}

impl Middleware for Cron {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        catalog.register(Arc::new(ScheduleTask {
            write: Arc::clone(&self.write),
            source_session_id: runtime.session_id.clone(),
        }))
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(Some(self.section()))
    }

    fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: self.name().into(),
            commands: vec![FrontendCommand {
                name: self.name().into(),
                arguments: text::COMMAND_ARGUMENTS.into(),
                description: text::COMMAND_DESCRIPTION.into(),
                requires_idle: false,
            }],
            ..FrontendContribution::default()
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        render_tool_event(
            event,
            |name| name == "schedule_task",
            |_, arguments| labeled_tool_heading(text::RENDER_SCHEDULE, "schedule", arguments),
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleTaskArgs {
    task: String,
    schedule: String,
}

struct ScheduleTask {
    write: Arc<TaskWriter>,
    source_session_id: String,
}

impl Tool for ScheduleTask {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "schedule_task".into(),
            description: text::TOOL_SCHEDULE_TASK_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": text::TOOL_SCHEDULE_TASK_PARAMETER_TASK_DESCRIPTION
                    },
                    "schedule": {
                        "type": "string",
                        "description": text::TOOL_SCHEDULE_TASK_PARAMETER_SCHEDULE_DESCRIPTION
                    }
                },
                "required": ["task", "schedule"],
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
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: ScheduleTaskArgs = serde_json::from_value(arguments)?;
            let id = (self.write)(
                &self.source_session_id,
                &arguments.task,
                &arguments.schedule,
            )?;
            Ok(format!("scheduled `{}` as task {id}", arguments.schedule))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::backend::sandbox::local::LocalSandbox;
    use crate::backend::sandbox::{ApprovalPolicy, NetworkAccess, Sandbox, SandboxPermissions};

    #[test]
    fn scheduling_contributes_its_toml_backed_prompt() {
        let middleware = Cron::new(|_, _, _| Ok("task".into()));

        assert_eq!(middleware.section(), PromptSection::new(text::PROMPT_MAIN));
    }

    #[test]
    fn schedule_task_events_render_as_cron_blocks() {
        let middleware = Cron::new(|_, _, _| Ok("task".into()));

        for event in [
            EventMsg::ToolCallBegin(crate::protocol::ToolCallBeginEvent {
                turn_id: "turn".into(),
                call_id: "call".into(),
                name: "schedule_task".into(),
                arguments: serde_json::json!({"schedule": "0 9 * * *"}),
            }),
            EventMsg::ToolCallEnd(crate::protocol::ToolCallEndEvent {
                turn_id: "turn".into(),
                call_id: "call".into(),
                name: "schedule_task".into(),
                output: String::new(),
                is_error: false,
            }),
        ] {
            assert!(middleware.render(&event, "session").is_some());
        }
    }

    #[tokio::test]
    async fn schedule_task_uses_the_injected_writer_and_requires_approval() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let tool = ScheduleTask {
            write: Arc::new(move |session, task, schedule| {
                recorded.lock().expect("calls").push((
                    session.to_string(),
                    task.to_string(),
                    schedule.to_string(),
                ));
                Ok("task-id".into())
            }),
            source_session_id: "session-a".into(),
        };
        let workspace = tempfile::tempdir().expect("workspace");
        let sandbox = Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        ));
        let permissions = SandboxPermissions::restore(
            "session-a",
            crate::backend::sandbox::SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            ["call".into()],
        )
        .for_call("call");

        let output = tool
            .call(
                ToolContext {
                    sandbox,
                    permissions,
                    turn_id: "turn".into(),
                },
                serde_json::json!({
                    "task": "Review open pull requests",
                    "schedule": "0 9 * * 1"
                }),
            )
            .await
            .expect("schedule task");

        assert_eq!(tool.definition().name, "schedule_task");
        assert_eq!(tool.approval(), ApprovalRequirement::Always);
        assert_eq!(output, "scheduled `0 9 * * 1` as task task-id");
        assert_eq!(
            *calls.lock().expect("calls"),
            [(
                "session-a".into(),
                "Review open pull requests".into(),
                "0 9 * * 1".into()
            )]
        );
    }
}
