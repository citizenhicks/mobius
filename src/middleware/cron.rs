//! Conversational recurring-task setup.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::manifest::MiddlewareManifest;
use super::tools::{
    ApprovalRequirement, Catalog, Tool, ToolContext, labeled_tool_heading, render_tool_event,
};
use super::{
    ActiveCommandContext, ActiveSubmissionResult, Middleware, MiddlewareCommandContext,
    MiddlewareCommandOutput, PromptSection, RuntimeContext,
};
use crate::backend::model::ToolDefinition;
use crate::protocol::{
    EventMsg, FrontendBlock, FrontendCommand, FrontendContribution, MessageTarget,
};
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
/// Host operations used by the capability-owned command grammar.
pub type CronCommandHandler =
    Arc<dyn Fn(String, CronCommand) -> BoxFuture<'static, Result<CronCommandResult>> + Send + Sync>;

/// One normalized scheduling command sent to the durable host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronCommand {
    List,
    New { task: Option<String> },
    Reschedule { id: String, schedule: String },
    Delete { id: String },
    Run { id: String },
    History { id: Option<String> },
}

/// Frontend-safe scheduled task metadata returned by the durable host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronTask {
    pub id: String,
    pub schedule: String,
    pub task: String,
}

/// Frontend-safe scheduled-run metadata returned by the durable host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronRun {
    pub id: String,
    pub task_id: String,
    pub status: String,
    pub started_at: i64,
}

/// Result of one durable host scheduling operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronCommandResult {
    None,
    Tasks(Vec<CronTask>),
    History(Vec<CronRun>),
}

/// Lets the model turn a confirmed conversation into a recurring task.
pub struct Cron {
    write: Arc<TaskWriter>,
    command: CronCommandHandler,
}

impl Cron {
    /// Creates recurring-task middleware backed by the host's task writer.
    pub fn new(
        write: impl Fn(&str, &str, &str) -> Result<String> + Send + Sync + 'static,
        command: CronCommandHandler,
    ) -> Self {
        Self {
            write: Arc::new(write),
            command,
        }
    }

    fn section(&self) -> PromptSection {
        PromptSection::new(text::PROMPT_MAIN)
    }

    async fn execute_command(
        &self,
        session_id: &str,
        arguments: &str,
        input: Option<&str>,
        target: Option<MessageTarget>,
    ) -> Result<MiddlewareCommandOutput> {
        if input.is_some() || target.is_some() {
            return Err(crate::Error::Config(
                "cron commands do not accept input or a message target".into(),
            ));
        }
        let command = parse_command(arguments)?;
        let result = (self.command)(session_id.into(), command).await?;
        Ok(render_command_result(result))
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

    fn command<'a>(
        &'a self,
        context: MiddlewareCommandContext<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareCommandOutput>> {
        Box::pin(self.execute_command(
            context.session_id,
            context.arguments,
            context.input,
            context.target,
        ))
    }

    fn active_command<'a>(
        &'a self,
        context: &'a mut ActiveCommandContext<'_>,
    ) -> BoxFuture<'a, Result<Option<ActiveSubmissionResult>>> {
        Box::pin(async move {
            match self
                .execute_command(
                    context.session_id,
                    context.arguments,
                    context.input,
                    context.target,
                )
                .await
            {
                Ok(output) => {
                    context
                        .events
                        .extend(output.events.into_iter().map(EventMsg::Frontend));
                    Ok(Some(ActiveSubmissionResult::Handled))
                }
                Err(error) => Ok(Some(ActiveSubmissionResult::Rejected(error.to_string()))),
            }
        })
    }
}

fn parse_command(arguments: &str) -> Result<CronCommand> {
    let arguments = arguments.trim();
    if arguments.is_empty() || arguments == "list" {
        return Ok(CronCommand::List);
    }
    let mut parts = arguments.split_ascii_whitespace();
    match parts.next() {
        Some("new") => {
            let task = parts.collect::<Vec<_>>().join(" ");
            Ok(CronCommand::New {
                task: (!task.is_empty()).then_some(task),
            })
        }
        Some("reschedule") => {
            let id = required_command_part(
                parts.next().unwrap_or_default(),
                "usage: /cron reschedule <id> <schedule>",
            )?;
            let schedule = parts.collect::<Vec<_>>().join(" ");
            required_command_part(&schedule, "usage: /cron reschedule <id> <schedule>")?;
            Ok(CronCommand::Reschedule {
                id: id.into(),
                schedule,
            })
        }
        Some("delete") => one_id_command(parts, |id| CronCommand::Delete { id }),
        Some("run") => one_id_command(parts, |id| CronCommand::Run { id }),
        Some("history") => {
            let id = parts.next().map(str::to_owned);
            if parts.next().is_some() {
                return Err(crate::Error::Config("usage: /cron history [id]".into()));
            }
            Ok(CronCommand::History { id })
        }
        _ => Err(crate::Error::Config(
            "usage: /cron [new [task]|list|reschedule <id> <schedule>|delete <id>|run <id>|history [id]]".into(),
        )),
    }
}

fn one_id_command<'a>(
    mut parts: impl Iterator<Item = &'a str>,
    build: impl FnOnce(String) -> CronCommand,
) -> Result<CronCommand> {
    let id = required_command_part(parts.next().unwrap_or_default(), "cron task ID is required")?;
    if parts.next().is_some() {
        return Err(crate::Error::Config(
            "cron command accepts one task ID".into(),
        ));
    }
    Ok(build(id.into()))
}

