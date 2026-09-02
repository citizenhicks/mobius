//! Chat catalog, durable forking, and Bot-scoped thread search middleware.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::Middleware;
use super::MiddlewareCommandContext;
use super::MiddlewareCommandOutput;
use super::RuntimeContext;
use super::manifest::{MiddlewareManifest, MiddlewareSettingManifest};
use super::tools::{Catalog, Tool, ToolContext, rank_bm25, render_tool_event};
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::Checkpoint;
use crate::backend::checkpoint::SessionCursor;
use crate::backend::checkpoint::SessionPage;
use crate::backend::checkpoint::SessionPageRequest;
use crate::backend::checkpoint::SessionSummary;
use crate::backend::checkpoint::TranscriptPageRequest;
use crate::backend::model::ToolDefinition;
use crate::protocol::EventMsg;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendCommand;
use crate::protocol::FrontendContribution;
use crate::protocol::FrontendEvent;
use crate::protocol::FrontendPickerOption;
use crate::protocol::FrontendSlot;
use crate::protocol::FrontendSymbol;
use crate::protocol::FrontendTone;
use crate::protocol::FrontendWidget;
use crate::protocol::MessageTarget;
use crate::protocol::Op;
use crate::protocol::replay_events;
use crate::protocol::strip_attachment_references;

mod text {
    include!(concat!(env!("OUT_DIR"), "/src_middleware_sessions_text.rs"));
}

const MAX_PAGE_SIZE: usize = 1_000;
const MAX_THREAD_SEARCH_QUERY_BYTES: usize = 512;
const MAX_THREAD_SEARCH_RESULTS: usize = 8;
const MAX_THREAD_SEARCH_BATCHES: usize = 64;
const MAX_THREAD_SEARCH_DOCUMENT_CHARS: usize = 12_000;
const MAX_THREAD_SEARCH_EXCERPT_CHARS: usize = 800;
const _: () = {
    assert!(text::DEFAULTS_PAGE_SIZE >= 1);
    assert!(text::DEFAULTS_PAGE_SIZE <= MAX_PAGE_SIZE as i64);
    assert!(text::SETTING_PAGE_SIZE_STEP > 0);
};
/// Default number of chats loaded per catalog page.
pub const DEFAULT_PAGE_SIZE: usize = text::DEFAULTS_PAGE_SIZE as usize;
const SETTINGS: &[MiddlewareSettingManifest] = &[MiddlewareSettingManifest::Integer {
    id: "page_size",
    label: text::SETTING_PAGE_SIZE_LABEL,
    description: text::SETTING_PAGE_SIZE_DESCRIPTION,
    min: 1,
    max: Some(MAX_PAGE_SIZE as i64),
    step: text::SETTING_PAGE_SIZE_STEP,
    default: DEFAULT_PAGE_SIZE as i64,
}];

/// Configuration and presentation metadata for durable sessions.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "sessions",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: true,
    default_enabled: true,
    settings: SETTINGS,
};

/// Adds chat discovery and branching without changing the core loop.
pub struct Sessions {
    page_size: usize,
}

impl Sessions {
    /// Creates session middleware with a bounded catalog page size.
    pub fn new(page_size: usize) -> Result<Self> {
        if page_size == 0 || page_size > MAX_PAGE_SIZE {
            return Err(Error::Config(format!(
                "chat catalog page size must be between 1 and {MAX_PAGE_SIZE}"
            )));
        }
        Ok(Self { page_size })
    }
}

impl Default for Sessions {
    fn default() -> Self {
        Self {
            page_size: DEFAULT_PAGE_SIZE,
        }
    }
}

