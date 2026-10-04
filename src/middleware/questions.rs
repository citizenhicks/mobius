//! Durable, nonblocking questions answered through ordinary user messages.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::tools::{ApprovalRequirement, Catalog, ExecutionMode, Tool, ToolContext, ToolExposure};
use super::{
    ActiveCommandContext, FrontendEventSink, Middleware, MiddlewareCommandContext,
    MiddlewareCommandOutput, PromptSection, RuntimeContext, SessionStartContext, SubmissionResult,
};
use crate::agent::{AgentRole, WeakAgentSender};
use crate::backend::checkpoint::CheckpointStore;
use crate::backend::model::ToolDefinition;
use crate::protocol::{
    ActiveMessageDelivery, EventMsg, FrontendAction, FrontendActionListItem, FrontendBlock,
    FrontendCommand, FrontendContribution, FrontendEditor, FrontendEvent, FrontendListItemState,
    FrontendSlot, FrontendSymbol, FrontendTone, FrontendWidget, FrontendWidgetContent,
    MessageAuthor, MessageSubmission, Op, Submission,
};
use crate::{BoxFuture, Error, Result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    default_enabled: bool,
    manifest_label: String,
    manifest_description: String,
    prompt_root: String,
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
    answer_message: String,
    message_not_found: String,
    message_closed: String,
    message_empty_answer: String,
    message_too_many_open: String,
    message_invalid_question: String,
    message_busy: String,
    message_unavailable: String,
}
crate::embedded_config! { static DEFINITION: Definition = include_str!("questions.toml"); }
super::manifest::middleware_manifest! {
    /// Configuration and presentation metadata for durable user questions.
    "questions", DEFINITION, required: false, capability: None, settings: &[]
}
const STATE_KEY: &str = "questions.v1";
const MAX_TITLE_BYTES: usize = 1024;
const MAX_OPTIONS: usize = 6;
const MAX_OPTION_BYTES: usize = 200;
const MAX_OPEN: usize = 16;
const MAX_CLOSED: usize = 50;
const MAX_ANSWER_BYTES: usize = 4096;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Question {
    id: String,
    title: String,
    options: Vec<String>,
    asked_at: i64,
    outcome: Option<Outcome>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Outcome {
    Answered { text: String, at: i64 },
    Skipped { at: i64 },
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
    sessions: Mutex<BTreeMap<String, Arc<QuestionSession>>>,
}

struct QuestionSession {
    id: String,
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
    fn sessions(&self) -> Result<MutexGuard<'_, BTreeMap<String, Arc<QuestionSession>>>> {
        self.sessions
            .lock()
            .map_err(|_| Error::Poisoned("questions sessions"))
    }

    fn session(&self, id: &str) -> Result<Arc<QuestionSession>> {
        self.sessions()?
            .get(id)
            .map(Arc::clone)
            .ok_or_else(|| Error::Tool(DEFINITION.message_unavailable.clone()))
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
            id: runtime.session_id.clone(),
            checkpoints: Arc::clone(&runtime.checkpoints),
            sender: runtime.sender.clone(),
            frontend: Arc::clone(&runtime.frontend),
            access: tokio::sync::Mutex::new(()),
        });
        catalog.register(Arc::new(AskUser(Arc::clone(&session))))?;
        self.sessions()?.insert(runtime.session_id.clone(), session);
        Ok(())
    }

    fn prompt_section(&self, runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(matches!(runtime.role, AgentRole::Main)
            .then(|| PromptSection::new(&DEFINITION.prompt_root)))
    }

    fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: MANIFEST.id.into(),
            commands: [
                (Command::Answer, &DEFINITION.command_answer_description),
                (Command::Skip, &DEFINITION.command_skip_description),
            ]
            .into_iter()
            .map(|(name, description)| FrontendCommand {
                name: name.as_str().into(),
                arguments: "<question-id>".into(),
                description: description.to_owned(),
                requires_idle: false,
            })
            .collect(),
            ..Default::default()
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        DEFINITION.ask_user.render(event)
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
            session.publish(session.load().await?)
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
                .command(context.command, context.arguments, context.input, None)
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
                    DEFINITION.message_busy.clone(),
                )));
            };
            match session
                .command(
                    context.command,
                    context.arguments,
                    context.input,
                    Some(context.active_turn_id),
                )
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
        self.checkpoints
            .load_state(&self.id, STATE_KEY)
            .await?
            .map(serde_json::from_value)
            .transpose()
            .map(|questions| questions.unwrap_or_default())
            .map_err(Into::into)
    }

    async fn save(&self, questions: &[Question]) -> Result<()> {
        self.checkpoints
            .save_state(&self.id, STATE_KEY, &serde_json::to_value(questions)?)
            .await
    }

    fn publish(&self, questions: Vec<Question>) -> Result<()> {
        (self.frontend)(FrontendEvent::Widget {
            capability: MANIFEST.id.into(),
            item: widget(questions),
        })
    }

    async fn command(
        &self,
        command: &str,
        id: &str,
        input: Option<&str>,
        turn_id: Option<&str>,
    ) -> Result<()> {
        let mut questions = self.load().await?;
        let index = questions
            .iter()
            .position(|question| question.id == id)
            .ok_or_else(|| Error::Tool(DEFINITION.message_not_found.clone()))?;
        let question = &mut questions[index];
        if question.outcome.is_some() {
            return Err(Error::Tool(DEFINITION.message_closed.clone()));
        }
        let at = chrono::Utc::now().timestamp_millis();
        let message = match Command::parse(command)? {
            Command::Answer => {
                let answer = input
                    .filter(|answer| !answer.trim().is_empty() && answer.len() <= MAX_ANSWER_BYTES)
                    .ok_or_else(|| Error::Tool(DEFINITION.message_empty_answer.clone()))?;
                question.outcome = Some(Outcome::Answered {
                    text: answer.into(),
                    at,
                });
                // Replace template placeholders only, never placeholder-like text in user input.
                Some(
                    DEFINITION
                        .answer_message
                        .split("{title}")
                        .enumerate()
                        .map(|(index, part)| {
                            let rendered = part.replace("{answer}", answer);
                            if index == 0 {
                                rendered
                            } else {
                                format!("{}{rendered}", question.title)
                            }
                        })
                        .collect(),
                )
            }
            Command::Skip => {
                question.outcome = Some(Outcome::Skipped { at });
                None
            }
        };
        self.save(&questions).await?;
        if let Some(text) = message {
            // Enqueue only. Waiting for admission from inside the same command loop deadlocks.
            let sent = self
                .sender
                .upgrade()
                .ok_or_else(|| Error::Stopped(DEFINITION.message_unavailable.clone()))
                .and_then(|sender| {
                    sender.send(Submission::message(MessageSubmission {
                        author: MessageAuthor::User,
                        text,
                        attachments: Vec::new(),
                        reply: None,
                        requested_delivery: Some(ActiveMessageDelivery::Steer),
                        target_turn_id: turn_id.map(str::to_owned),
                    }))
                });
            if let Err(error) = sent {
                questions[index].outcome = None;
                if let Err(rollback) = self.save(&questions).await {
                    return Err(Error::Rollback {
                        primary: Box::new(error),
                        rollback: Box::new(rollback),
                    });
                }
                return Err(error);
            }
        }
        let previous_len = questions.len();
        prune(&mut questions);
        if questions.len() != previous_len {
            self.save(&questions).await?;
        }
        self.publish(questions)
    }
}

