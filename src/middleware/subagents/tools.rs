use crate::SessionId;
use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use super::runtime::{AgentPresentation, MAX_MESSAGE_BYTES, Wake, WakeTarget, monitor_agent};
use super::{
    Collaboration, DEFAULT_WAIT_MS, ForkTurns, MAX_TASK_NAME_BYTES, MAX_WAIT_MS, MIN_WAIT_MS, text,
};
use crate::backend::model::ToolDefinition;
use crate::middleware::tools::{HookIdentity, Tool, ToolContext};
use crate::protocol::strip_attachment_references;
use crate::protocol::{MessageAuthor, MessageSubmission, Submission, is_internal_message};
use crate::{BoxFuture, Error, Result};

pub(super) struct SpawnAgent(pub(super) Arc<Collaboration>);

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
    fn definition(&self) -> &Arc<ToolDefinition> {
        &text::DEFINITION.spawn_agent.tool
    }

    fn hook_identity(&self) -> Option<HookIdentity> {
        Some(HookIdentity {
            name: "spawn_agent",
            subjects: &["spawn_agent", "Agent"],
        })
    }

    fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: SpawnArgs = serde_json::from_value(arguments)?;
            validate_task_name(&arguments.task_name)?;
            let text = validate_text(arguments.text)?;
            let turns = parse_fork_turns(arguments.fork_turns.as_deref())?;
            let model: Arc<str> = match arguments.model {
                Some(model) => model.into(),
                None => Arc::clone(self.0.default_model.as_ref().unwrap_or(&self.0.scope.model)),
            };
            let reasoning_effort = arguments.reasoning_effort;
            let path: Arc<str> = format!(
                "{}/{}",
                self.0.scope.agent_path.trim_end_matches('/'),
                arguments.task_name
            )
            .into();
            let session_id = SessionId::generate();
            let collaboration = Arc::clone(&self.0);
            let submission = peer_submission(
                &collaboration.scope.session_id,
                &collaboration.scope.agent_path,
                text,
                &context.author,
                &format!(
                    "{}:{}:{}",
                    collaboration.scope.session_id, context.turn_id, context.call_id
                ),
            );
            supervise(async move {
                let shared = &collaboration.shared;
                let scope = &collaboration.scope;
                let lifetime = shared.track_execution(&scope.root_session_id).await?;
                shared
                    .reserve(
                        &scope.root_session_id,
                        &path,
                        &scope.agent_path,
                        session_id.share(),
                        scope.next_depth()?,
                        AgentPresentation {
                            model: Arc::clone(&model),
                            spawn_context: turns.label(),
                        },
                    )
                    .await?;
                let agent = match scope
                    .fork(
                        &session_id,
                        &path,
                        &model,
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
                let model = Arc::from(agent.model_route());
                let (sender, events) = agent.into_parts();
                if let Err(error) = shared
                    .attach(&scope.root_session_id, &path, sender.clone(), Some(model)) // rules-allow: the root keeps the sender while this task admits the first message
                    .await
                {
                    drop(sender);
                    let mut events = events;
                    while events.recv().await.is_some() {}
                    return Err(cleanup_error(
                        error,
                        shared.remove(&scope.root_session_id, &path).await,
                    ));
                }
                tokio::spawn(monitor_agent(
                    Arc::clone(&collaboration),
                    Arc::clone(&path),
                    events,
                    lifetime,
                ));
                if let Err(error) = admit_message(&sender, submission).await {
                    return Err(cleanup_error(
                        error,
                        shared.remove(&scope.root_session_id, &path).await,
                    ));
                }
                Ok(serde_json::json!({"task_name": path}).to_string())
            })
            .await
            .map(Into::into)
        })
    }
}