impl Middleware for Sessions {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        catalog.register(Arc::new(SearchThreads {
            checkpoints: Arc::clone(&runtime.checkpoints),
            session_id: runtime.session_id.clone(),
            bot_id: runtime.session_context.bot_id.clone(),
        }))
    }

    fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: self.name().into(),
            accepts_file_attachments: false,
            count: None,
            commands: vec![
                FrontendCommand {
                    name: "resume".into(),
                    arguments: String::new(),
                    description: text::COMMAND_RESUME_DESCRIPTION.into(),
                    requires_idle: true,
                },
                FrontendCommand {
                    name: "fork".into(),
                    arguments: String::new(),
                    description: text::COMMAND_FORK_DESCRIPTION.into(),
                    requires_idle: true,
                },
            ],
            widgets: vec![FrontendWidget {
                id: "fork".into(),
                slot: FrontendSlot::MessageActions,
                text: text::WIDGET_FORK_CHAT.into(),
                tone: FrontendTone::Neutral,
                symbol: Some(FrontendSymbol::Branch),
                icon_only: true,
                progress: None,
                content: None,
                action: Some(Op::CapabilityCommand {
                    capability: MANIFEST.id.into(),
                    command: "fork".into(),
                    arguments: String::new(),
                    input: None,
                    target: None,
                }),
            }],
            references: Vec::new(),
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        render_tool_event(
            event,
            |name| name == "search_threads",
            |_, arguments| super::tools::ToolHeading {
                title: text::RENDER_SEARCH_THREADS.into(),
                detail: arguments
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            },
        )
    }

    fn command<'a>(
        &'a self,
        context: MiddlewareCommandContext<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareCommandOutput>> {
        Box::pin(async move {
            match context.command {
                "resume" => resume(context, self.page_size).await,
                "fork" => fork(context).await,
                command => Err(Error::Unknown(format!("sessions command `{command}`"))),
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchThreadsArgs {
    query: String,
}

#[derive(Serialize)]
struct ThreadSearchHit {
    session_id: String,
    workspace: Option<String>,
    started_with: Option<String>,
    excerpt: String,
    updated_at: i64,
}

struct ThreadSearchDocument {
    summary: SessionSummary,
    messages: Vec<String>,
    text: String,
}

struct SearchThreads {
    checkpoints: Arc<dyn crate::backend::checkpoint::CheckpointStore>,
    session_id: String,
    bot_id: String,
}

impl Tool for SearchThreads {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "search_threads".into(),
            description: text::TOOL_SEARCH_THREADS_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": text::TOOL_SEARCH_THREADS_PARAMETER_QUERY_DESCRIPTION
                    }
                },
                "required": ["query"],
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
            let arguments: SearchThreadsArgs = serde_json::from_value(arguments)?;
            let query = arguments.query.trim();
            if query.is_empty() {
                return Err(Error::Tool("search_threads query cannot be empty".into()));
            }
            if query.len() > MAX_THREAD_SEARCH_QUERY_BYTES {
                return Err(Error::Tool(format!(
                    "search_threads query exceeds {MAX_THREAD_SEARCH_QUERY_BYTES} bytes"
                )));
            }

            let documents = self.documents().await?;
            let searchable = documents
                .iter()
                .map(|document| document.text.clone())
                .collect::<Vec<_>>();
            let hits = rank_bm25(&searchable, query, MAX_THREAD_SEARCH_RESULTS)
                .into_iter()
                .filter_map(|index| documents.get(index))
                .map(|document| ThreadSearchHit {
                    session_id: document.summary.session_id.clone(),
                    workspace: document.summary.session_context.workspace_label.clone(),
                    started_with: document.summary.first_user_message.clone(),
                    excerpt: search_excerpt(&document.messages, query),
                    updated_at: document.summary.updated_at,
                })
                .collect::<Vec<_>>();
            Ok(serde_json::to_string(&hits)?)
        })
    }
}

