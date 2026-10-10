//! Chat catalog, durable forking, and bounded owner-scoped history retrieval.

use std::borrow::Cow;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::Middleware;
use super::MiddlewareCommandContext;
use super::MiddlewareCommandOutput;
use super::RuntimeContext;
use super::tools::{Catalog, ExecutionMode, Tool, ToolContext, ToolExposure, rank_bm25};
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
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) read_history: crate::middleware::tools::ToolSpec,
        pub(super) search_history: crate::middleware::tools::ToolSpec,
        pub(super) message_chat: crate::middleware::tools::ToolSpec,
        pub(super) list_chats: crate::middleware::tools::ToolSpec,
        #[serde(deserialize_with = "crate::middleware::manifest::deserialize_settings")]
        pub(super) settings: Vec<crate::middleware::manifest::MiddlewareSettingManifest>,
        pub(super) default_enabled: bool,
        pub(super) command_fork_description: String,
        pub(super) command_resume_description: String,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
        pub(super) message_fork_usage: String,
        pub(super) message_forked: String,
        pub(super) message_no_fork_messages: String,
        pub(super) message_no_saved_chats: String,
        pub(super) message_resume_usage: String,
        pub(super) picker_assistant_message: String,
        pub(super) picker_chat_label: String,
        pub(super) picker_created_at: String,
        pub(super) picker_fork_label: String,
        pub(super) picker_fork_chat_from_message: String,
        pub(super) picker_resume_chat: String,
        pub(super) picker_user_message: String,
        pub(super) widget_fork_chat: String,
        pub(super) widget_more_chats: String,
    }
    crate::embedded_config! { pub(super) static DEFINITION: Definition = include_str!("sessions.toml"); }
}
const MAX_HISTORY_QUERY_BYTES: usize = 512;
const MAX_HISTORY_CURSOR_BYTES: usize = 8_192;
const MAX_HISTORY_RESULTS: usize = 6;
const MAX_HISTORY_PAGES: usize = 32;
const MAX_HISTORY_ITEMS: usize = 128;
const MAX_HISTORY_SCAN_CHARS: usize = 64_000;
const HISTORY_CHUNK_CHARS: usize = 8_000;
const HISTORY_EXCERPT_CHARS: usize = 600;
const MAX_HISTORY_READ_CHARS: usize = 4_000;
const MAX_HANDLE_TITLE_BYTES: usize = 64;

super::manifest::middleware_manifest! {
/// Configuration and presentation metadata for durable sessions.
    "sessions", text::DEFINITION, required: true, settings: &text::DEFINITION.settings
}

/// One open chat of an owner that can receive peer messages from its sibling chats.
#[derive(Debug, PartialEq, Eq)]
pub struct LiveChat {
    /// The durable chat identifier.
    pub session_id: String,
    /// The user-visible chat title.
    pub title: Option<String>,
    /// The workspace label.
    pub workspace: Option<String>,
    /// Whether the chat has an active turn; an idle chat starts one for a message.
    pub running: bool,
    /// Active turn identity, used to interrupt only that turn.
    pub turn_id: Option<String>,
}

impl LiveChat {
    /// Returns the peer identity recipients see; its `#id` addresses this chat.
    #[must_use]
    pub fn handle(&self) -> String {
        let id = compact_id(&self.session_id);
        match &self.title {
            Some(title) => format!(
                "{} #{id}",
                &title[..title.floor_char_boundary(MAX_HANDLE_TITLE_BYTES)]
            ),
            None => format!("chat #{id}"),
        }
    }

    /// Reports whether a `message_chat` target names this chat.
    #[must_use]
    pub fn is_target(&self, target: &str) -> bool {
        let target = target.strip_prefix('#').unwrap_or(target);
        target == self.session_id || target == compact_id(&self.session_id)
    }
}

/// Destination for an existing chat command or a new project's first message.
pub enum ChatTarget<'a> {
    /// One existing chat identified by its session ID or sender handle.
    Existing(&'a str),
    /// A new chat in an existing workspace directory.
    Workspace(&'a std::path::Path),
}

/// One `message_chat` operation, validated where the tool parses its arguments.
pub enum PeerCommand<'a> {
    /// A text message to an existing chat or a new workspace chat.
    Message {
        /// The destination chat.
        target: ChatTarget<'a>,
        /// The message text.
        text: String,
        /// How the message reaches a running recipient.
        delivery: crate::protocol::ActiveMessageDelivery,
    },
    /// Interrupts one active turn of an existing chat.
    Interrupt {
        /// The session ID or sender handle of the chat.
        target: &'a str,
        /// The observed turn to interrupt.
        turn_id: String,
    },
}

/// Host access to the other chats of a chat's owner.
pub trait LiveChats: Send + Sync {
    /// Lists the owner's open chats other than `session_id`.
    /// # Errors
    ///
    /// Returns an error if the host cannot read its open chats.
    fn list<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<Vec<LiveChat>>>;

    /// Delivers a message or interrupt, returning the destination's session ID.
    /// # Errors
    ///
    /// Returns an error if the target or workspace is unavailable, the
    /// initiating turn cannot authorize it, or the destination rejects it.
    fn send<'a>(
        &'a self,
        session_id: &'a str,
        command: PeerCommand<'a>,
        initiating_author: &'a crate::protocol::MessageAuthor,
        command_id: &'a str,
    ) -> BoxFuture<'a, Result<String>>;
}

/// Adds chat discovery and branching without changing the core loop.
pub struct Sessions {
    page_size: usize,
    files: Option<crate::backend::session_files::SessionFileStore>,
    live_chats: Option<Arc<dyn LiveChats>>,
}

impl Sessions {
    /// Creates session middleware with a bounded catalog page size, the durable media
    /// storage a fork is granted its observations from, and, for hosts that have them,
    /// the owner's other open chats that main chats may list and message.
    /// # Errors
    ///
    /// Returns an error if the page size is outside its manifest bounds.
    pub fn new(
        page_size: usize,
        files: Option<crate::backend::session_files::SessionFileStore>,
        live_chats: Option<Arc<dyn LiveChats>>,
    ) -> Result<Self> {
        let (min, max) = page_size_bounds();
        if !(min..=max).contains(&page_size) {
            return Err(Error::Config(format!(
                "chat catalog page size must be between {min} and {max}"
            )));
        }
        Ok(Self {
            page_size,
            files,
            live_chats,
        })
    }
}

fn page_size_bounds() -> (usize, usize) {
    match text::DEFINITION
        .settings
        .iter()
        .find(|setting| setting.id() == "page_size")
    {
        Some(super::manifest::MiddlewareSettingManifest::Integer {
            min,
            max: Some(max),
            ..
        }) => (
            usize::try_from(*min).expect("embedded page size minimum must fit"),
            usize::try_from(*max).expect("embedded page size maximum must fit"),
        ),
        _ => panic!("missing embedded integer setting page_size"),
    }
}

enum Command {
    Resume,
    Fork,
}

impl Command {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Fork => "fork",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        [Self::Resume, Self::Fork]
            .into_iter()
            .find(|command| command.as_str() == value)
            .ok_or_else(|| Error::Unknown(format!("{} command `{value}`", MANIFEST.id)))
    }
}

