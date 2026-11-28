//! Durable, nonblocking questions answered through ordinary user messages.

use crate::SessionId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::tools::{ApprovalRequirement, Catalog, ExecutionMode, Tool, ToolContext, ToolExposure};
use super::{
    ActiveCommandContext, FrontendEventSink, Middleware, MiddlewareCommandContext,
    MiddlewareCommandOutput, RuntimeContext, SessionStartContext, SubmissionResult,
};
use crate::agent::{AgentRole, WeakAgentSender};
use crate::backend::checkpoint::CheckpointStore;
use crate::backend::model::ToolDefinition;
use crate::protocol::{
    EventMsg, FrontendAction, FrontendActionListItem, FrontendBlock, FrontendCommand,
    FrontendContribution, FrontendEditor, FrontendEvent, FrontendListItemState, FrontendSlot,
    FrontendSymbol, FrontendTone, FrontendWidget, FrontendWidgetContent, Op,
};
use crate::{BoxFuture, Error, Result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    default_enabled: bool,
    manifest_label: String,
    manifest_description: String,
    ask_user: super::tools::ToolSpec,
    command_answer_description: String,
    command_skip_description: String,
    widget_title: String,
    widget_item_answered: String,
    widget_item_skipped: String,
    action_other_label: String,
    action_skip_label: String,
    editor_title: String,
    editor_label: String,
    editor_description: String,
    editor_submit_label: String,
    result_waiting: String,
    message_not_found: String,
    message_closed: String,
    message_empty_answer: String,
    message_too_many_open: String,
    message_invalid_question: String,
    message_busy: String,
    message_unavailable: String,
}
crate::embedded_config! { static DEFINITION: Definition = include_str!("questions.toml") => limit_schema; }
super::manifest::middleware_manifest! {
    /// Configuration and presentation metadata for durable user questions.
    "questions", DEFINITION, required: false, settings: &[]
}
const STATE_KEY: &str = "questions.v1";
const MAX_TITLE_BYTES: usize = 1024;
const MAX_OPTIONS: usize = 6;
const MAX_OPTION_BYTES: usize = 200;

fn limit_schema(definition: &mut Definition) {
    super::tools::limit_schema(
        &mut definition.ask_user.tool,
        [
            ("/properties/title", "maxLength", MAX_TITLE_BYTES.into()),
            ("/properties/options", "maxItems", MAX_OPTIONS.into()),
            (
                "/properties/options/items",
                "maxLength",
                MAX_OPTION_BYTES.into(),
            ),
        ],
    );
}
const MAX_OPEN: usize = 16;
const MAX_CLOSED: usize = 50;
const MAX_ANSWER_BYTES: usize = 4096;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Question {
    #[serde(rename = "id")]
    call: crate::protocol::DeferredToolCall,
    title: String,
    options: Vec<String>,
    asked_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    asked_turn: Option<Arc<str>>,
    outcome: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_outcome: Option<Outcome>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Outcome {
    Answered { text: String, at: i64 },
    Skipped { at: i64 },
}

impl Outcome {
    fn tool_output(&self) -> Value {
        match self {
            Self::Answered { text, .. } => serde_json::json!({"answer":text, "status":"answered"}),
            Self::Skipped { .. } => serde_json::json!({"status":"no_answer"}),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskArgs {
    title: String,
    #[serde(default)]
    options: Vec<String>,
}

/// Owns questions independently of turn lifetime, using the session checkpoint store.
#[derive(Default)]
pub struct Questions {
    // Middleware instances may be reused across sessions; each session serializes its own writes.
    sessions: Mutex<BTreeMap<SessionId, Arc<QuestionSession>>>,
}

impl std::fmt::Debug for Questions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Questions").finish_non_exhaustive()
    }
}

struct QuestionSession {
    id: SessionId,
    checkpoints: Arc<dyn CheckpointStore>,
    sender: WeakAgentSender,
    frontend: FrontendEventSink,
    access: tokio::sync::Mutex<()>,
}

enum Command {
    Answer,
    Skip,
}

impl Command {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Answer => "answer_question",
            Self::Skip => "skip_question",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        [Self::Answer, Self::Skip]
            .into_iter()
            .find(|command| command.as_str() == value)
            .ok_or_else(|| Error::Unknown(value.into()))
    }
}

impl Questions {
    fn sessions(&self) -> Result<MutexGuard<'_, BTreeMap<SessionId, Arc<QuestionSession>>>> {
        self.sessions.lock().map_err(|error| {
            tracing::warn!(%error, "questions sessions lock poisoned");
            Error::Poisoned("questions sessions")
        })
    }

    fn session(&self, id: &SessionId) -> Result<Arc<QuestionSession>> {
        self.sessions()?
            .get(id)
            .map(Arc::clone)
            .ok_or_else(|| Error::Tool(DEFINITION.message_unavailable.clone())) // rules-allow: the error owns a copy of the embedded message
    }
}

