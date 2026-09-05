use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use super::runtime::{AgentPresentation, Followup, MAX_MESSAGE_BYTES, Shared, monitor_agent};
use super::{
    AgentScope, DEFAULT_WAIT_MS, ForkTurns, MAX_TASK_NAME_BYTES, MAX_WAIT_MS, MIN_WAIT_MS, text,
};
use crate::backend::model::ToolDefinition;
use crate::middleware::attachments::strip_attachment_references;
use crate::middleware::tools::{HookIdentity, Tool, ToolContext};
use crate::protocol::{MessageAuthor, MessageSubmission, Op, is_internal_message};
use crate::{BoxFuture, Error, Result};

pub(super) struct SpawnAgent {
    pub(super) default_model: Option<String>,
    pub(super) default_reasoning: Option<String>,
    pub(super) shared: Arc<Shared>,
    pub(super) scope: Arc<AgentScope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    task_name: String,
    text: String,
    fork_turns: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
}

impl Tool for SpawnAgent {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "spawn_agent".into(),
            description: text::TOOL_SPAWN_AGENT_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_name": {
                        "type": "string",
                        "description": text::TOOL_SPAWN_AGENT_PARAMETER_TASK_NAME_DESCRIPTION
                    },
                    "text": {"type": "string"},
                    "fork_turns": {
                        "type": "string",
                        "description": text::TOOL_SPAWN_AGENT_PARAMETER_FORK_TURNS_DESCRIPTION
                    },
                    "model": {
                        "type": "string",
                        "description": text::TOOL_SPAWN_AGENT_PARAMETER_MODEL_DESCRIPTION
                    },
                    "reasoning_effort": {
                        "type": "string",
                        "description": text::TOOL_SPAWN_AGENT_PARAMETER_REASONING_EFFORT_DESCRIPTION
                    }
                },
                "required": ["task_name", "text"],
                "additionalProperties": false
            }),
        }
    }

    fn hook_identity(&self) -> Option<HookIdentity> {
        Some(HookIdentity {
            name: "spawn_agent",
            subjects: &["spawn_agent", "Agent"],
        })
    }

    fn call<'a>(&'a self, context: ToolContext, arguments: Value) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: SpawnArgs = serde_json::from_value(arguments)?;
            validate_task_name(&arguments.task_name)?;
            let text = validate_text(arguments.text)?;
            let turns = parse_fork_turns(arguments.fork_turns.as_deref())?;
            let model = arguments
                .model
                .or_else(|| self.default_model.clone())
                .unwrap_or_else(|| self.scope.model.clone());
            let reasoning_effort = arguments
                .reasoning_effort
                .or_else(|| self.default_reasoning.clone());
            let path = format!(
                "{}/{}",
                self.scope.agent_path.trim_end_matches('/'),
                arguments.task_name
            );
            let session_id = Uuid::new_v4().to_string();
            let shared = Arc::clone(&self.shared);
            let scope = Arc::clone(&self.scope);
            let submission = peer_submission(&scope.session_id, &scope.agent_path, text);
            supervise(async move {
                shared
                    .reserve(
                        &scope.root_session_id,
                        &path,
                        &scope.agent_path,
                        session_id.clone(),
                        scope.depth + 1,
                        AgentPresentation {
                            model: model.clone(),
                            spawn_context: turns.label(),
                        },
                    )
                    .await?;
                let agent = match scope
                    .fork(
                        session_id,
                        path.clone(),
                        model,
                        reasoning_effort,
                        turns,
                        context.turn_id,
                    )
                    .await
                {
                    Ok(agent) => agent,
                    Err(error) => {
                        return Err(cleanup_error(
                            error,
                            shared.remove(&scope.root_session_id, &path).await,
                        ));
                    }
                };
                let model = agent.model_route().to_string();
                let (sender, events) = agent.into_parts();
                if let Err(error) = shared
                    .attach(&scope.root_session_id, &path, sender.clone(), Some(model))
                    .await
                    .and_then(|()| {
                        sender
                            .submit(Op::Message {
                                message: submission,
                            })
                            .map(|_| ())
                    })
                {
                    return Err(cleanup_error(
                        error,
                        shared.remove(&scope.root_session_id, &path).await,
                    ));
                }
                tokio::spawn(monitor_agent(
                    Arc::clone(&shared),
                    scope.root_session_id.clone(),
                    path.clone(),
                    events,
                ));
                Ok(serde_json::json!({"task_name": path}).to_string())
            })
            .await
        })
    }
}

pub(super) struct SendMessage {
    pub(super) shared: Arc<Shared>,
    pub(super) scope: Arc<AgentScope>,
}