impl Middleware for Sessions {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        let history = Arc::new(History {
            checkpoints: Arc::clone(&runtime.checkpoints),
            session_id: runtime.session_id.clone(),
            owner_id: runtime.session_context.owner_id.clone(),
        });
        catalog.register(Arc::new(SearchHistory(Arc::clone(&history))))?;
        catalog.register(Arc::new(ReadHistory(Arc::clone(&history))))?;
        let (Some(chats), crate::agent::AgentRole::Main) = (&self.live_chats, &runtime.role) else {
            return Ok(());
        };
        let chats = Arc::new(OpenChats {
            history,
            chats: Arc::clone(chats),
        });
        catalog.register(Arc::new(ListChats(Arc::clone(&chats))))?;
        catalog.register(Arc::new(MessageChat(chats)))
    }

    fn frontend(&self, _session_id: &str) -> FrontendContribution {
        FrontendContribution {
            capability: MANIFEST.id.into(),
            commands: [
                (
                    Command::Resume,
                    &text::DEFINITION.command_resume_description,
                ),
                (Command::Fork, &text::DEFINITION.command_fork_description),
            ]
            .into_iter()
            .map(|(command, description)| FrontendCommand {
                name: command.as_str().into(),
                arguments: String::new(),
                description: description.to_owned(),
                requires_idle: true,
            })
            .collect(),
            widgets: vec![FrontendWidget {
                id: Command::Fork.as_str().into(),
                slot: FrontendSlot::MessageActions,
                text: text::DEFINITION.widget_fork_chat.clone(),
                tone: FrontendTone::Neutral,
                symbol: Some(FrontendSymbol::Branch),
                icon_only: true,
                progress: None,
                content: None,
                action: Some(Op::command(
                    MANIFEST.id,
                    Command::Fork.as_str(),
                    String::new(),
                )),
            }],
            ..Default::default()
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        [
            &text::DEFINITION.read_history,
            &text::DEFINITION.search_history,
            &text::DEFINITION.message_chat,
            &text::DEFINITION.list_chats,
        ]
        .into_iter()
        .find_map(|spec| spec.render(event))
    }

    fn command<'a>(
        &'a self,
        context: MiddlewareCommandContext<'a>,
    ) -> BoxFuture<'a, Result<MiddlewareCommandOutput>> {
        Box::pin(async move {
            match Command::parse(context.command)? {
                Command::Resume => resume(context, self.page_size).await,
                Command::Fork => fork(context, self.files.as_ref(), self.page_size).await,
            }
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum HistoryScope {
    #[default]
    Current,
    OtherChats,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchHistoryArgs {
    query: String,
    #[serde(default)]
    scope: HistoryScope,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadHistoryArgs {
    session_id: Option<String>,
    target: MessageTarget,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_read_chars")]
    max_chars: usize,
}

const fn default_read_chars() -> usize {
    MAX_HISTORY_READ_CHARS
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HistoryCursor {
    scope: HistoryScope,
    query: String,
    catalog: Option<SessionCursor>,
    session_id: Option<String>,
    before_sequence: Option<u64>,
    remaining_items: Option<usize>,
    offset: usize,
}

#[derive(Serialize)]
struct HistoryHit<'a> {
    session_id: &'a str,
    target: MessageTarget,
    kind: &'static str,
    offset: usize,
    excerpt: String,
}

struct HistoryDocument {
    session_id: Arc<str>,
    target: MessageTarget,
    kind: &'static str,
    offset: usize,
    text: String,
}

struct History {
    checkpoints: Arc<dyn crate::backend::checkpoint::CheckpointStore>,
    session_id: String,
    owner_id: String,
}

struct SearchHistory(Arc<History>);
struct ReadHistory(Arc<History>);

struct OpenChats {
    history: Arc<History>,
    chats: Arc<dyn LiveChats>,
}

struct ListChats(Arc<OpenChats>);
struct MessageChat(Arc<OpenChats>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageChatArgs {
    target: Option<String>,
    workspace: Option<std::path::PathBuf>,
    text: Option<String>,
    delivery: Option<crate::protocol::ActiveMessageDelivery>,
    interrupt_turn_id: Option<String>,
}

impl MessageChatArgs {
    fn command(&mut self) -> Result<PeerCommand<'_>> {
        let target = match (self.target.as_deref(), self.workspace.as_deref()) {
            (Some(target), None) => ChatTarget::Existing(target),
            (None, Some(workspace)) => ChatTarget::Workspace(workspace),
            _ => {
                return Err(Error::Tool(
                    "provide exactly one of target or workspace".into(),
                ));
            }
        };
        match (
            target,
            self.text.take(),
            self.interrupt_turn_id.take(),
            self.delivery,
        ) {
            (target, Some(text), None, delivery) => Ok(PeerCommand::Message {
                target,
                text,
                delivery: delivery.unwrap_or(crate::protocol::ActiveMessageDelivery::Steer),
            }),
            (ChatTarget::Existing(target), None, Some(turn_id), None) => {
                Ok(PeerCommand::Interrupt { target, turn_id })
            }
            _ => Err(Error::Tool(
                "provide text with optional delivery, or interrupt_turn_id with an existing target"
                    .into(),
            )),
        }
    }
}

impl Tool for ListChats {
    fn definition(&self) -> ToolDefinition {
        text::DEFINITION.list_chats.tool.clone()
    }

    fn read_only(&self) -> bool {
        true
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let chats = self.0.chats.list(&self.0.history.session_id).await?;
            let chats = chats
                .iter()
                .map(|chat| {
                    serde_json::json!({
                        "target": chat.session_id,
                        "title": chat.title,
                        "workspace": chat.workspace,
                        "running": chat.running,
                        "turn_id": chat.turn_id,
                    })
                })
                .collect::<Vec<_>>();
            Ok(serde_json::json!({ "chats": chats }).to_string().into())
        })
    }
}

impl Tool for MessageChat {
    fn definition(&self) -> ToolDefinition {
        text::DEFINITION.message_chat.tool.clone()
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn call<'a>(
        &'a self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let mut args: MessageChatArgs = serde_json::from_value(arguments)?;
            let command = args.command()?;
            let target = self
                .0
                .chats
                .send(
                    &self.0.history.session_id,
                    command,
                    &context.author,
                    &context.call_id,
                )
                .await?;
            Ok(serde_json::json!({"target": target}).to_string().into())
        })
    }
}

impl Tool for SearchHistory {
    fn definition(&self) -> ToolDefinition {
        let mut tool = text::DEFINITION.search_history.tool.clone();
        tool.parameters["properties"]["query"]["maxLength"] = MAX_HISTORY_QUERY_BYTES.into();
        tool.parameters["properties"]["cursor"]["maxLength"] = MAX_HISTORY_CURSOR_BYTES.into();
        tool
    }

    fn read_only(&self) -> bool {
        true
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            self.0
                .search(serde_json::from_value(arguments)?)
                .await
                .map(Into::into)
        })
    }
}