impl SearchThreads {
    async fn documents(&self) -> Result<Vec<ThreadSearchDocument>> {
        // ponytail: index the newest 1,000 matching threads; page deeper if one Bot exceeds that.
        let mut cursor = None;
        let mut summaries = Vec::new();
        while summaries.len() < MAX_PAGE_SIZE {
            let page = self
                .checkpoints
                .list_sessions_page(SessionPageRequest {
                    cursor,
                    limit: MAX_PAGE_SIZE,
                })
                .await?;
            let remaining = MAX_PAGE_SIZE - summaries.len();
            summaries.extend(
                page.sessions
                    .into_iter()
                    .filter(|summary| {
                        is_searchable_bot_thread(summary, &self.session_id, &self.bot_id)
                    })
                    .take(remaining),
            );
            let Some(next) = page.next_cursor else { break };
            cursor = Some(next);
        }

        let mut documents = Vec::new();
        for summary in summaries {
            let page = self
                .checkpoints
                .transcript_page(
                    &summary.session_id,
                    TranscriptPageRequest {
                        before_sequence: None,
                        max_batches: MAX_THREAD_SEARCH_BATCHES,
                    },
                )
                .await?;
            let mut messages = replay_events(
                &page.into_positioned_items_chronological(),
                &summary.session_id,
            )
            .into_iter()
            .filter_map(searchable_event_text)
            .collect::<Vec<_>>();
            if let Some(message) = summary.first_user_message.clone()
                && messages.first() != Some(&message)
            {
                messages.insert(0, message);
            }
            let text = messages
                .iter()
                .flat_map(|message| message.chars().chain(std::iter::once('\n')))
                .take(MAX_THREAD_SEARCH_DOCUMENT_CHARS)
                .collect();
            documents.push(ThreadSearchDocument {
                summary,
                messages,
                text,
            });
        }
        Ok(documents)
    }
}

fn is_searchable_bot_thread(
    summary: &SessionSummary,
    current_session_id: &str,
    bot_id: &str,
) -> bool {
    summary.session_id != current_session_id && summary.session_context.bot_id == bot_id
}

fn searchable_event_text(event: EventMsg) -> Option<String> {
    match event {
        EventMsg::Message(message) => Some(message.text),
        EventMsg::AssistantMessage(message) => assistant_message_text(&message.content),
        _ => None,
    }
}

fn search_excerpt(messages: &[String], query: &str) -> String {
    let query = query.to_lowercase();
    let terms = query.split_whitespace().collect::<Vec<_>>();
    let message = messages
        .iter()
        .find(|message| message.to_lowercase().contains(&query))
        .or_else(|| {
            messages.iter().find(|message| {
                let message = message.to_lowercase();
                terms.iter().any(|term| message.contains(term))
            })
        })
        .or_else(|| messages.first())
        .map_or("", String::as_str);
    message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_THREAD_SEARCH_EXCERPT_CHARS)
        .collect()
}

async fn fork(context: MiddlewareCommandContext<'_>) -> Result<MiddlewareCommandOutput> {
    if !context.arguments.trim().is_empty() {
        return Ok(MiddlewareCommandOutput::render(
            "sessions",
            "! usage: fork",
            FrontendTone::Warning,
        ));
    }
    let target = context.target;
    let through_sequence = target
        .as_ref()
        .map_or(context.checkpoint.sequence, |target| {
            target.checkpoint_sequence
        });
    let items = transcript_items_through(&context, through_sequence).await?;
    let Some(target) = target else {
        let options = fork_options(&items, context.session_id);
        if options.is_empty() {
            return Ok(MiddlewareCommandOutput::render(
                "sessions",
                "no messages to fork",
                FrontendTone::Neutral,
            ));
        }
        return Ok(MiddlewareCommandOutput::events(vec![
            FrontendEvent::Picker {
                title: text::PICKER_FORK_CHAT_FROM_MESSAGE.into(),
                options,
            },
        ]));
    };
    let transcript = fork_prefix(items, &target, context.session_id)?;
    let checkpoint = manual_fork_checkpoint(context.checkpoint, transcript);
    context
        .checkpoints
        .fork(
            &context.checkpoint.session_id,
            target.checkpoint_sequence,
            &checkpoint,
        )
        .await?;
    // A picker here waits for a choice the reader has already made. The fork is listed with
    // every other chat, so a confirmation that scrolls away is enough.
    Ok(MiddlewareCommandOutput::render(
        "sessions",
        format!("◇ forked chat {}", compact_id(&checkpoint.session_id)),
        FrontendTone::Success,
    ))
}

async fn transcript_items_through(
    context: &MiddlewareCommandContext<'_>,
    through_sequence: u64,
) -> Result<Vec<(MessageTarget, serde_json::Value)>> {
    let mut before_sequence =
        Some(through_sequence.checked_add(1).ok_or_else(|| {
            Error::Checkpoint("fork sequence exceeds the supported range".into())
        })?);
    let mut pages = Vec::new();
    loop {
        let page = context
            .checkpoints
            .transcript_page(
                context.session_id,
                TranscriptPageRequest {
                    before_sequence,
                    max_batches: DEFAULT_PAGE_SIZE,
                },
            )
            .await?;
        before_sequence = page.next_before_sequence;
        pages.push(page.into_positioned_items_chronological());
        if before_sequence.is_none() {
            break;
        }
    }
    Ok(pages.into_iter().rev().flatten().collect())
}