fn required_command_part<'a>(value: &'a str, usage: &str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        Err(crate::Error::Config(usage.into()))
    } else {
        Ok(value)
    }
}

fn render_command_result(result: CronCommandResult) -> MiddlewareCommandOutput {
    let text = match result {
        CronCommandResult::None => return MiddlewareCommandOutput::events(Vec::new()),
        CronCommandResult::Tasks(tasks) if tasks.is_empty() => "no scheduled tasks".into(),
        CronCommandResult::Tasks(tasks) => tasks
            .into_iter()
            .map(|task| format!("{}  {}\n  task: {}", task.id, task.schedule, task.task))
            .collect::<Vec<_>>()
            .join("\n"),
        CronCommandResult::History(runs) if runs.is_empty() => "no cron runs".into(),
        CronCommandResult::History(runs) => runs
            .into_iter()
            .map(|run| {
                format!(
                    "{} · {} · {} · started {}",
                    run.id, run.task_id, run.status, run.started_at
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };
    MiddlewareCommandOutput::render(MANIFEST.id, text, crate::protocol::FrontendTone::Neutral)
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

    fn command_handler() -> CronCommandHandler {
        Arc::new(|_, _| Box::pin(async { Ok(CronCommandResult::None) }))
    }

    #[test]
    fn scheduling_contributes_its_toml_backed_prompt() {
        let middleware = Cron::new(|_, _, _| Ok("task".into()), command_handler());

        assert_eq!(middleware.section(), PromptSection::new(text::PROMPT_MAIN));
    }

    #[test]
    fn schedule_task_events_render_as_cron_blocks() {
        let middleware = Cron::new(|_, _, _| Ok("task".into()), command_handler());

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

    #[test]
    fn command_grammar_is_owned_by_cron() {
        assert_eq!(parse_command("").expect("list"), CronCommand::List);
        assert_eq!(
            parse_command("new review pull requests").expect("new"),
            CronCommand::New {
                task: Some("review pull requests".into())
            }
        );
        assert_eq!(
            parse_command("reschedule abc 0 9 * * 1").expect("reschedule"),
            CronCommand::Reschedule {
                id: "abc".into(),
                schedule: "0 9 * * 1".into()
            }
        );
        assert!(parse_command("delete").is_err());
        assert!(parse_command("history one two").is_err());
    }

    #[test]
    fn command_results_use_capability_owned_presentation() {
        let output = render_command_result(CronCommandResult::Tasks(vec![CronTask {
            id: "task-a".into(),
            schedule: "0 9 * * 1".into(),
            task: "/private/task.md".into(),
        }]));
        let [crate::protocol::FrontendEvent::Render { capability, block }] =
            output.events.as_slice()
        else {
            panic!("expected one rendered cron block");
        };
        assert_eq!(capability, MANIFEST.id);
        assert_eq!(block.title, "task-a  0 9 * * 1\n  task: /private/task.md");
    }

    #[tokio::test]
    async fn command_routes_the_session_and_renders_the_host_result() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let middleware = Cron::new(
            |_, _, _| Ok("task".into()),
            Arc::new(move |session_id, command| {
                recorded
                    .lock()
                    .expect("commands")
                    .push((session_id, command));
                Box::pin(async {
                    Ok(CronCommandResult::Tasks(vec![CronTask {
                        id: "task-a".into(),
                        schedule: "0 9 * * 1".into(),
                        task: "review pull requests".into(),
                    }]))
                })
            }),
        );
        let checkpoint = crate::backend::checkpoint::Checkpoint::empty("session-a");
        let session_context = crate::protocol::SessionContext::default();
        let directory = tempfile::tempdir().expect("checkpoint directory");
        let checkpoints: Arc<dyn crate::backend::checkpoint::CheckpointStore> = Arc::new(
            crate::backend::checkpoint::sqlite::SqliteCheckpoint::new(
                directory.path().join("checkpoints.sqlite3"),
            )
            .expect("checkpoint store"),
        );

        let output = middleware
            .command(MiddlewareCommandContext {
                command: MANIFEST.id,
                arguments: "list",
                input: None,
                target: None,
                session_id: "session-a",
                session_context: &session_context,
                checkpoint: &checkpoint,
                checkpoints,
            })
            .await
            .expect("cron command");

        assert_eq!(
            *calls.lock().expect("commands"),
            [("session-a".into(), CronCommand::List)]
        );
        assert!(matches!(
            output.events.as_slice(),
            [crate::protocol::FrontendEvent::Render { capability, block }]
                if capability == MANIFEST.id && block.title.contains("task-a")
        ));

        let mut queued = Vec::new();
        let mut events = Vec::new();
        let active = middleware
            .active_command(&mut ActiveCommandContext {
                submission_id: "command-a",
                session_id: "session-a",
                metadata: &checkpoint.metadata,
                active_turn_id: "turn-a",
                command: MANIFEST.id,
                arguments: "history",
                input: None,
                target: None,
                queued_input: crate::middleware::QueuedInputQueue::new(
                    &mut queued,
                    crate::middleware::QueuedInputBaseline::default(),
                ),
                events: &mut events,
            })
            .await
            .expect("active cron command");

        assert_eq!(active, Some(ActiveSubmissionResult::Handled));
        assert!(matches!(events.as_slice(), [EventMsg::Frontend(_)]));
        assert_eq!(
            *calls.lock().expect("commands"),
            [
                ("session-a".into(), CronCommand::List),
                ("session-a".into(), CronCommand::History { id: None })
            ]
        );
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