impl Tool for ReadHistory {
    fn definition(&self) -> ToolDefinition {
        let mut tool = text::DEFINITION.read_history.tool.clone();
        tool.parameters["properties"]["max_chars"]["maximum"] = MAX_HISTORY_READ_CHARS.into();
        tool
    }

    fn read_only(&self) -> bool {
        true
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            self.0
                .read(serde_json::from_value(arguments)?)
                .await
                .map(Into::into)
        })
    }
}

impl History {
    async fn page(
        &self,
        session_id: &str,
        before_sequence: Option<u64>,
    ) -> Result<crate::backend::checkpoint::TranscriptPage> {
        self.checkpoints
            .transcript_page(
                session_id,
                TranscriptPageRequest {
                    before_sequence,
                    max_batches: 1,
                },
            )
            .await
    }
    async fn authorize(&self, session_id: &str) -> Result<()> {
        validate_history_session_id(session_id)?;
        let checkpoint = self.checkpoints.load(session_id).await?;
        if !checkpoint
            .is_some_and(|checkpoint| checkpoint.session_context.owner_id == self.owner_id)
        {
            return Err(Error::Tool(
                "history chat is unavailable to this owner".into(),
            ));
        }
        Ok(())
    }

    fn cursor(&self, arguments: SearchHistoryArgs) -> Result<HistoryCursor> {
        let query = arguments.query.trim();
        if query.is_empty() || query.len() > MAX_HISTORY_QUERY_BYTES {
            return Err(Error::Tool(format!(
                "history query must be 1–{MAX_HISTORY_QUERY_BYTES} bytes"
            )));
        }
        let Some(value) = arguments.cursor else {
            return Ok(HistoryCursor {
                scope: arguments.scope,
                query: query.into(),
                catalog: None,
                session_id: (arguments.scope == HistoryScope::Current)
                    .then(|| self.session_id.clone()),
                before_sequence: None,
                remaining_items: None,
                offset: 0,
            });
        };
        if value.len() > MAX_HISTORY_CURSOR_BYTES {
            return Err(Error::Tool("history cursor is too large".into()));
        }
        let cursor: HistoryCursor = serde_json::from_str(&value)?;
        if cursor.scope != arguments.scope
            || cursor.query != query
            || cursor.remaining_items == Some(0)
            || (cursor.remaining_items.is_some() && cursor.before_sequence.is_none())
            || (cursor.remaining_items.is_none() && cursor.offset != 0)
            || (cursor.scope == HistoryScope::Current
                && (cursor.session_id.as_deref() != Some(&self.session_id)
                    || cursor.catalog.is_some()))
            || (cursor.scope == HistoryScope::OtherChats
                && cursor.session_id.as_deref() == Some(&self.session_id))
        {
            return Err(Error::Tool(
                "history cursor does not match this query and scope".into(),
            ));
        }
        if let Some(catalog) = &cursor.catalog {
            validate_history_session_id(&catalog.session_id)?;
            validate_history_sequence(catalog.sequence)?;
        }
        if let Some(sequence) = cursor.before_sequence {
            validate_history_sequence(sequence)?;
        }
        Ok(cursor)
    }

    async fn search(&self, arguments: SearchHistoryArgs) -> Result<String> {
        let mut cursor = self.cursor(arguments)?;
        if let Some(session_id) = &cursor.session_id {
            self.authorize(session_id).await?;
        }
        if let Some(catalog) = &cursor.catalog {
            self.authorize(&catalog.session_id).await?;
        }
        let mut documents = Vec::new();
        let mut scanned_chars = 0;
        let mut scanned_items = 0;
        let mut complete = false;
        // ponytail: scan bounded transcript batches on demand; add an index only if measured latency warrants it.
        for _ in 0..MAX_HISTORY_PAGES {
            let Some(session_id) = cursor.session_id.as_deref() else {
                if !self.advance_session(&mut cursor).await? {
                    complete = true;
                    break;
                }
                continue;
            };
            let session_id = Arc::<str>::from(session_id);
            let page = self.page(&session_id, cursor.before_sequence).await?;
            let Some(batch) = page.batches.into_iter().next() else {
                if cursor.scope == HistoryScope::Current {
                    complete = true;
                    break;
                }
                cursor.session_id = None;
                cursor.before_sequence = None;
                cursor.remaining_items = None;
                cursor.offset = 0;
                continue;
            };
            scan_history_batch(
                &batch,
                &session_id,
                &mut cursor,
                &mut documents,
                &mut scanned_items,
                &mut scanned_chars,
            )?;
            if scanned_items >= MAX_HISTORY_ITEMS
                || MAX_HISTORY_SCAN_CHARS - scanned_chars <= MAX_HISTORY_QUERY_BYTES
            {
                break;
            }
        }
        let searchable = documents.iter().map(|document| document.text.as_str());
        let hits = rank_bm25(searchable, &cursor.query, MAX_HISTORY_RESULTS)
            .into_iter()
            .map(|index| {
                let document = &documents[index];
                let (offset, excerpt) = history_excerpt(&document.text, &cursor.query);
                HistoryHit {
                    session_id: &document.session_id,
                    target: document.target,
                    kind: document.kind,
                    offset: document.offset + offset,
                    excerpt,
                }
            })
            .collect::<Vec<_>>();
        let next_cursor = (!complete)
            .then(|| serde_json::to_string(&cursor))
            .transpose()?;
        Ok(serde_json::to_string(&serde_json::json!({
            "hits": hits, "next_cursor": next_cursor,
            "scanned_items": scanned_items, "scanned_chars": scanned_chars,
        }))?)
    }

    async fn advance_session(&self, cursor: &mut HistoryCursor) -> Result<bool> {
        let page = self
            .checkpoints
            .list_sessions_page(SessionPageRequest {
                owner_id: Some(self.owner_id.clone()),
                cursor: cursor.catalog.take(),
                limit: 1,
            })
            .await?;
        let Some(summary) = page.sessions.into_iter().next() else {
            return Ok(false);
        };
        if summary.session_context.owner_id != self.owner_id {
            return Err(Error::Checkpoint(
                "session catalog returned another owner's chat".into(),
            ));
        }
        cursor.catalog = Some(SessionCursor {
            updated_at: summary.updated_at,
            sequence: summary.sequence,
            session_id: summary.session_id.clone(),
        });
        if summary.session_id != self.session_id {
            cursor.session_id = Some(summary.session_id);
        }
        Ok(true)
    }