fn fork_options(
    items: &[(MessageTarget, serde_json::Value)],
    session_id: &str,
) -> Vec<FrontendPickerOption> {
    replay_events(items, session_id)
        .into_iter()
        .rev()
        .filter_map(|event| {
            let (description, message, target) = match event {
                EventMsg::Message(message) => (
                    text::PICKER_USER_MESSAGE,
                    message.text,
                    message.message_target?,
                ),
                EventMsg::AssistantMessage(message) => (
                    text::PICKER_ASSISTANT_MESSAGE,
                    assistant_message_text(&message.content)?,
                    message.message_target?,
                ),
                _ => return None,
            };
            Some(FrontendPickerOption {
                label: compact_message(&message),
                description: description.into(),
                detail: String::new(),
                symbol: None,
                shows_detail: false,
                op: Op::CapabilityCommand {
                    capability: MANIFEST.id.into(),
                    command: "fork".into(),
                    arguments: String::new(),
                    input: None,
                    target: Some(target),
                },
            })
        })
        .collect()
}

fn fork_prefix(
    items: Vec<(MessageTarget, serde_json::Value)>,
    target: &MessageTarget,
    session_id: &str,
) -> Result<Vec<serde_json::Value>> {
    let index = items
        .iter()
        .position(|(position, _)| position == target)
        .ok_or_else(invalid_fork_target)?;
    let prefix = &items[..=index];
    if !replay_events(prefix, session_id)
        .into_iter()
        .any(|event| match event {
            EventMsg::Message(message) => message.message_target.as_ref() == Some(target),
            EventMsg::AssistantMessage(message) => message.message_target.as_ref() == Some(target),
            _ => false,
        })
    {
        return Err(invalid_fork_target());
    }
    Ok(items
        .into_iter()
        .take(index + 1)
        .map(|(_, item)| item)
        .collect())
}

fn invalid_fork_target() -> Error {
    Error::Checkpoint("fork target is not a safe durable message boundary".into())
}

fn compact_message(message: &str) -> String {
    message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(42)
        .collect::<String>()
        .trim_end()
        .into()
}

fn assistant_message_text(content: &[crate::protocol::ModelStepContent]) -> Option<String> {
    [
        crate::protocol::ModelStepContentPhase::FinalAnswer,
        crate::protocol::ModelStepContentPhase::Commentary,
    ]
    .into_iter()
    .find_map(|phase| {
        let text = content
            .iter()
            .filter(|item| item.phase == phase)
            .map(|item| item.text.as_str())
            .collect::<String>();
        (!text.is_empty()).then_some(text)
    })
}

fn manual_fork_checkpoint(parent: &Checkpoint, context: Vec<serde_json::Value>) -> Checkpoint {
    let mut checkpoint = Checkpoint::empty(Uuid::new_v4().to_string());
    checkpoint.context = context;
    strip_attachment_references(&mut checkpoint.context);
    checkpoint
        .first_user_message
        .clone_from(&parent.first_user_message);
    checkpoint.model_route.clone_from(&parent.model_route);
    checkpoint
        .session_context
        .clone_from(&parent.session_context);
    checkpoint.metadata.clone_from(&parent.metadata);
    checkpoint.session_context.origin_label = None;
    checkpoint
}

async fn resume(
    context: MiddlewareCommandContext<'_>,
    page_size: usize,
) -> Result<MiddlewareCommandOutput> {
    let arguments = context.arguments.trim();
    let cursor = if arguments.is_empty() {
        None
    } else {
        match serde_json::from_str(arguments) {
            Ok(cursor) => Some(cursor),
            Err(_) => {
                return Ok(MiddlewareCommandOutput::render(
                    "sessions",
                    "! usage: resume",
                    FrontendTone::Warning,
                ));
            }
        }
    };
    let options = resume_options(&context, cursor, page_size).await?;
    if options.is_empty() {
        return Ok(MiddlewareCommandOutput::render(
            "sessions",
            "no saved chats",
            FrontendTone::Neutral,
        ));
    }
    Ok(MiddlewareCommandOutput::events(vec![
        FrontendEvent::Picker {
            title: text::PICKER_RESUME_CHAT.into(),
            options,
        },
    ]))
}