impl Middleware for Questions {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        if !matches!(runtime.role, AgentRole::Main) {
            return Ok(());
        }
        let session = Arc::new(QuestionSession {
            id: runtime.session_id.share(),
            checkpoints: Arc::clone(&runtime.checkpoints),
            sender: runtime.sender.clone(), // rules-allow: the session owns its weak sender handle
            frontend: Arc::clone(&runtime.frontend),
            access: tokio::sync::Mutex::new(()),
        });
        catalog.register(Arc::new(AskUser(Arc::clone(&session))))?;
        self.sessions()?.insert(runtime.session_id.share(), session);
        Ok(())
    }

    fn frontend(&self, _session_id: &str) -> FrontendContribution {
        FrontendContribution {
            capability: MANIFEST.id.into(),
            commands: [
                (Command::Answer, &DEFINITION.command_answer_description),
                (Command::Skip, &DEFINITION.command_skip_description),
            ]
            .into_iter()
            .map(|(name, description)| {
                FrontendCommand::new(name.as_str(), "<question-id>", description, false)
            })
            .collect(),
            ..Default::default()
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        let mut block = DEFINITION.ask_user.render(event)?;
        // The model reads the acceptance receipt; the reader sees that the question is now theirs.
        if let EventMsg::ToolCallEnd(result) = event
            && !result.is_error
        {
            block.text.clone_from(&DEFINITION.result_waiting);
        }
        Some(block)
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !matches!(context.runtime.role, AgentRole::Main) {
                return Ok(());
            }
            let session = self.session(&context.runtime.session_id)?;
            let _access = session.access.lock().await;
            let questions = session.load().await?;
            for question in &questions {
                if let Some(outcome) = &question.pending_outcome {
                    session.deliver(&question.call, outcome, question.asked_turn.as_deref())?;
                }
            }
            session.publish(questions)
        })
    }

    fn session_end<'a>(&'a self, runtime: &'a RuntimeContext) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.sessions()?.remove(&runtime.session_id);
            Ok(())
        })
    }

    fn command<'a>(
        &'a self,
        context: MiddlewareCommandContext<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareCommandOutput>> {
        Box::pin(async move {
            let session = self.session(context.session_id)?;
            let _access = session.access.lock().await;
            session
                .command(context.command, context.arguments, context.input)
                .await?;
            Ok(MiddlewareCommandOutput::events(Vec::new()))
        })
    }

    fn active_command<'a>(
        &'a self,
        context: &'a mut ActiveCommandContext<'_>,
    ) -> BoxFuture<'a, Result<Option<SubmissionResult>>> {
        Box::pin(async move {
            let session = self.session(context.session_id)?;
            // The tool future is suspended while this hook runs: never wait for its lock.
            let Ok(_access) = session.access.try_lock() else {
                return Ok(Some(SubmissionResult::Rejected(
                    DEFINITION.message_busy.clone(), // rules-allow: the rejection owns a copy of the embedded message
                )));
            };
            match session
                .command(context.command, context.arguments, context.input)
                .await
            {
                Ok(()) => Ok(Some(SubmissionResult::Handled)),
                Err(Error::Tool(message) | Error::Busy(message) | Error::Unknown(message)) => {
                    Ok(Some(SubmissionResult::Rejected(message)))
                }
                Err(error) => Err(error),
            }
        })
    }
}

impl QuestionSession {
    async fn load(&self) -> Result<Vec<Question>> {
        super::load_state_or_default(self.checkpoints.as_ref(), &self.id, STATE_KEY).await
    }

    async fn save(&self, questions: &[Question]) -> Result<()> {
        self.checkpoints
            .save_state(&self.id, STATE_KEY, &serde_json::to_value(questions)?)
            .await
    }

    fn publish(&self, questions: Vec<Question>) -> Result<()> {
        (self.frontend)(FrontendEvent::Widget {
            capability: MANIFEST.id.into(),
            item: Arc::new(widget(questions)),
        })
    }