    async fn read(&self, arguments: ReadHistoryArgs) -> Result<String> {
        if arguments.max_chars == 0 || arguments.max_chars > MAX_HISTORY_READ_CHARS {
            return Err(Error::Tool(format!(
                "max_chars must be 1–{MAX_HISTORY_READ_CHARS}"
            )));
        }
        let session_id = arguments.session_id.as_deref().unwrap_or(&self.session_id);
        self.authorize(session_id).await?;
        let before_sequence = arguments
            .target
            .checkpoint_sequence
            .checked_add(1)
            .ok_or_else(|| Error::Tool("history target sequence is too large".into()))?;
        validate_history_sequence(before_sequence)?;
        let page = self.page(session_id, Some(before_sequence)).await?;
        let item = page
            .batches
            .first()
            .filter(|batch| batch.sequence == arguments.target.checkpoint_sequence)
            .and_then(|batch| {
                arguments
                    .target
                    .batch_item_count
                    .checked_sub(1)
                    .and_then(|index| batch.items.get(index))
            })
            .and_then(history_text)
            .ok_or_else(|| {
                Error::Tool("history target is not a readable transcript item".into())
            })?;
        let (content, next_offset) = history_chunk(&item.1, arguments.offset, arguments.max_chars)?;
        Ok(serde_json::to_string(&serde_json::json!({
            "session_id": session_id, "target": arguments.target, "kind": item.0,
            "offset": arguments.offset, "text": content, "next_offset": next_offset,
        }))?)
    }
}

fn validate_history_session_id(session_id: &str) -> Result<()> {
    if session_id.trim().is_empty()
        || session_id.len() > 512
        || session_id.chars().any(char::is_control)
    {
        return Err(Error::Tool("history session_id must be 1–512 bytes".into()));
    }
    Ok(())
}

fn validate_history_sequence(sequence: u64) -> Result<()> {
    if i64::try_from(sequence).is_err() {
        return Err(Error::Tool(
            "history sequence exceeds the supported range".into(),
        ));
    }
    Ok(())
}

fn scan_history_batch(
    batch: &crate::backend::checkpoint::TranscriptBatch,
    session_id: &Arc<str>,
    cursor: &mut HistoryCursor,
    documents: &mut Vec<HistoryDocument>,
    scanned_items: &mut usize,
    scanned_chars: &mut usize,
) -> Result<()> {
    let mut remaining = cursor.remaining_items.unwrap_or(batch.items.len());
    if remaining > batch.items.len()
        || (cursor.remaining_items.is_some()
            && cursor.before_sequence != batch.sequence.checked_add(1))
    {
        return Err(Error::Tool(
            "history cursor no longer identifies a transcript item".into(),
        ));
    }
    while remaining > 0
        && *scanned_items < MAX_HISTORY_ITEMS
        && MAX_HISTORY_SCAN_CHARS - *scanned_chars > MAX_HISTORY_QUERY_BYTES
    {
        *scanned_items += 1;
        let mut next_offset = None;
        if let Some((kind, text)) = history_text(&batch.items[remaining - 1]) {
            let limit = HISTORY_CHUNK_CHARS.min(MAX_HISTORY_SCAN_CHARS - *scanned_chars);
            let (chunk, next) = history_chunk(&text, cursor.offset, limit)?;
            *scanned_chars += chunk.chars().count();
            documents.push(HistoryDocument {
                session_id: Arc::clone(session_id),
                target: MessageTarget {
                    checkpoint_sequence: batch.sequence,
                    batch_item_count: remaining,
                },
                kind,
                offset: cursor.offset,
                text: chunk,
            });
            // Overlap the query length so chunk boundaries cannot hide a literal search phrase.
            next_offset = next.map(|offset| offset - cursor.query.chars().count());
        }
        if let Some(offset) = next_offset {
            cursor.offset = offset;
        } else {
            remaining -= 1;
            cursor.offset = 0;
        }
    }
    cursor.before_sequence = Some(if remaining == 0 {
        batch.sequence
    } else {
        batch
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::Tool("history sequence is too large".into()))?
    });
    cursor.remaining_items = (remaining > 0).then_some(remaining);
    Ok(())
}

fn history_text(item: &Value) -> Option<(&'static str, Cow<'_, str>)> {
    if let Some(message) = crate::protocol::message_metadata(item) {
        return Some(match message.author {
            crate::protocol::MessageAuthor::User => {
                let mut text = message.text;
                for file in message.attachments {
                    let reference = crate::protocol::content_part_text(
                        &serde_json::json!({"type": "file", "file": file}),
                    )?;
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&reference);
                }
                ("user", Cow::Owned(text))
            }
            crate::protocol::MessageAuthor::Source { source, handle, .. } => (
                "assistant",
                Cow::Owned(format!("@{handle} ({}): {}", source.id(), message.text)),
            ),
        });
    }
    if crate::protocol::is_internal_message(item) {
        let attachments = crate::protocol::content_parts(item)?
            .iter()
            .filter(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("input_image" | "file")
                )
            })
            .filter_map(crate::protocol::content_part_text)
            .collect::<Vec<_>>();
        return (!attachments.is_empty()).then(|| ("user", Cow::Owned(attachments.join("\n"))));
    }
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => Some((
            "tool_call",
            Cow::Owned(format!(
                "{}\n{}",
                item.get("name")?.as_str()?,
                history_value_text(item.get("arguments")?)
            )),
        )),
        Some("function_call_output") => Some((
            "tool_result",
            Cow::Owned(
                item.get("output")?
                    .as_array()?
                    .iter()
                    .filter_map(crate::protocol::content_part_text)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        )),
        Some("reasoning" | "compaction") => None,
        _ => {
            let kind = match item.get("role")?.as_str()? {
                "user" => "user",
                "assistant" => "assistant",
                _ => return None,
            };
            let content = item.get("content")?;
            let text = match content {
                Value::String(text) => Cow::Borrowed(text.as_str()),
                Value::Array(parts) => Cow::Owned(
                    parts
                        .iter()
                        .filter_map(crate::protocol::content_part_text)
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => return None,
            };
            Some((kind, text))
        }
    }
}

fn history_value_text(value: &Value) -> Cow<'_, str> {
    value
        .as_str()
        .map_or_else(|| Cow::Owned(value.to_string()), Cow::Borrowed)
}

fn history_chunk(text: &str, offset: usize, max_chars: usize) -> Result<(String, Option<usize>)> {
    let start = text
        .char_indices()
        .map(|(index, _)| index)
        .nth(offset)
        .or_else(|| (text.chars().count() == offset).then_some(text.len()))
        .ok_or_else(|| Error::Tool("history offset exceeds the item length".into()))?;
    let mut chars = text[start..].chars();
    let content = chars.by_ref().take(max_chars).collect::<String>();
    let next = chars.next().is_some().then(|| offset + max_chars);
    Ok((content, next))
}

fn history_excerpt(text: &str, query: &str) -> (usize, String) {
    let lower = text.to_ascii_lowercase();
    let query = query.to_ascii_lowercase();
    let found = lower
        .find(&query)
        .or_else(|| query.split_whitespace().find_map(|term| lower.find(term)));
    let offset = found.map_or(0, |index| text[..index].chars().count().saturating_sub(100));
    (
        offset,
        text.chars()
            .skip(offset)
            .take(HISTORY_EXCERPT_CHARS)
            .collect(),
    )
}