fn compact_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

async fn resume_options(
    context: &MiddlewareCommandContext<'_>,
    cursor: Option<SessionCursor>,
    page_size: usize,
) -> Result<Vec<FrontendPickerOption>> {
    let page = context
        .checkpoints
        .list_sessions_page(SessionPageRequest {
            cursor,
            limit: page_size,
        })
        .await?;
    resume_page_options(page, &context.checkpoint.session_id)
}

fn resume_page_options(
    page: SessionPage,
    current_session_id: &str,
) -> Result<Vec<FrontendPickerOption>> {
    let mut options = page
        .sessions
        .into_iter()
        .filter_map(|session| resume_option(session, current_session_id))
        .collect::<Vec<_>>();
    if let Some(cursor) = page.next_cursor {
        options.push(FrontendPickerOption {
            label: text::WIDGET_MORE_CHATS.into(),
            description: String::new(),
            detail: String::new(),
            symbol: None,
            shows_detail: false,
            op: Op::CapabilityCommand {
                capability: MANIFEST.id.into(),
                command: "resume".into(),
                arguments: serde_json::to_string(&cursor)?,
                input: None,
                target: None,
            },
        });
    }
    Ok(options)
}

fn resume_option(
    session: SessionSummary,
    current_session_id: &str,
) -> Option<FrontendPickerOption> {
    if !session.catalog_visible || session.session_id == current_session_id {
        return None;
    }
    let description = session_description(&session);
    let label = session.first_user_message.map_or_else(
        || {
            format!(
                "{} {}",
                if session.parent_session_id.is_some() {
                    "Fork"
                } else {
                    "Chat"
                },
                compact_id(&session.session_id)
            )
        },
        |message| compact_message(&message),
    );
    Some(FrontendPickerOption {
        label,
        description,
        detail: String::new(),
        symbol: None,
        shows_detail: false,
        op: Op::ResumeSession {
            session_id: session.session_id,
        },
    })
}