    async fn command(self: &Arc<Self>, command: &str, id: &str, input: Option<&str>) -> Result<()> {
        let mut questions = self.load().await?;
        let (index, question) = questions
            .iter_mut()
            .enumerate()
            .find(|(_, question)| question.call.id() == id)
            .ok_or_else(|| Error::Tool(DEFINITION.message_not_found.clone()))?; // rules-allow: the error owns a copy of the embedded message
        if question.outcome.is_some() {
            return Err(Error::Tool(DEFINITION.message_closed.clone())); // rules-allow: the error owns a copy of the embedded message
        }
        if question.pending_outcome.is_some() {
            return Err(Error::Busy(DEFINITION.message_busy.clone())); // rules-allow: the error owns a copy of the embedded message
        }
        let at = chrono::Utc::now().timestamp_millis();
        let outcome = match Command::parse(command)? {
            Command::Answer => {
                let answer = input
                    .filter(|answer| !answer.trim().is_empty() && answer.len() <= MAX_ANSWER_BYTES)
                    .ok_or_else(|| Error::Tool(DEFINITION.message_empty_answer.clone()))?; // rules-allow: the error owns a copy of the embedded message
                Outcome::Answered {
                    text: answer.into(),
                    at,
                }
            }
            Command::Skip => Outcome::Skipped { at },
        };
        question.pending_outcome = Some(outcome);
        self.save(&questions).await?;
        let sent = questions.get(index).and_then(|question| {
            let outcome = question.pending_outcome.as_ref()?;
            Some(self.deliver(&question.call, outcome, question.asked_turn.as_deref()))
        });
        let sent =
            sent.unwrap_or_else(|| Err(Error::Internal("question outcome was not staged".into())));
        if let Err(error) = sent {
            if let Some(question) = questions.get_mut(index) {
                question.pending_outcome = None;
            }
            return match self.save(&questions).await {
                Ok(()) => Err(error),
                Err(rollback) => Err(Error::Rollback {
                    primary: Box::new(error),
                    rollback: Box::new(rollback),
                }),
            };
        }
        Ok(())
    }

    fn deliver(
        self: &Arc<Self>,
        call_id: &crate::protocol::DeferredToolCall,
        outcome: &Outcome,
        turn_id: Option<&str>,
    ) -> Result<()> {
        // An acknowledgment must be awaited outside the command loop. A durable pending
        // outcome survives a restart, and the agent's receipt prevents duplicate admission.
        let admission = self
            .sender
            .upgrade()
            .ok_or_else(|| Error::Stopped(DEFINITION.message_unavailable.clone()))? // rules-allow: the error owns a copy of the embedded message
            .send_with_admission(super::tools::async_tool_result(
                call_id,
                &DEFINITION.ask_user.tool.name,
                &outcome.tool_output(),
                turn_id.map(str::to_owned),
            ))?;
        let session = Arc::clone(self);
        let call_id = call_id.clone(); // rules-allow: the admission task retains the shared invocation reference
        tokio::spawn(async move {
            let admitted = admission.wait().await;
            let _access = session.access.lock().await;
            if let Err(error) = session.finish_answer(call_id.id(), admitted).await {
                tracing::warn!(%error, "question result could not be finalized");
            }
        });
        Ok(())
    }

    async fn finish_answer(
        &self,
        call_id: &str,
        admitted: Result<crate::agent::MessageAcceptance>,
    ) -> Result<()> {
        let mut questions = self.load().await?;
        let Some(question) = questions
            .iter_mut()
            .find(|question| question.call.id() == call_id)
        else {
            return Ok(());
        };
        let Some(outcome) = question.pending_outcome.take() else {
            return Ok(());
        };
        match admitted {
            Ok(_) => question.outcome = Some(outcome),
            Err(error @ Error::Invalid(_)) => {
                self.save(&questions).await?;
                self.publish(questions)?;
                return Err(error);
            }
            Err(error) => return Err(error),
        }
        prune(&mut questions);
        self.save(&questions).await?;
        self.publish(questions)
    }
}

struct AskUser(Arc<QuestionSession>);
impl Tool for AskUser {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &DEFINITION.ask_user.tool
    }
    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }
    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Never
    }
    fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let args: AskArgs = serde_json::from_value(arguments).map_err(|error| {
                Error::Tool(format!("{}: {error}", DEFINITION.message_invalid_question))
            })?;
            validate(&args)?;
            let _access = self.0.access.lock().await;
            let mut questions = self.0.load().await?;
            let invocation = context.deferred_call();
            if !questions.iter().any(|question| question.call == invocation) {
                if questions
                    .iter()
                    .filter(|question| question.outcome.is_none())
                    .count()
                    >= MAX_OPEN
                {
                    return Err(Error::Tool(DEFINITION.message_too_many_open.clone())); // rules-allow: the error owns a copy of the embedded message
                }
                questions.push(Question {
                    call: invocation.clone(), // rules-allow: the durable question and pending response share the invocation
                    title: args.title,
                    options: args.options,
                    asked_at: chrono::Utc::now().timestamp_millis(),
                    asked_turn: Some(context.turn_id),
                    outcome: None,
                    pending_outcome: None,
                });
                self.0.save(&questions).await?;
            }
            self.0.publish(questions)?;
            Ok(crate::protocol::ToolResponse::Deferred(invocation))
        })
    }
}