async fn fork(
    context: MiddlewareCommandContext<'_>,
    files: Option<&crate::backend::session_files::SessionFileStore>,
    page_size: usize,
) -> Result<MiddlewareCommandOutput> {
    if !context.arguments.trim().is_empty() {
        return Ok(MiddlewareCommandOutput::render(
            MANIFEST.id,
            text::DEFINITION.message_fork_usage.as_str(),
            FrontendTone::Warning,
        ));
    }
    let target = context.target;
    let through_sequence = target
        .as_ref()
        .map_or(context.checkpoint.sequence, |target| {
            target.checkpoint_sequence
        });
    let items = transcript_items_through(&context, through_sequence, page_size).await?;
    let Some(target) = target else {
        let options = fork_options(&items, context.session_id);
        if options.is_empty() {
            return Ok(MiddlewareCommandOutput::render(
                MANIFEST.id,
                text::DEFINITION.message_no_fork_messages.as_str(),
                FrontendTone::Neutral,
            ));
        }
        return Ok(MiddlewareCommandOutput::events(vec![
            FrontendEvent::Picker {
                title: text::DEFINITION.picker_fork_chat_from_message.clone(),
                options,
            },
        ]));
    };
    let transcript = fork_prefix(items, &target, context.session_id)?;
    let checkpoint = manual_fork_checkpoint(context.checkpoint, transcript);
    crate::backend::session_files::grant_context(
        files,
        context.session_id,
        &checkpoint.session_id,
        &checkpoint.context,
    )
    .await?;
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
        MANIFEST.id,
        text::DEFINITION
            .message_forked
            .replace("{id}", compact_id(&checkpoint.session_id)),
        FrontendTone::Success,
    ))
}