fn session_description(session: &SessionSummary) -> String {
    let mut details = [
        session.session_context.workspace_label.as_deref(),
        session.session_context.origin_label.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    details.push(format!("created at Unix time {}", session.created_at));
    details.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_search_is_bot_scoped_and_bm25_ranked() {
        let summary = |session_id: &str, bot_id: &str| SessionSummary {
            session_id: session_id.into(),
            session_context: crate::protocol::SessionContext {
                bot_id: bot_id.into(),
                ..crate::protocol::SessionContext::default()
            },
            parent_session_id: None,
            parent_sequence: None,
            sequence: 1,
            catalog_visible: true,
            first_user_message: None,
            execution_stats: Default::default(),
            created_at: 0,
            updated_at: 0,
        };

        assert!(is_searchable_bot_thread(
            &summary("prior", "researcher"),
            "current",
            "researcher"
        ));
        assert!(!is_searchable_bot_thread(
            &summary("other-bot", "writer"),
            "current",
            "researcher"
        ));
        assert!(!is_searchable_bot_thread(
            &summary("current", "researcher"),
            "current",
            "researcher"
        ));
        let mut hidden = summary("routine", "researcher");
        hidden.catalog_visible = false;
        assert!(is_searchable_bot_thread(&hidden, "current", "researcher"));
        let documents = vec![
            "Release checklist and changelog".into(),
            "Hermes durable Bot memory design".into(),
        ];
        assert_eq!(rank_bm25(&documents, "durable Bot", 1), [1]);
        assert_eq!(
            search_excerpt(&documents, "durable Bot"),
            "Hermes durable Bot memory design"
        );
    }

    #[test]
    fn thread_search_registers_for_the_required_bot() {
        let state = tempfile::tempdir().expect("state");
        let checkpoints: Arc<dyn crate::backend::checkpoint::CheckpointStore> = Arc::new(
            crate::backend::checkpoint::sqlite::SqliteCheckpoint::new(
                state.path().join("checkpoints.sqlite3"),
            )
            .expect("checkpoint store"),
        );
        let runtime = RuntimeContext {
            sender: crate::agent::test_sender(),
            checkpoints,
            session_id: "session".into(),
            model_route: "model".into(),
            model: "model".into(),
            approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
            session_context: crate::protocol::SessionContext {
                bot_id: "bot-1".into(),
                ..crate::protocol::SessionContext::default()
            },
            metadata: std::collections::BTreeMap::new(),
            role: crate::agent::AgentRole::Main,
            frontend: Arc::new(|_| Ok(())),
        };
        let mut catalog = Catalog::default();
        Sessions::default()
            .register(&mut catalog, &runtime)
            .expect("register session tools");

        assert_eq!(catalog.registered_definitions()[0].name, "search_threads");
    }

    #[tokio::test]
    async fn thread_search_reads_only_the_owning_bots_other_transcripts() {
        let state = tempfile::tempdir().expect("state");
        let checkpoints: Arc<dyn crate::backend::checkpoint::CheckpointStore> = Arc::new(
            crate::backend::checkpoint::sqlite::SqliteCheckpoint::new(
                state.path().join("checkpoints.sqlite3"),
            )
            .expect("checkpoint store"),
        );
        for (session_id, bot_id, message) in [
            ("prior", "researcher", "Hermes durable memory"),
            ("current", "researcher", "Current conversation"),
            ("other", "writer", "Hermes private draft"),
        ] {
            let mut checkpoint = Checkpoint::empty(session_id);
            checkpoint.sequence = 1;
            checkpoint.session_context.bot_id = bot_id.into();
            checkpoint.first_user_message = Some(message.into());
            let input = serde_json::json!({"role": "user", "content": message});
            checkpoint.context.push(input.clone());
            checkpoints
                .save(&checkpoint, &[input], None)
                .await
                .expect("save thread");
        }

        let documents = SearchThreads {
            checkpoints,
            session_id: "current".into(),
            bot_id: "researcher".into(),
        }
        .documents()
        .await
        .expect("search documents");

        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].summary.session_id, "prior");
        assert!(documents[0].text.contains("Hermes durable memory"));
    }

    #[test]
    fn sessions_rejects_page_sizes_outside_its_manifest_bounds() {
        assert!(Sessions::new(0).is_err());
        assert!(Sessions::new(MAX_PAGE_SIZE + 1).is_err());
    }

    #[test]
    fn fork_is_exposed_as_a_generic_message_action() {
        let contribution = Sessions::default().frontend();
        let widget = contribution.widgets.first().expect("fork widget");

        assert_eq!(widget.slot, FrontendSlot::MessageActions);
        assert_eq!(widget.text, "Fork chat");
        assert_eq!(widget.symbol, Some(FrontendSymbol::Branch));
        assert_eq!(
            widget.action,
            Some(Op::CapabilityCommand {
                capability: "sessions".into(),
                command: "fork".into(),
                arguments: String::new(),
                input: None,
                target: None,
            })
        );
    }

    #[test]
    fn fork_picker_lists_only_safe_user_and_assistant_messages() {
        let items = [
            crate::backend::model::message_input(&crate::protocol::MessageEvent {
                author: crate::protocol::MessageAuthor::User,
                delivery: crate::protocol::MessageDelivery::Turn,
                text: "Start here".into(),
                attachments: Vec::new(),
                message_target: None,
            })
            .expect("message input"),
            serde_json::json!({"type": "function_call", "call_id": "call-1", "name": "read"}),
            serde_json::json!({"type": "function_call_output", "call_id": "call-1", "output": "done"}),
            serde_json::json!({"role": "assistant", "content": "Finished"}),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            (
                MessageTarget {
                    checkpoint_sequence: index as u64 + 1,
                    batch_item_count: 1,
                },
                item,
            )
        })
        .collect::<Vec<_>>();

        let targets = fork_options(&items, "session")
            .into_iter()
            .map(|option| match option.op {
                Op::CapabilityCommand {
                    target: Some(target),
                    ..
                } => target,
                operation => panic!("expected targeted fork, got {operation:?}"),
            })
            .collect::<Vec<_>>();

        assert_eq!(targets, [items[3].0, items[0].0]);
    }

    #[test]
    fn fork_prefix_includes_the_selected_batch_item() {
        let items = vec![
            (
                MessageTarget {
                    checkpoint_sequence: 1,
                    batch_item_count: 1,
                },
                serde_json::json!({"role": "user", "content": "Question"}),
            ),
            (
                MessageTarget {
                    checkpoint_sequence: 2,
                    batch_item_count: 1,
                },
                serde_json::json!({"role": "assistant", "content": "Answer"}),
            ),
            (
                MessageTarget {
                    checkpoint_sequence: 2,
                    batch_item_count: 2,
                },
                serde_json::json!({"type": "function_call", "call_id": "call-1", "name": "read"}),
            ),
        ];

        let target = items[1].0;
        let prefix = fork_prefix(items, &target, "session").expect("fork prefix");

        assert_eq!(
            prefix,
            [
                serde_json::json!({"role": "user", "content": "Question"}),
                serde_json::json!({"role": "assistant", "content": "Answer"}),
            ]
        );
    }

    #[test]
    fn fork_prefix_rejects_a_message_with_an_open_tool_call() {
        let items = vec![
            (
                MessageTarget {
                    checkpoint_sequence: 1,
                    batch_item_count: 1,
                },
                serde_json::json!({"type": "function_call", "call_id": "call-1", "name": "read"}),
            ),
            (
                MessageTarget {
                    checkpoint_sequence: 1,
                    batch_item_count: 2,
                },
                serde_json::json!({"role": "assistant", "content": "Working"}),
            ),
        ];
        let target = items[1].0;

        let error = fork_prefix(items, &target, "session").expect_err("unsafe fork must fail");

        assert_eq!(
            error.to_string(),
            "checkpoint error: fork target is not a safe durable message boundary"
        );
    }

    #[test]
    fn resume_lists_fresh_forks() {
        let summary = |session_id: &str, parent_session_id: Option<&str>| SessionSummary {
            session_id: session_id.into(),
            session_context: Default::default(),
            parent_session_id: parent_session_id.map(str::to_string),
            parent_sequence: parent_session_id.map(|_| 4),
            sequence: 0,
            catalog_visible: true,
            first_user_message: None,
            execution_stats: Default::default(),
            created_at: 0,
            updated_at: 0,
        };

        assert_eq!(
            resume_option(summary("branch-id", Some("parent")), "current")
                .map(|option| option.label),
            Some("Fork branch-i".into())
        );
    }

    #[test]
    fn resume_lists_empty_durable_root_chats() {
        let option = resume_option(
            SessionSummary {
                session_id: "empty-root".into(),
                session_context: Default::default(),
                parent_session_id: None,
                parent_sequence: None,
                sequence: 0,
                catalog_visible: true,
                first_user_message: None,
                execution_stats: Default::default(),
                created_at: 0,
                updated_at: 0,
            },
            "current",
        );

        assert_eq!(
            option.map(|option| option.label),
            Some("Chat empty-ro".into())
        );
    }

    #[test]
    fn resume_lists_catalog_visible_chats_across_workspaces() {
        let summary = |session_id: &str, workspace: &str| SessionSummary {
            session_id: session_id.into(),
            session_context: crate::protocol::SessionContext {
                workspace_id: Some(workspace.into()),
                workspace_label: Some(workspace.into()),
                ..crate::protocol::SessionContext::default()
            },
            parent_session_id: None,
            parent_sequence: None,
            sequence: 1,
            catalog_visible: true,
            first_user_message: Some(format!("Work in {workspace}")),
            execution_stats: Default::default(),
            created_at: 0,
            updated_at: 0,
        };
        let options = resume_page_options(
            SessionPage {
                sessions: vec![
                    summary("workspace-a-chat", "Workspace A"),
                    summary("workspace-b-chat", "Workspace B"),
                ],
                next_cursor: None,
            },
            "current",
        )
        .expect("resume options");
        let session_ids = options
            .into_iter()
            .map(|option| match option.op {
                Op::ResumeSession { session_id } => session_id,
                operation => panic!("expected resume operation, got {operation:?}"),
            })
            .collect::<Vec<_>>();

        assert_eq!(session_ids, ["workspace-a-chat", "workspace-b-chat"]);
    }

    #[test]
    fn resume_excludes_only_current_and_explicitly_hidden_chats() {
        let summary = |session_id: &str, catalog_visible: bool| SessionSummary {
            session_id: session_id.into(),
            session_context: Default::default(),
            parent_session_id: None,
            parent_sequence: None,
            sequence: 0,
            catalog_visible,
            first_user_message: None,
            execution_stats: Default::default(),
            created_at: 0,
            updated_at: 0,
        };
        let options = resume_page_options(
            SessionPage {
                sessions: vec![
                    summary("current", true),
                    summary("hidden", false),
                    summary("visible", true),
                ],
                next_cursor: None,
            },
            "current",
        )
        .expect("resume options");
        let session_ids = options
            .into_iter()
            .map(|option| match option.op {
                Op::ResumeSession { session_id } => session_id,
                operation => panic!("expected resume operation, got {operation:?}"),
            })
            .collect::<Vec<_>>();

        assert_eq!(session_ids, ["visible"]);
    }

    #[test]
    fn resume_description_includes_workspace_and_origin_labels() {
        let option = resume_option(
            SessionSummary {
                session_id: "routine".into(),
                session_context: crate::protocol::SessionContext {
                    workspace_label: Some("Project One".into()),
                    origin_label: Some("routine".into()),
                    ..crate::protocol::SessionContext::default()
                },
                parent_session_id: None,
                parent_sequence: None,
                sequence: 1,
                catalog_visible: true,
                first_user_message: Some("Update dependencies".into()),
                execution_stats: Default::default(),
                created_at: 42,
                updated_at: 42,
            },
            "current",
        )
        .expect("resume option");

        assert_eq!(
            option.description,
            "Project One · routine · created at Unix time 42"
        );
    }

    #[test]
    fn manual_fork_keeps_context_workspace_and_metadata_but_clears_origin() {
        let mut parent = Checkpoint::empty("parent");
        parent.context = vec![serde_json::json!({
            "role": "user",
            "content": "Hello",
            "_mobius_attachments": [{
                "id": "378b8581-e96c-4413-a138-93e74561cb87",
                "name": "photo.png",
                "size": 1,
                "media_type": "image/png"
            }]
        })];
        parent.first_user_message = Some("Hello".into());
        parent.metadata.insert(
            "gateway.chat".into(),
            serde_json::json!({"workspace": "/srv/project"}),
        );
        parent.session_context = crate::protocol::SessionContext {
            bot_id: "bot-1".into(),
            workspace_id: Some("workspace-1".into()),
            workspace_label: Some("Project One".into()),
            origin_label: Some("routine".into()),
            ..crate::protocol::SessionContext::default()
        };

        let fork = manual_fork_checkpoint(&parent, parent.context.clone());

        assert!(fork.context[0].get("_mobius_attachments").is_none());
        assert_eq!(fork.first_user_message, parent.first_user_message);
        assert_eq!(fork.metadata, parent.metadata);
        assert_eq!(
            fork.session_context,
            crate::protocol::SessionContext {
                bot_id: "bot-1".into(),
                workspace_id: Some("workspace-1".into()),
                workspace_label: Some("Project One".into()),
                ..crate::protocol::SessionContext::default()
            }
        );
    }

    #[test]
    fn resume_page_preserves_the_next_catalog_cursor() {
        let cursor = SessionCursor {
            updated_at: 12,
            sequence: 4,
            session_id: "next".into(),
        };
        let options = resume_page_options(
            SessionPage {
                sessions: Vec::new(),
                next_cursor: Some(cursor.clone()),
            },
            "current",
        )
        .expect("build resume page");
        let Op::CapabilityCommand { arguments, .. } = &options[0].op else {
            panic!("expected middleware command");
        };

        assert_eq!(
            serde_json::from_str::<SessionCursor>(arguments).expect("decode cursor"),
            cursor
        );
    }
}