struct AskUser(Arc<QuestionSession>);
impl Tool for AskUser {
    fn definition(&self) -> ToolDefinition {
        let mut tool = DEFINITION.ask_user.tool.clone();
        tool.parameters["properties"]["title"]["maxLength"] = MAX_TITLE_BYTES.into();
        tool.parameters["properties"]["options"]["maxItems"] = MAX_OPTIONS.into();
        tool.parameters["properties"]["options"]["items"]["maxLength"] = MAX_OPTION_BYTES.into();
        tool
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
    fn call<'a>(
        &'a self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let args: AskArgs = serde_json::from_value(arguments)
                .map_err(|_| Error::Tool(DEFINITION.message_invalid_question.clone()))?;
            validate(&args)?;
            let _access = self.0.access.lock().await;
            let mut questions = self.0.load().await?;
            let response =
                serde_json::json!({"accepted":true,"question_id":context.call_id}).to_string();
            if !questions
                .iter()
                .any(|question| question.id == context.call_id)
            {
                if questions
                    .iter()
                    .filter(|question| question.outcome.is_none())
                    .count()
                    >= MAX_OPEN
                {
                    return Err(Error::Tool(DEFINITION.message_too_many_open.clone()));
                }
                questions.push(Question {
                    id: context.call_id,
                    title: args.title,
                    options: args.options,
                    asked_at: chrono::Utc::now().timestamp_millis(),
                    outcome: None,
                });
                self.0.save(&questions).await?;
            }
            self.0.publish(questions)?;
            Ok(response.into())
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
        return Err(Error::Tool(DEFINITION.message_invalid_question.clone()));
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
        id: "status".into(),
        slot: FrontendSlot::Attention,
        text: open.to_string(),
        tone: if open > 0 {
            FrontendTone::Warning
        } else {
            FrontendTone::Neutral
        },
        symbol: Some(FrontendSymbol::Question),
        icon_only: false,
        progress: None,
        action: None,
        content: Some(FrontendWidgetContent::ActionList {
            title: DEFINITION.widget_title.clone(),
            items: questions.into_iter().map(question_item).collect(),
            actions: Vec::new(),
        }),
    }
}

fn question_item(question: Question) -> FrontendActionListItem {
    let mut text = question.title;
    let mut actions = Vec::new();
    match &question.outcome {
        Some(Outcome::Answered { text: answer, .. }) => {
            text.push('\n');
            text.push_str(&DEFINITION.widget_item_answered.replace("{answer}", answer));
        }
        Some(Outcome::Skipped { .. }) => {
            text.push('\n');
            text.push_str(&DEFINITION.widget_item_skipped);
        }
        None => {
            actions.extend(
                question
                    .options
                    .into_iter()
                    .enumerate()
                    .map(|(index, option)| {
                        action(
                            &question.id,
                            index.to_string(),
                            option,
                            Command::Answer,
                            true,
                        )
                    }),
            );
            let mut other = action(
                &question.id,
                "other".into(),
                DEFINITION.action_other_label.clone(),
                Command::Answer,
                false,
            );
            other.editor = Some(FrontendEditor {
                title: DEFINITION.editor_title.clone(),
                label: DEFINITION.editor_label.clone(),
                description: DEFINITION.editor_description.clone(),
                submit_label: DEFINITION.editor_submit_label.clone(),
            });
            actions.push(other);
            actions.push(action(
                &question.id,
                "skip".into(),
                DEFINITION.action_skip_label.clone(),
                Command::Skip,
                false,
            ));
        }
    }
    FrontendActionListItem {
        id: question.id,
        text,
        state: if question.outcome.is_none() {
            FrontendListItemState::Pending
        } else {
            FrontendListItemState::Completed
        },
        actions,
    }
}

fn action(
    question_id: &str,
    id: String,
    label: String,
    command: Command,
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