fn validate(args: &AskArgs) -> Result<()> {
    let mut options = BTreeSet::new();
    if args.title.trim().is_empty()
        || args.title.len() > MAX_TITLE_BYTES
        || args.options.len() > MAX_OPTIONS
        || args.options.iter().any(|option| {
            option.trim().is_empty() || option.len() > MAX_OPTION_BYTES || !options.insert(option)
        })
    {
        return Err(Error::Tool(DEFINITION.message_invalid_question.clone())); // rules-allow: the error owns a copy of the embedded message
    }
    Ok(())
}

fn prune(questions: &mut Vec<Question>) {
    let mut closed: Vec<_> = questions
        .iter()
        .enumerate()
        .filter_map(|(index, question)| {
            let (Outcome::Answered { at, .. } | Outcome::Skipped { at }) =
                question.outcome.as_ref()?;
            Some((*at, index))
        })
        .collect();
    let excess = closed.len().saturating_sub(MAX_CLOSED);
    closed.sort_unstable();
    let remove: BTreeSet<_> = closed
        .into_iter()
        .take(excess)
        .map(|(_, index)| index)
        .collect();
    let mut index = 0;
    questions.retain(|_| {
        let keep = !remove.contains(&index);
        index += 1;
        keep
    });
}

fn widget(mut questions: Vec<Question>) -> FrontendWidget {
    let open = questions
        .iter()
        .filter(|question| question.outcome.is_none())
        .count();
    questions.sort_by_key(|question| {
        (
            question.outcome.is_some(),
            std::cmp::Reverse(question.asked_at),
        )
    });
    FrontendWidget {
        tone: if open > 0 {
            FrontendTone::Warning
        } else {
            FrontendTone::Neutral
        },
        symbol: Some(FrontendSymbol::Question),
        content: Some(FrontendWidgetContent::ActionList {
            title: DEFINITION.widget_title.clone(), // rules-allow: the widget record owns a copy of the embedded text
            items: questions.into_iter().map(question_item).collect(),
            actions: Vec::new(),
        }),
        ..FrontendWidget::new("status", FrontendSlot::Attention, open.to_string())
    }
}

fn question_item(question: Question) -> FrontendActionListItem {
    let mut actions = Vec::new();
    let outcome = match question.outcome {
        Some(Outcome::Answered { text: answer, .. }) => {
            Some(DEFINITION.widget_item_answered.replace("{answer}", &answer))
        }
        Some(Outcome::Skipped { .. }) => {
            Some(DEFINITION.widget_item_skipped.clone()) // rules-allow: the widget owns its outcome text
        }
        None => {
            actions.extend(
                question
                    .options
                    .into_iter()
                    .enumerate()
                    .map(|(index, option)| {
                        action(
                            question.call.id(),
                            index.to_string(),
                            option,
                            &Command::Answer,
                            true,
                        )
                    }),
            );
            let mut other = action(
                question.call.id(),
                "other".into(),
                DEFINITION.action_other_label.clone(), // rules-allow: the widget record owns a copy of the embedded text
                &Command::Answer,
                false,
            );
            other.editor = Some(FrontendEditor {
                title: DEFINITION.editor_title.clone(), // rules-allow: the widget record owns a copy of the embedded text
                label: DEFINITION.editor_label.clone(), // rules-allow: the widget record owns a copy of the embedded text
                description: DEFINITION.editor_description.clone(), // rules-allow: the widget record owns a copy of the embedded text
                submit_label: DEFINITION.editor_submit_label.clone(), // rules-allow: the widget record owns a copy of the embedded text
            });
            actions.push(other);
            actions.push(action(
                question.call.id(),
                "skip".into(),
                DEFINITION.action_skip_label.clone(), // rules-allow: the widget record owns a copy of the embedded text
                &Command::Skip,
                false,
            ));
            None
        }
    };
    FrontendActionListItem {
        id: question.call.0,
        text: question.title,
        state: if outcome.is_none() {
            FrontendListItemState::Pending
        } else {
            FrontendListItemState::Completed
        },
        outcome,
        actions,
    }
}

fn action(
    question_id: &str,
    id: String,
    label: String,
    command: &Command,
    input_from_label: bool,
) -> FrontendAction {
    FrontendAction {
        id,
        label,
        symbol: FrontendSymbol::Question,
        tone: FrontendTone::Neutral,
        op: Op::CapabilityCommand {
            capability: MANIFEST.id.into(),
            command: command.as_str().into(),
            arguments: question_id.into(),
            input: None,
            target: None,
        },
        input_from_label,
        editor: None,
    }
}

#[cfg(test)]
mod tests;