pub(super) struct SendMessage(pub(super) Arc<Collaboration>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageArgs {
    target: String,
    text: String,
}

impl Tool for SendMessage {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &text::DEFINITION.send_message.tool
    }

    fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: MessageArgs = serde_json::from_value(arguments)?;
            let message = peer_submission(
                &self.0.scope.session_id,
                &self.0.scope.agent_path,
                validate_text(arguments.text)?,
                &context.author,
                &format!(
                    "{}:{}:{}",
                    self.0.scope.session_id, context.turn_id, context.call_id
                ),
            );
            let collaboration = Arc::clone(&self.0);
            let target: Arc<str> = arguments.target.into();
            let turn_id = context.turn_id;
            supervise(async move {
                let shared = &collaboration.shared;
                let scope = &collaboration.scope;
                let lifetime = shared.track_execution(&scope.root_session_id).await?;
                let wake = shared
                    .send_message(&scope.root_session_id, &scope.agent_path, &target, message)
                    .await?;
                let Some(Wake {
                    message,
                    target: wake_target,
                    previous,
                }) = wake
                else {
                    return Ok(String::new());
                };
                let (sender, events, model) = match wake_target {
                    WakeTarget::Live(sender) => (sender, None, None),
                    WakeTarget::Resume {
                        session_id,
                        depth,
                        model,
                    } => {
                        let agent = match scope
                            .resume(&session_id, &target, depth, &model, turn_id)
                            .await
                        {
                            Ok(agent) => agent,
                            Err(error) => {
                                return Err(cleanup_error(
                                    error,
                                    shared
                                        .rollback(&scope.root_session_id, &target, previous)
                                        .await,
                                ));
                            }
                        };
                        let model = Arc::from(agent.model_route());
                        let (sender, events) = agent.into_parts();
                        (sender, Some(events), Some(model))
                    }
                };
                if let Err(error) = shared
                    .attach(&scope.root_session_id, &target, sender.clone(), model) // rules-allow: the root keeps the sender while this task admits the message
                    .await
                {
                    drop(sender);
                    if let Some(mut events) = events {
                        while events.recv().await.is_some() {}
                    }
                    return Err(cleanup_error(
                        error,
                        shared
                            .rollback(&scope.root_session_id, &target, previous)
                            .await,
                    ));
                }
                if let Some(events) = events {
                    tokio::spawn(monitor_agent(
                        Arc::clone(&collaboration),
                        Arc::clone(&target),
                        events,
                        lifetime,
                    ));
                }
                if let Err(error) = admit_message(&sender, message).await {
                    return Err(cleanup_error(
                        error,
                        shared
                            .rollback(&scope.root_session_id, &target, previous)
                            .await,
                    ));
                }
                Ok(String::new())
            })
            .await
            .map(Into::into)
        })
    }
}

pub(super) struct ListAgents(pub(super) Arc<Collaboration>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    path_prefix: Option<String>,
}

impl Tool for ListAgents {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &text::DEFINITION.list_agents.tool
    }

    fn call(
        &self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: ListArgs = serde_json::from_value(arguments)?;
            let agents = self
                .0
                .shared
                .list(
                    &self.0.scope.root_session_id,
                    arguments.path_prefix.as_deref(),
                )
                .await?;
            Ok((serde_json::json!({"agents": agents}).to_string()).into())
        })
    }
}

pub(super) struct InterruptAgent(pub(super) Arc<Collaboration>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetArgs {
    target: String,
}

impl Tool for InterruptAgent {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &text::DEFINITION.interrupt_agent.tool
    }

    fn call(
        &self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: TargetArgs = serde_json::from_value(arguments)?;
            let previous_status = self
                .0
                .shared
                .interrupt(&self.0.scope.root_session_id, &arguments.target)
                .await?;
            Ok((serde_json::json!({"previous_status": previous_status}).to_string()).into())
        })
    }
}

pub(super) struct WaitAgent(pub(super) Arc<Collaboration>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    timeout_ms: Option<u64>,
}

impl Tool for WaitAgent {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &text::DEFINITION.wait_agent.tool
    }

    fn cancel_on_input(&self) -> bool {
        true
    }

    fn call(
        &self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: WaitArgs = serde_json::from_value(arguments)?;
            let timeout = wait_timeout(arguments.timeout_ms)?;
            let agents = self
                .0
                .shared
                .wait(
                    &self.0.scope.root_session_id,
                    &self.0.scope.agent_path,
                    timeout,
                )
                .await?;
            Ok((serde_json::json!({
                "updated": !agents.is_empty(),
                "agents": agents
            })
            .to_string())
            .into())
        })
    }
}

pub(super) fn wait_timeout(timeout_ms: Option<u64>) -> Result<Duration> {
    let timeout_ms = timeout_ms.unwrap_or(*DEFAULT_WAIT_MS);
    if !(MIN_WAIT_MS..=MAX_WAIT_MS).contains(&timeout_ms) {
        return Err(Error::Tool(format!(
            "timeout_ms must be between {MIN_WAIT_MS} and {MAX_WAIT_MS}"
        )));
    }
    Ok(Duration::from_millis(timeout_ms))
}