pub(super) struct FollowupTask {
    pub(super) shared: Arc<Shared>,
    pub(super) scope: Arc<AgentScope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageArgs {
    target: String,
    text: String,
}

impl Tool for SendMessage {
    fn definition(&self) -> ToolDefinition {
        message_definition("send_message", text::TOOL_SEND_MESSAGE_DESCRIPTION)
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: MessageArgs = serde_json::from_value(arguments)?;
            let message = peer_submission(
                &self.scope.session_id,
                &self.scope.agent_path,
                validate_text(arguments.text)?,
            );
            self.shared
                .submit_message(
                    &self.scope.root_session_id,
                    &self.scope.agent_path,
                    &arguments.target,
                    message,
                )
                .await?;
            Ok(String::new())
        })
    }
}

impl Tool for FollowupTask {
    fn definition(&self) -> ToolDefinition {
        message_definition("followup_task", text::TOOL_FOLLOWUP_TASK_DESCRIPTION)
    }

    fn call<'a>(&'a self, context: ToolContext, arguments: Value) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: MessageArgs = serde_json::from_value(arguments)?;
            let message = peer_submission(
                &self.scope.session_id,
                &self.scope.agent_path,
                validate_text(arguments.text)?,
            );
            let shared = Arc::clone(&self.shared);
            let scope = Arc::clone(&self.scope);
            supervise(async move {
                let followup = shared
                    .prepare_followup(&scope.root_session_id, &scope.agent_path, &arguments.target)
                    .await?;
                let Followup {
                    record,
                    sender,
                    previous,
                } = followup;
                let (sender, events, model) = match sender {
                    Some(sender) => (sender, None, None),
                    None => {
                        let agent = match scope
                            .resume(
                                record.session_id,
                                arguments.target.clone(),
                                record.depth,
                                record.model,
                                context.turn_id,
                            )
                            .await
                        {
                            Ok(agent) => agent,
                            Err(error) => {
                                return Err(cleanup_error(
                                    error,
                                    shared
                                        .rollback(
                                            &scope.root_session_id,
                                            &arguments.target,
                                            previous.clone(),
                                        )
                                        .await,
                                ));
                            }
                        };
                        let model = agent.model_route().to_string();
                        let (sender, events) = agent.into_parts();
                        (sender, Some(events), Some(model))
                    }
                };
                if let Err(error) = shared
                    .attach(
                        &scope.root_session_id,
                        &arguments.target,
                        sender.clone(),
                        model,
                    )
                    .await
                    .and_then(|()| sender.submit(Op::Message { message }).map(|_| ()))
                {
                    return Err(cleanup_error(
                        error,
                        shared
                            .rollback(&scope.root_session_id, &arguments.target, previous)
                            .await,
                    ));
                }
                if let Some(events) = events {
                    tokio::spawn(monitor_agent(
                        Arc::clone(&shared),
                        scope.root_session_id.clone(),
                        arguments.target,
                        events,
                    ));
                }
                Ok(String::new())
            })
            .await
        })
    }
}

fn message_definition(name: &str, description: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "target": {"type": "string"},
                "text": {"type": "string"}
            },
            "required": ["target", "text"],
            "additionalProperties": false
        }),
    }
}

pub(super) struct ListAgents {
    pub(super) shared: Arc<Shared>,
    pub(super) scope: Arc<AgentScope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    path_prefix: Option<String>,
}

impl Tool for ListAgents {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_agents".into(),
            description: text::TOOL_LIST_AGENTS_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path_prefix": {"type": "string"}},
                "additionalProperties": false
            }),
        }
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: ListArgs = serde_json::from_value(arguments)?;
            let agents = self
                .shared
                .list(
                    &self.scope.root_session_id,
                    arguments.path_prefix.as_deref(),
                )
                .await?;
            Ok(serde_json::json!({"agents": agents}).to_string())
        })
    }
}

pub(super) struct InterruptAgent {
    pub(super) shared: Arc<Shared>,
    pub(super) scope: Arc<AgentScope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetArgs {
    target: String,
}

impl Tool for InterruptAgent {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "interrupt_agent".into(),
            description: text::TOOL_INTERRUPT_AGENT_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"target": {"type": "string"}},
                "required": ["target"],
                "additionalProperties": false
            }),
        }
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: TargetArgs = serde_json::from_value(arguments)?;
            let previous_status = self
                .shared
                .interrupt(&self.scope.root_session_id, &arguments.target)
                .await?;
            Ok(serde_json::json!({"previous_status": previous_status}).to_string())
        })
    }
}

pub(super) struct WaitAgent {
    pub(super) shared: Arc<Shared>,
    pub(super) scope: Arc<AgentScope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    timeout_ms: Option<u64>,
}