async fn transcript_items_through(
    context: &MiddlewareCommandContext<'_>,
    through_sequence: u64,
    page_size: usize,
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
                    max_batches: page_size,
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
                    text::DEFINITION.picker_user_message.as_str(),
                    message.text,
                    message.message_target?,
                ),
                EventMsg::AssistantMessage(message) => (
                    text::DEFINITION.picker_assistant_message.as_str(),
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
                    command: Command::Fork.as_str().into(),
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

fn manual_fork_checkpoint(parent: &Checkpoint, context: Vec<serde_json::Value>) -> Checkpoint {
    let mut checkpoint = Checkpoint::empty(Uuid::new_v4().to_string());
    let mut context = context.into_iter().map(Arc::new).collect();
    strip_attachment_references(&mut context);
    checkpoint.context = Arc::new(context);
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
                    MANIFEST.id,
                    text::DEFINITION.message_resume_usage.as_str(),
                    FrontendTone::Warning,
                ));
            }
        }
    };
    let options = resume_options(&context, cursor, page_size).await?;
    if options.is_empty() {
        return Ok(MiddlewareCommandOutput::render(
            MANIFEST.id,
            text::DEFINITION.message_no_saved_chats.as_str(),
            FrontendTone::Neutral,
        ));
    }
    Ok(MiddlewareCommandOutput::events(vec![
        FrontendEvent::Picker {
            title: text::DEFINITION.picker_resume_chat.clone(),
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
            owner_id: None,
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
            label: text::DEFINITION.widget_more_chats.clone(),
            description: String::new(),
            detail: String::new(),
            symbol: None,
            shows_detail: false,
            op: Op::command(
                MANIFEST.id,
                Command::Resume.as_str(),
                serde_json::to_string(&cursor)?,
            ),
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
            if session.parent_session_id.is_some() {
                &text::DEFINITION.picker_fork_label
            } else {
                &text::DEFINITION.picker_chat_label
            }
            .replace("{id}", compact_id(&session.session_id))
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
    let created = text::DEFINITION
        .picker_created_at
        .replace("{time}", &session.created_at.to_string());
    [
        session.session_context.workspace_label.as_deref(),
        session.session_context.origin_label.as_deref(),
        Some(created.as_str()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_local_defaults_preserve_catalog_paging() {
        assert_eq!(
            super::super::manifest::integer_default(&text::DEFINITION.settings, "page_size"),
            100
        );
        assert!(MANIFEST.default_enabled);
    }

    #[tokio::test]
    async fn history_preserves_image_references_and_finds_masked_internal_attachments() {
        let image = serde_json::json!({"type":"input_image","image":{"file":{"id":"screen-id","name":"screen.png","size":100,"media_type":"image/png"},"width":10,"height":10,"detail":"high"}});
        let result = serde_json::json!({"type":"function_call_output","output":[{"type":"input_text","text":"before"},image,{"type":"input_text","text":"after"}]});
        let (_, text) = history_text(&result).expect("tool history");
        assert!(text.starts_with("before\nImage:"));
        assert!(text.contains("screen-id"));
        assert!(text.ends_with("\nafter"));
        let mut materialization =
            crate::backend::model::internal_user_message("attachments", "private instructions");
        materialization["content"]
            .as_array_mut()
            .expect("content")
            .push(image);
        let (_, text) = history_text(&materialization).expect("image history");
        assert!(text.contains("screen-id"));
        assert!(!text.contains("private instructions"));

        materialization["content"][1] = serde_json::json!({
            "type": "file", "file": materialization["content"][1]["image"]["file"]
        });
        let state = tempfile::tempdir().expect("state");
        let history = history_store(&state.path().join("history.sqlite3"));
        save_history(
            history.checkpoints.as_ref(),
            "current",
            "researcher",
            vec![materialization],
        )
        .await;
        let page: Value = serde_json::from_str(
            &history
                .search(search_args("screen-id", HistoryScope::Current, None))
                .await
                .expect("search masked attachment"),
        )
        .expect("search page");
        let excerpt = page["hits"][0]["excerpt"].as_str().expect("search hit");
        assert!(excerpt.starts_with("Stored file:"));
        assert!(excerpt.contains("screen-id"));
        assert!(!excerpt.contains("private instructions"));
    }

    #[tokio::test]
    async fn history_finds_attachment_names_and_ids_in_saved_user_messages() {
        let message = crate::backend::model::message_input(&crate::protocol::MessageEvent {
            author: crate::protocol::MessageAuthor::User,
            delivery: crate::protocol::MessageDelivery::Turn,
            text: "Inspect this receipt.".into(),
            attachments: vec![crate::protocol::SessionFileReference {
                id: "upload-id".into(),
                name: "receipt.png".into(),
                size: 100,
                media_type: "image/png".into(),
            }],
            reply: None,
            message_target: None,
        })
        .expect("ordinary user message");
        let state = tempfile::tempdir().expect("state");
        let history = history_store(&state.path().join("history.sqlite3"));
        save_history(
            history.checkpoints.as_ref(),
            "current",
            "researcher",
            vec![message],
        )
        .await;
        for query in ["upload-id", "receipt.png"] {
            let page: Value = serde_json::from_str(
                &history
                    .search(search_args(query, HistoryScope::Current, None))
                    .await
                    .expect("search uploaded attachment"),
            )
            .expect("search page");
            let excerpt = page["hits"][0]["excerpt"].as_str().expect("attachment hit");
            assert!(excerpt.starts_with("Inspect this receipt.\nStored file:"));
            assert!(excerpt.contains("upload-id"));
            assert!(excerpt.contains("receipt.png"));
        }
    }

    #[test]
    fn history_preserves_peer_identity_as_assistant_evidence() {
        let item = crate::backend::model::message_input(&crate::protocol::MessageEvent {
            author: crate::protocol::MessageAuthor::Source {
                message_id: "message-1".into(),
                source: crate::protocol::MessageSource::Session {
                    session_id: "session-reviewer".into(),
                },
                cause_id: None,
                ancestry: Vec::new(),
                handle: "reviewer".into(),
                symbol: None,
            },
            delivery: crate::protocol::MessageDelivery::Steer,
            text: "Keep the boundary.".into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        })
        .expect("peer message");

        assert_eq!(
            history_text(&item),
            Some((
                "assistant",
                "@reviewer (session-reviewer): Keep the boundary.".into()
            ))
        );
    }

    fn direct_tool_names(sessions: &Sessions, role: crate::agent::AgentRole) -> Vec<String> {
        let state = tempfile::tempdir().expect("state");
        let checkpoints: Arc<dyn crate::backend::checkpoint::CheckpointStore> = Arc::new(
            crate::backend::checkpoint::sqlite::SqliteCheckpoint::new(
                state.path().join("checkpoints.sqlite3"),
            )
            .expect("checkpoint store"),
        );
        let runtime = RuntimeContext {
            children: crate::agent::ChildAgents::default(),
            sender: crate::agent::test_sender(),
            checkpoints,
            session_id: "session".into(),
            model_route: "model".into(),
            model: "model".into(),
            approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
            session_context: crate::protocol::SessionContext {
                owner_id: "bot-1".into(),
                ..crate::protocol::SessionContext::default()
            },
            metadata: std::collections::BTreeMap::new(),
            role,
            frontend: Arc::new(|_| Ok(())),
        };
        let mut catalog = Catalog::default();
        sessions
            .register(&mut catalog, &runtime)
            .expect("register session tools");

        catalog.finalize().expect("finalize tools");
        catalog
            .direct_definitions()
            .iter()
            .map(|tool| tool.name.as_str().into())
            .collect()
    }

    #[test]
    fn history_tools_are_directly_available_for_the_required_owner() {
        assert_eq!(
            direct_tool_names(
                &Sessions::new(100, None, None).expect("sessions"),
                crate::agent::AgentRole::Main
            ),
            ["read_history", "search_history"]
        );
    }

    struct NoChats;

    #[test]
    fn chat_commands_require_one_destination_and_one_operation() {
        for arguments in [
            serde_json::json!({"target":"chat", "text":"Continue"}),
            serde_json::json!({"workspace":"/project", "text":"Start", "delivery":"queue"}),
            serde_json::json!({"target":"chat", "interrupt_turn_id":"turn"}),
        ] {
            let mut args: MessageChatArgs = serde_json::from_value(arguments).unwrap();
            assert!(args.command().is_ok());
        }
        for arguments in [
            serde_json::json!({"text":"Missing destination"}),
            serde_json::json!({"target":"chat", "workspace":"/project", "text":"Ambiguous"}),
            serde_json::json!({"target":"chat"}),
            serde_json::json!({"target":"chat", "text":"Ambiguous", "interrupt_turn_id":"turn"}),
            serde_json::json!({"workspace":"/project", "interrupt_turn_id":"turn"}),
            serde_json::json!({"target":"chat", "interrupt_turn_id":"turn", "delivery":"queue"}),
        ] {
            let mut args: MessageChatArgs = serde_json::from_value(arguments).unwrap();
            assert!(args.command().is_err());
        }
    }

    impl LiveChats for NoChats {
        fn list<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Result<Vec<LiveChat>>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn send<'a>(
            &'a self,
            _session_id: &'a str,
            _command: PeerCommand<'a>,
            _initiating_author: &'a crate::protocol::MessageAuthor,
            _command_id: &'a str,
        ) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { Ok("target".into()) })
        }
    }

    #[test]
    fn live_chat_tools_are_offered_only_to_main_chats_with_a_host() {
        let sessions = Sessions::new(100, None, Some(Arc::new(NoChats))).expect("sessions");
        let subagent = crate::agent::AgentRole::Subagent {
            parent_session_id: "session".into(),
            parent_turn_id: "turn".into(),
        };

        assert_eq!(
            direct_tool_names(&sessions, crate::agent::AgentRole::Main),
            [
                "list_chats",
                "message_chat",
                "read_history",
                "search_history"
            ]
        );
        assert_eq!(
            direct_tool_names(&sessions, subagent),
            ["read_history", "search_history"]
        );
    }

    #[test]
    fn live_chat_handle_carries_the_id_it_answers_to() {
        let mut chat = LiveChat {
            session_id: "3f2a91c0-0000-4000-8000-000000000000".into(),
            title: None,
            workspace: None,
            running: false,
            turn_id: None,
        };
        assert_eq!(chat.handle(), "chat #3f2a91c0");
        chat.title = Some(format!("Backend {}", "é".repeat(40)));
        assert!(chat.handle().ends_with(" #3f2a91c0"));
        assert!(chat.handle().len() <= MAX_HANDLE_TITLE_BYTES + " #3f2a91c0".len());

        assert!(chat.is_target("#3f2a91c0"));
        assert!(chat.is_target("3f2a91c0"));
        assert!(chat.is_target("3f2a91c0-0000-4000-8000-000000000000"));
        assert!(!chat.is_target("3f2a91c"));
        assert!(!chat.is_target("#9b1c77d2"));
    }

    async fn save_history(
        checkpoints: &dyn crate::backend::checkpoint::CheckpointStore,
        session_id: &str,
        owner_id: &str,
        items: Vec<Value>,
    ) -> Checkpoint {
        let mut checkpoint = Checkpoint::empty(session_id);
        checkpoint.sequence = 1;
        checkpoint.session_context.owner_id = owner_id.into();
        let items = items.into_iter().map(Arc::new).collect::<Vec<_>>();
        checkpoint.context = Arc::new(items);
        checkpoints
            .save(&checkpoint, &checkpoint.context, None)
            .await
            .expect("save history");
        checkpoint
    }

    fn history_store(path: &std::path::Path) -> History {
        History {
            checkpoints: Arc::new(
                crate::backend::checkpoint::sqlite::SqliteCheckpoint::new(path).expect("store"),
            ),
            session_id: "current".into(),
            owner_id: "researcher".into(),
        }
    }

    fn search_args(query: &str, scope: HistoryScope, cursor: Option<String>) -> SearchHistoryArgs {
        SearchHistoryArgs {
            query: query.into(),
            scope,
            cursor,
        }
    }

    #[tokio::test]
    async fn history_recovers_offloaded_tool_output_beyond_the_first_page() {
        let state = tempfile::tempdir().expect("state");
        let history = history_store(&state.path().join("history.sqlite3"));
        let output = format!("{}\nneedle exact result\n", "🦀".repeat(9_000));
        let mut checkpoint = save_history(history.checkpoints.as_ref(), "current", "researcher", vec![
            crate::backend::model::user_message("Investigate"),
            serde_json::json!({"type":"function_call", "call_id":"call-1", "name":"read_file", "arguments":"{\"path\":\"earlier.rs\"}"}),
            crate::backend::model::tool_output("call-1", &output.as_str().into(), false),
        ]).await;
        Arc::make_mut(&mut Arc::make_mut(&mut checkpoint.context)[2])["output"] =
            serde_json::json!([{"type":"input_text", "text":"[offloaded]"}]);
        checkpoint.context_epoch += 1;
        for sequence in 2..=70 {
            checkpoint.sequence = sequence;
            history
                .checkpoints
                .save(
                    &checkpoint,
                    &[
                        serde_json::json!({"role":"assistant", "content":"newer unrelated work"})
                            .into(),
                    ],
                    None,
                )
                .await
                .expect("save later batch");
        }
        let mut cursor = None;
        let mut pages = 0;
        let hit = loop {
            let page: Value = serde_json::from_str(
                &history
                    .search(search_args("needle", HistoryScope::Current, cursor))
                    .await
                    .expect("search"),
            )
            .expect("search page");
            pages += 1;
            if let Some(hit) = page["hits"].as_array().expect("hits").first() {
                break hit.clone();
            }
            assert!(pages < 10, "history scan must make progress");
            cursor = Some(
                page["next_cursor"]
                    .as_str()
                    .expect("older history cursor")
                    .into(),
            );
        };
        assert!(pages > 1);
        assert_eq!(hit["session_id"], "current");
        assert_eq!(hit["kind"], "tool_result");
        assert!(
            hit["excerpt"]
                .as_str()
                .expect("excerpt")
                .contains("needle exact result")
        );
        let target: MessageTarget = serde_json::from_value(hit["target"].clone()).expect("target");
        assert_eq!(
            target,
            MessageTarget {
                checkpoint_sequence: 1,
                batch_item_count: 3
            }
        );
        let mut restored = String::new();
        let mut offset = 0;
        loop {
            let page: Value = serde_json::from_str(
                &history
                    .read(ReadHistoryArgs {
                        session_id: None,
                        target,
                        offset,
                        max_chars: 4_000,
                    })
                    .await
                    .expect("read history"),
            )
            .expect("read page");
            restored.push_str(page["text"].as_str().expect("text"));
            let Some(next) = page["next_offset"].as_u64() else {
                break;
            };
            offset = usize::try_from(next).expect("offset");
        }
        assert_eq!(restored, output);
        let calls: Value = serde_json::from_str(
            &history
                .read(ReadHistoryArgs {
                    session_id: None,
                    target: MessageTarget {
                        checkpoint_sequence: 1,
                        batch_item_count: 2,
                    },
                    offset: 0,
                    max_chars: 4_000,
                })
                .await
                .expect("read call"),
        )
        .expect("call");
        assert_eq!(calls["text"], "read_file\n{\"path\":\"earlier.rs\"}");
    }

    #[tokio::test]
    async fn history_cursor_continues_inside_a_large_message() {
        let state = tempfile::tempdir().expect("state");
        let history = history_store(&state.path().join("history.sqlite3"));
        save_history(
            history.checkpoints.as_ref(),
            "current",
            "researcher",
            vec![crate::backend::model::user_message(&format!(
                "{}needle after the scan limit",
                "padding ".repeat(10_000)
            ))],
        )
        .await;
        let first: Value = serde_json::from_str(
            &history
                .search(search_args("needle", HistoryScope::Current, None))
                .await
                .expect("first page"),
        )
        .expect("first page json");
        assert!(first["hits"].as_array().expect("hits").is_empty());
        assert!(first["scanned_chars"].as_u64().expect("scan bound") <= 64_000);
        let second: Value = serde_json::from_str(
            &history
                .search(search_args(
                    "needle",
                    HistoryScope::Current,
                    Some(first["next_cursor"].as_str().expect("cursor").into()),
                ))
                .await
                .expect("second page"),
        )
        .expect("second page json");
        assert!(
            second["hits"][0]["excerpt"]
                .as_str()
                .expect("late match")
                .contains("needle after the scan limit")
        );
    }

    #[tokio::test]
    async fn history_requires_explicit_other_chat_scope_and_rejects_foreign_owner_reads() {
        let state = tempfile::tempdir().expect("state");
        let history = history_store(&state.path().join("history.sqlite3"));
        for (session, owner) in [
            ("current", "researcher"),
            ("prior", "researcher"),
            ("private", "writer"),
        ] {
            save_history(
                history.checkpoints.as_ref(),
                session,
                owner,
                vec![crate::backend::model::user_message(&format!(
                    "needle in {session}"
                ))],
            )
            .await;
        }
        for (scope, expected) in [
            (HistoryScope::Current, "current"),
            (HistoryScope::OtherChats, "prior"),
        ] {
            let result: Value = serde_json::from_str(
                &history
                    .search(search_args("needle", scope, None))
                    .await
                    .expect("search"),
            )
            .expect("result");
            let hits = result["hits"].as_array().expect("hits");
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0]["session_id"], expected);
        }
        let target = MessageTarget {
            checkpoint_sequence: 1,
            batch_item_count: 1,
        };
        assert!(
            history
                .read(ReadHistoryArgs {
                    session_id: Some("prior".into()),
                    target,
                    offset: 0,
                    max_chars: 100
                })
                .await
                .is_ok()
        );
        assert!(
            history
                .read(ReadHistoryArgs {
                    session_id: Some("private".into()),
                    target,
                    offset: 0,
                    max_chars: 100
                })
                .await
                .is_err()
        );
        let mut forged = history
            .cursor(search_args("needle", HistoryScope::OtherChats, None))
            .expect("cursor");
        forged.session_id = Some("private".into());
        assert!(
            history
                .search(search_args(
                    "needle",
                    HistoryScope::OtherChats,
                    Some(serde_json::to_string(&forged).expect("cursor json"))
                ))
                .await
                .is_err()
        );
        forged.session_id = Some("prior".into());
        assert!(
            history
                .search(search_args(
                    "needle",
                    HistoryScope::Current,
                    Some(serde_json::to_string(&forged).expect("cursor json"))
                ))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn history_validates_limits_targets_and_hides_private_context() {
        let state = tempfile::tempdir().expect("state");
        let history = history_store(&state.path().join("history.sqlite3"));
        save_history(history.checkpoints.as_ref(), "current", "researcher", vec![
            serde_json::json!({"role":"assistant", "content":"visible", "encrypted_content":"hidden", "_mobius_reasoning":"hidden"}),
            serde_json::json!({"type":"reasoning", "encrypted_content":"secret"}),
            crate::backend::model::internal_user_message("private", "hidden"),
        ]).await;
        let target = MessageTarget {
            checkpoint_sequence: 1,
            batch_item_count: 1,
        };
        for (offset, max_chars) in [(0, 0), (0, MAX_HISTORY_READ_CHARS + 1), (usize::MAX, 10)] {
            assert!(
                history
                    .read(ReadHistoryArgs {
                        session_id: None,
                        target,
                        offset,
                        max_chars
                    })
                    .await
                    .is_err()
            );
        }
        for batch_item_count in [0, 2, 3, 4] {
            assert!(
                history
                    .read(ReadHistoryArgs {
                        session_id: None,
                        target: MessageTarget {
                            batch_item_count,
                            ..target
                        },
                        offset: 0,
                        max_chars: 100,
                    })
                    .await
                    .is_err()
            );
        }
        assert!(
            history
                .cursor(search_args("", HistoryScope::Current, None))
                .is_err()
        );
        assert!(
            history
                .cursor(search_args(
                    &"a".repeat(MAX_HISTORY_QUERY_BYTES + 1),
                    HistoryScope::Current,
                    None
                ))
                .is_err()
        );
        assert!(
            history
                .cursor(search_args(
                    "needle",
                    HistoryScope::Current,
                    Some("x".repeat(MAX_HISTORY_CURSOR_BYTES + 1))
                ))
                .is_err()
        );
        let result = history
            .read(ReadHistoryArgs {
                session_id: None,
                target,
                offset: 0,
                max_chars: 100,
            })
            .await
            .expect("visible text");
        assert!(result.contains("visible"));
        assert!(!result.contains("hidden"));
    }

    #[test]
    fn sessions_rejects_page_sizes_outside_its_manifest_bounds() {
        assert!(Sessions::new(0, None, None).is_err());
        assert!(Sessions::new(page_size_bounds().1 + 1, None, None).is_err());
    }

    #[test]
    fn fork_is_exposed_as_a_generic_message_action() {
        let contribution = Sessions::new(100, None, None)
            .expect("sessions")
            .frontend("session");
        let widget = contribution.widgets.first().expect("fork widget");

        assert_eq!(widget.slot, FrontendSlot::MessageActions);
        assert_eq!(widget.text, "Fork chat");
        assert_eq!(widget.symbol, Some(FrontendSymbol::Branch));
        assert_eq!(
            widget.action,
            Some(Op::command("sessions", "fork", String::new()))
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
                reply: None,
                message_target: None,
            })
            .expect("message input"),
            serde_json::json!({"type": "function_call", "call_id": "call-1", "name": "read"}),
            serde_json::json!({"type": "function_call_output", "call_id": "call-1", "output": [{"type": "input_text", "text": "done"}]}),
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
    fn history_arguments_borrow_strings_and_format_json_values() {
        let arguments = Value::String("large tool arguments".into());
        assert!(matches!(history_value_text(&arguments), Cow::Borrowed(_)));
        assert_eq!(
            history_value_text(&serde_json::json!({"n": 1})),
            "{\"n\":1}"
        );
        assert_eq!(
            history_text(&serde_json::json!({
                "type": "function_call", "name": "tool", "arguments": "raw"
            })),
            Some(("tool_call", Cow::Owned("tool\nraw".into())))
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

        let mut described = summary("branch-id", Some("parent"));
        assert_eq!(session_description(&described), "created at Unix time 0");
        described.session_context.workspace_label = Some(String::new());
        described.session_context.origin_label = Some("origin".into());
        assert_eq!(
            session_description(&described),
            " · origin · created at Unix time 0"
        );

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
        parent.context = std::sync::Arc::new(
            vec![serde_json::json!({
                "role": "user",
                "content": "Hello",
                "_mobius_attachments": [{
                    "id": "378b8581-e96c-4413-a138-93e74561cb87",
                    "name": "photo.png",
                    "size": 1,
                    "media_type": "image/png"
                }]
            })]
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
        );
        parent.first_user_message = Some("Hello".into());
        parent.metadata.insert(
            "gateway.chat".into(),
            serde_json::json!({"workspace": "/srv/project"}),
        );
        parent.session_context = crate::protocol::SessionContext {
            owner_id: "bot-1".into(),
            workspace_id: Some("workspace-1".into()),
            workspace_label: Some("Project One".into()),
            origin_label: Some("routine".into()),
            ..crate::protocol::SessionContext::default()
        };

        let fork = manual_fork_checkpoint(
            &parent,
            parent
                .context
                .iter()
                .map(|item| item.as_ref().to_owned())
                .collect(),
        );

        assert!(fork.context[0].get("_mobius_attachments").is_none());
        assert_eq!(fork.first_user_message, parent.first_user_message);
        assert_eq!(fork.metadata, parent.metadata);
        assert_eq!(
            fork.session_context,
            crate::protocol::SessionContext {
                owner_id: "bot-1".into(),
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

    struct TranscriptPageProbe(std::sync::Mutex<Vec<usize>>);

    impl crate::backend::checkpoint::CheckpointStore for TranscriptPageProbe {
        fn load<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Option<Checkpoint>>> {
            unreachable!()
        }
        fn message_accepted<'a>(&'a self, _: &'a str, _: &'a str) -> BoxFuture<'a, Result<bool>> {
            unreachable!()
        }
        fn delete_sessions<'a>(&'a self, _: &'a [String]) -> BoxFuture<'a, Result<bool>> {
            unreachable!()
        }
        fn save<'a>(
            &'a self,
            _: &'a Checkpoint,
            _: &'a [Arc<Value>],
            _: Option<&'a crate::backend::checkpoint::ExecutionRecord>,
        ) -> BoxFuture<'a, Result<()>> {
            unreachable!()
        }
        fn save_with_events<'a>(
            &'a self,
            _: Arc<Checkpoint>,
            _: Vec<Arc<Value>>,
            _: Option<crate::backend::checkpoint::ExecutionRecord>,
            _: Vec<crate::backend::checkpoint::TimestampedEvent>,
        ) -> BoxFuture<'a, Result<Vec<crate::backend::checkpoint::JournalEvent>>> {
            unreachable!()
        }
        fn append_event<'a>(
            &'a self,
            _: &'a str,
            _: i64,
            _: &'a crate::protocol::Event,
        ) -> BoxFuture<'a, Result<crate::backend::checkpoint::JournalEvent>> {
            unreachable!()
        }
        fn event_page<'a>(
            &'a self,
            _: &'a str,
            _: crate::backend::checkpoint::EventPageRequest,
        ) -> BoxFuture<'a, Result<crate::backend::checkpoint::EventPage>> {
            unreachable!()
        }
        fn transcript_page<'a>(
            &'a self,
            _: &'a str,
            request: TranscriptPageRequest,
        ) -> BoxFuture<'a, Result<crate::backend::checkpoint::TranscriptPage>> {
            self.0.lock().expect("probe").push(request.max_batches);
            Box::pin(async {
                Ok(crate::backend::checkpoint::TranscriptPage {
                    batches: Vec::new(),
                    next_before_sequence: None,
                })
            })
        }
        fn load_state<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Option<Value>>> {
            unreachable!()
        }
        fn save_state<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a Value,
        ) -> BoxFuture<'a, Result<()>> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn fork_reads_transcript_pages_of_the_configured_size() {
        let probe = Arc::new(TranscriptPageProbe(std::sync::Mutex::default()));
        let checkpoint = Checkpoint::empty("session");
        let context = crate::protocol::SessionContext::default();
        Sessions::new(7, None, None)
            .expect("sessions")
            .command(MiddlewareCommandContext {
                command: "fork",
                arguments: "",
                input: None,
                target: None,
                session_id: "session",
                session_context: &context,
                checkpoint: &checkpoint,
                checkpoints: probe.clone(),
            })
            .await
            .expect("fork picker");
        assert_eq!(*probe.0.lock().expect("probe"), vec![7]);
    }
}