pub(super) fn fork_context(
    context: &[Arc<Value>],
    turns: ForkTurns,
    pending: &BTreeSet<&str>,
) -> Vec<Arc<Value>> {
    let start = match turns {
        ForkTurns::None => return Vec::new(),
        ForkTurns::All => 0,
        ForkTurns::Last(turns) => context
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, item)| {
                item.get("role").and_then(Value::as_str) == Some("user")
                    && !is_internal_message(item)
            })
            .nth(turns.saturating_sub(1))
            .map_or(0, |(index, _)| index),
    };
    let mut fork = context
        .iter()
        .skip(start)
        .filter(|item| {
            item.get("type").and_then(Value::as_str) != Some("function_call")
                || item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .is_none_or(|call_id| !pending.contains(call_id))
        })
        .map(Arc::clone)
        .collect::<Vec<_>>();
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
    match value.parse::<usize>() {
        Ok(turns) if turns > 0 => Ok(ForkTurns::Last(turns)),
        _ => Err(Error::Tool(
            text::DEFINITION.error_fork_turns.as_str().into(),
        )),
    }
}

fn validate_task_name(name: &str) -> Result<()> {
    if !crate::identifier::valid_ascii_identifier(
        name,
        MAX_TASK_NAME_BYTES,
        crate::identifier::AsciiCase::Lower,
        b"_",
    ) {
        return Err(Error::Tool(
            text::DEFINITION
                .error_task_name
                .replace("{max_bytes}", &MAX_TASK_NAME_BYTES.to_string()),
        ));
    }
    Ok(())
}

fn validate_text(text: String) -> Result<String> {
    if text.trim().is_empty() {
        return Err(Error::Tool(
            text::DEFINITION.error_empty_text.as_str().into(),
        ));
    }
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(Error::Tool(format!(
            "text exceeded {MAX_MESSAGE_BYTES} bytes"
        )));
    }
    Ok(text)
}

fn peer_submission(
    session_id: &SessionId,
    agent_path: &str,
    text: String,
    origin: &MessageAuthor,
    command_id: &str,
) -> MessageSubmission {
    let (cause_id, ancestry) = origin.causal_origin();
    MessageSubmission {
        author: MessageAuthor::Source {
            message_id: if command_id.is_empty() {
                Uuid::new_v4().to_string().into()
            } else {
                command_id.into()
            },
            source: crate::protocol::MessageSource::Session {
                session_id: session_id.share(),
            },
            cause_id,
            ancestry,
            handle: agent_path.rsplit('/').next().unwrap_or(agent_path).into(),
            symbol: None,
        },
        text,
        attachments: Vec::new(),
        reply: None,
        requested_delivery: Some(crate::protocol::ActiveMessageDelivery::Steer),
        target_turn_id: None,
    }
}

async fn admit_message(
    sender: &crate::agent::AgentSender,
    message: MessageSubmission,
) -> Result<()> {
    sender
        .send_with_admission(Submission::message(message))?
        .wait()
        .await
        .map(|_| ())
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
            &"session-reviewer".parse().expect("session ID"),
            "/root/team/reviewer",
            "Review the parser".into(),
            &MessageAuthor::User,
            "stable-call",
        );

        assert!(matches!(
            submission,
            MessageSubmission {
                author: MessageAuthor::Source {
                    source: crate::protocol::MessageSource::Session { session_id },
                    handle,
                    ..
                },
                text,
                ..
            } if &*session_id == "session-reviewer"
                && &*handle == "reviewer"
                && text == "Review the parser"
        ));
    }

    #[test]
    fn peer_only_loops_reach_the_same_admission_bound_without_silently_trimming() {
        let mut origin = MessageAuthor::User;
        for depth in 0..=16 {
            let next = peer_submission(
                &"sender-session".parse().expect("session ID"),
                "/root/reviewer",
                "check it".into(),
                &origin,
                &format!("command-{depth}"),
            );
            crate::protocol::validate_message_content(&next.author, &next.text, &[])
                .expect("within bound");
            origin = next.author;
        }
        let next = peer_submission(
            &"sender-session".parse().expect("session ID"),
            "/root/reviewer",
            "check it".into(),
            &origin,
            "too-deep",
        );
        assert!(crate::protocol::validate_message_content(&next.author, &next.text, &[]).is_err());
    }
}