impl Tool for WaitAgent {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "wait_agent".into(),
            description: text::TOOL_WAIT_AGENT_DESCRIPTION.into(),
            parameters: wait_parameters(),
        }
    }

    fn cancel_on_input(&self) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let arguments: WaitArgs = serde_json::from_value(arguments)?;
            let timeout = wait_timeout(arguments.timeout_ms)?;
            let agents = self
                .shared
                .wait(&self.scope.root_session_id, &self.scope.agent_path, timeout)
                .await?;
            Ok(serde_json::json!({
                "updated": !agents.is_empty(),
                "agents": agents
            })
            .to_string())
        })
    }
}

pub(super) fn wait_parameters() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "timeout_ms": {
                "type": "integer",
                "minimum": MIN_WAIT_MS,
                "maximum": MAX_WAIT_MS
            }
        },
        "additionalProperties": false
    })
}

pub(super) fn wait_timeout(timeout_ms: Option<u64>) -> Result<Duration> {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_WAIT_MS);
    if !(MIN_WAIT_MS..=MAX_WAIT_MS).contains(&timeout_ms) {
        return Err(Error::Tool(format!(
            "timeout_ms must be between {MIN_WAIT_MS} and {MAX_WAIT_MS}"
        )));
    }
    Ok(Duration::from_millis(timeout_ms))
}

pub(super) fn fork_context(context: &[Value], turns: ForkTurns) -> Vec<Value> {
    let mut fork = match turns {
        ForkTurns::None => Vec::new(),
        ForkTurns::All => context.to_vec(),
        ForkTurns::Last(turns) => {
            let start = context
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, item)| {
                    item.get("role").and_then(Value::as_str) == Some("user")
                        && !is_internal_message(item)
                })
                .nth(turns.saturating_sub(1))
                .map_or(0, |(index, _)| index);
            context[start..].to_vec()
        }
    };
    strip_attachment_references(&mut fork);
    fork
}

fn parse_fork_turns(value: Option<&str>) -> Result<ForkTurns> {
    let Some(value) = value else {
        return Ok(ForkTurns::default());
    };
    let value = value.trim();
    if value.eq_ignore_ascii_case("none") {
        return Ok(ForkTurns::None);
    }
    if value.eq_ignore_ascii_case("all") {
        return Ok(ForkTurns::All);
    }
    let turns = value.parse::<usize>().map_err(|_| {
        Error::Tool("fork_turns must be `none`, `all`, or a positive integer string".into())
    })?;
    if turns == 0 {
        return Err(Error::Tool(
            "fork_turns must be `none`, `all`, or a positive integer string".into(),
        ));
    }
    Ok(ForkTurns::Last(turns))
}

fn validate_task_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_TASK_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(Error::Tool(
            "task_name must contain 1-64 lowercase letters, digits, or underscores".into(),
        ));
    }
    Ok(())
}

fn validate_text(text: String) -> Result<String> {
    if text.trim().is_empty() {
        return Err(Error::Tool("text cannot be empty".into()));
    }
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(Error::Tool(format!(
            "text exceeded {MAX_MESSAGE_BYTES} bytes"
        )));
    }
    Ok(text)
}

fn peer_submission(session_id: &str, agent_path: &str, text: String) -> MessageSubmission {
    MessageSubmission {
        author: MessageAuthor::Peer {
            message_id: Uuid::new_v4().to_string(),
            session_id: session_id.into(),
            handle: agent_path.rsplit('/').next().unwrap_or(agent_path).into(),
            symbol: None,
        },
        text,
        attachments: Vec::new(),
        reply: None,
        requested_delivery: None,
        target_turn_id: None,
    }
}

pub(super) fn cleanup_error(error: Error, cleanup: Result<()>) -> Error {
    match cleanup {
        Ok(()) => error,
        Err(cleanup) => Error::Rollback {
            primary: Box::new(error),
            rollback: Box::new(cleanup),
        },
    }
}

pub(super) async fn supervise<T>(
    operation: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
{
    tokio::spawn(operation)
        .await
        .map_err(|error| Error::Stopped(format!("subagent lifecycle task failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_submission_preserves_agent_provenance() {
        let submission = peer_submission(
            "session-reviewer",
            "/root/team/reviewer",
            "Review the parser".into(),
        );

        assert!(matches!(
            submission,
            MessageSubmission {
                author: MessageAuthor::Peer {
                    session_id,
                    handle,
                    ..
                },
                text,
                ..
            } if session_id == "session-reviewer"
                && handle == "reviewer"
                && text == "Review the parser"
        ));
    }

    #[test]
    fn message_tool_schema_names_message_content_text() {
        let definition = message_definition("send_message", "send");

        assert_eq!(
            definition.parameters["required"],
            serde_json::json!(["target", "text"])
        );
    }
}
