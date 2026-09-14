//! Frontend-neutral event payload records.

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;

use super::EventMsg;
use super::MessageAuthor;
use super::SessionContext;
use super::SessionFileReference;
use super::WebSearchAction;
use super::required_option;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for error event.
pub struct ErrorEvent {
    /// The kind.
    pub kind: ErrorKind,
    /// The message.
    pub message: String,
    /// The retryable.
    pub retryable: bool,
    /// The status.
    pub status: Option<u16>,
    /// The retry after.
    pub retry_after: Option<String>,
}

impl ErrorEvent {
    pub(crate) fn from_error(error: &crate::Error) -> Self {
        let (kind, retryable, status, retry_after) = match error {
            crate::Error::Config(_) => (ErrorKind::Configuration, false, None, None),
            crate::Error::Duplicate(_) => (ErrorKind::DuplicateRegistration, false, None, None),
            crate::Error::Unknown(_) => (ErrorKind::UnknownRegistration, false, None, None),
            crate::Error::Provider(error) => (
                ErrorKind::Provider,
                error.is_retryable(),
                error.status(),
                error.retry_after().map(str::to_owned),
            ),
            crate::Error::Auth(_) => (ErrorKind::Authentication, false, None, None),
            crate::Error::Sandbox(_) => (ErrorKind::Sandbox, false, None, None),
            crate::Error::Tool(_) => (ErrorKind::Tool, false, None, None),
            crate::Error::Checkpoint(_) => (ErrorKind::Checkpoint, false, None, None),
            crate::Error::Busy(_) => (ErrorKind::Busy, false, None, None),
            crate::Error::Stopped(_) => (ErrorKind::Stopped, false, None, None),
            crate::Error::Rollback { .. } => (ErrorKind::Rollback, false, None, None),
            crate::Error::Io(_) => (ErrorKind::Io, false, None, None),
            crate::Error::Http(_) => (ErrorKind::Http, false, None, None),
            crate::Error::Json(_) => (ErrorKind::Json, false, None, None),
            crate::Error::Sqlite(_) => (ErrorKind::Storage, false, None, None),
        };
        Self {
            kind,
            message: error.to_string(),
            retryable,
            status,
            retry_after,
        }
    }
}

/// Stable frontend classification for framework failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Selects the configuration case.
    Configuration,
    /// Selects the duplicate registration case.
    DuplicateRegistration,
    /// Selects the unknown registration case.
    UnknownRegistration,
    /// Selects the provider case.
    Provider,
    /// Selects the authentication case.
    Authentication,
    /// Selects the sandbox case.
    Sandbox,
    /// Selects the tool case.
    Tool,
    /// Selects the checkpoint case.
    Checkpoint,
    /// Selects the busy case.
    Busy,
    /// Selects the stopped case.
    Stopped,
    /// Selects the rollback case.
    Rollback,
    /// Selects the I/O case.
    Io,
    /// Selects the HTTP case.
    Http,
    /// Selects the JSON case.
    Json,
    /// Selects the storage case.
    Storage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for warning event.
pub struct WarningEvent {
    /// The message.
    pub message: String,
}

/// A submission the agent rejected without changing durable turn state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionRejectedEvent {
    /// The message.
    pub message: String,
}

/// Immutable session data emitted once when an agent starts or resumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfiguredEvent {
    /// The session identifier.
    pub session_id: String,
    /// The context.
    pub context: SessionContext,
    /// The model.
    pub model: ModelChangedEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for turn started event.
pub struct TurnStartedEvent {
    /// The turn identifier.
    pub turn_id: String,
    /// The model context window.
    pub model_context_window: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for turn complete event.
pub struct TurnCompleteEvent {
    /// The turn identifier.
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for turn aborted event.
pub struct TurnAbortedEvent {
    /// The turn identifier.
    pub turn_id: String,
    /// The reason.
    pub reason: String,
}

/// Exact durable transcript prefix selected by a message action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageTarget {
    /// Durable checkpoint sequence containing the selected message.
    pub checkpoint_sequence: u64,
    /// One-based item count within the checkpoint's transcript batch.
    #[serde(deserialize_with = "positive_usize")]
    pub batch_item_count: usize,
}

/// Durable message identity and immutable text shown when replying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageReply {
    /// The target.
    pub target: MessageTarget,
    /// The text.
    pub text: String,
}

fn positive_usize<'de, D>(deserializer: D) -> std::result::Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    let value = usize::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "message target item count must be positive",
        ));
    }
    Ok(value)
}

/// How one accepted message actually entered the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageDelivery {
    /// Selects the turn case.
    Turn,
    /// Selects the steer case.
    Steer,
    /// Selects the queue case.
    Queue,
}

/// Incremental user text; the enclosing submission ID identifies the eventual message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDeltaEvent {
    /// The text.
    pub text: String,
}

/// One accepted conversation message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageEvent {
    /// The author.
    pub author: MessageAuthor,
    /// The delivery.
    pub delivery: MessageDelivery,
    /// The text.
    pub text: String,
    /// The attachments.
    pub attachments: Vec<SessionFileReference>,
    #[serde(default)]
    /// The reply.
    pub reply: Option<MessageReply>,
    #[serde(deserialize_with = "required_option")]
    /// The message target.
    pub message_target: Option<MessageTarget>,
}

impl MessageEvent {
    pub(crate) fn reply_text(&self) -> Option<String> {
        if !self.text.is_empty() {
            return Some(self.text.clone());
        }
        (!self.attachments.is_empty()).then(|| {
            self.attachments
                .iter()
                .map(|attachment| attachment.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for assistant message event.
pub struct AssistantMessageEvent {
    /// The session identifier.
    pub session_id: String,
    /// The turn identifier.
    pub turn_id: String,
    /// The model step identifier.
    pub model_step_id: String,
    /// The content.
    pub content: Vec<ModelStepContent>,
    #[serde(deserialize_with = "required_option")]
    /// The message target.
    pub message_target: Option<MessageTarget>,
}

impl AssistantMessageEvent {
    pub(crate) fn reply_text(&self) -> Option<String> {
        self.content
            .iter()
            .rev()
            .find(|item| item.phase != ModelStepContentPhase::Reasoning && !item.text.is_empty())
            .map(|item| item.text.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for assistant content delta event.
pub struct AssistantContentDeltaEvent {
    /// The session identifier.
    pub session_id: String,
    /// The turn identifier.
    pub turn_id: String,
    /// The model step identifier.
    pub model_step_id: String,
    /// The delta.
    pub delta: String,
    /// The phase.
    pub phase: ModelStepContentPhase,
}

/// One provider request becoming active within a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStepStartedEvent {
    /// The session identifier.
    pub session_id: String,
    /// The turn identifier.
    pub turn_id: String,
    /// The model step identifier.
    pub model_step_id: String,
    /// The step index.
    pub step_index: usize,
    /// The started at milliseconds.
    pub started_at_ms: i64,
}

/// The terminal record for one provider request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStepCompletedEvent {
    /// The session identifier.
    pub session_id: String,
    /// The turn identifier.
    pub turn_id: String,
    /// The model step identifier.
    pub model_step_id: String,
    /// The step index.
    pub step_index: usize,
    /// The started at milliseconds.
    pub started_at_ms: i64,
    /// The completed at milliseconds.
    pub completed_at_ms: i64,
    /// The outcome.
    pub outcome: ModelStepOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The diagnostics.
    pub diagnostics: Option<ModelStepDiagnostics>,
}

/// Provider-owned cost and prompt-cache observations for one completed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStepDiagnostics {
    /// The provider.
    pub provider: String,
    /// The prompt cache.
    pub prompt_cache: PromptCacheDiagnostics,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// The estimated cost microusd.
    pub estimated_cost_microusd: Option<u64>,
}

/// Prompt-cache behavior observed for one completed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheDiagnostics {
    /// The capability.
    pub capability: PromptCacheMode,
    /// The context epoch.
    pub context_epoch: u64,
    /// The outcome.
    pub outcome: PromptCacheOutcome,
    /// The rewrite reasons.
    pub rewrite_reasons: Vec<String>,
}

/// Provider-advertised prompt-cache behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheMode {
    /// Selects the unsupported case.
    Unsupported,
    /// Selects the implicit case.
    Implicit,
    /// Selects the explicit case.
    Explicit,
}

/// Cache result inferred from provider usage and local rewrite metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheOutcome {
    /// Selects the unsupported case.
    Unsupported,
    /// Selects the hit case.
    Hit,
    /// Selects the write case.
    Write,
    /// Selects the miss case.
    Miss,
    /// Selects the context rewrite case.
    ContextRewrite,
}

/// Provider-neutral outcome of a completed model step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ModelStepOutcome {
    /// Selects the completed case.
    Completed {
        /// The end turn.
        end_turn: bool,
        /// The tool call identifiers.
        tool_call_ids: Vec<String>,
        /// The usage.
        usage: TokenUsage,
    },
    /// Selects the failed case.
    Failed,
    /// Selects the interrupted case.
    Interrupted,
    /// The response stream failed and the logical step will restart with a fresh ID.
    Retrying,
}

/// One complete normalized text item produced by a model step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStepContent {
    /// The output index.
    pub output_index: usize,
    /// The part index.
    pub part_index: usize,
    /// The phase.
    pub phase: ModelStepContentPhase,
    /// The text.
    pub text: String,
    /// The annotations.
    pub annotations: Vec<ModelStepAnnotation>,
}

/// A provider-neutral annotation attached to one complete text part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelStepAnnotation {
    /// Selects the URL citation case.
    UrlCitation {
        /// The URL.
        url: String,
        /// The title.
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// The content.
        content: Option<String>,
        /// The start index.
        start_index: usize,
        /// The end index.
        end_index: usize,
    },
    /// Selects the file citation case.
    FileCitation {
        /// The file identifier.
        file_id: String,
        /// The filename.
        filename: String,
        /// The index.
        index: usize,
    },
    /// Selects the container file citation case.
    ContainerFileCitation {
        /// The container identifier.
        container_id: String,
        /// The file identifier.
        file_id: String,
        /// The filename.
        filename: String,
        /// The start index.
        start_index: usize,
        /// The end index.
        end_index: usize,
    },
    /// Selects the file path case.
    FilePath {
        /// The file identifier.
        file_id: String,
        /// The index.
        index: usize,
    },
    /// Selects the document character citation case.
    DocumentCharacterCitation {
        /// The cited text.
        cited_text: String,
        /// The document index.
        document_index: usize,
        /// The document title.
        document_title: Option<String>,
        /// The file identifier.
        file_id: Option<String>,
        /// The start char index.
        start_char_index: usize,
        /// The end char index.
        end_char_index: usize,
    },
    /// Selects the document page citation case.
    DocumentPageCitation {
        /// The cited text.
        cited_text: String,
        /// The document index.
        document_index: usize,
        /// The document title.
        document_title: Option<String>,
        /// The file identifier.
        file_id: Option<String>,
        /// The start page number.
        start_page_number: usize,
        /// The end page number.
        end_page_number: usize,
    },
    /// Selects the document content block citation case.
    DocumentContentBlockCitation {
        /// The cited text.
        cited_text: String,
        /// The document index.
        document_index: usize,
        /// The document title.
        document_title: Option<String>,
        /// The file identifier.
        file_id: Option<String>,
        /// The start block index.
        start_block_index: usize,
        /// The end block index.
        end_block_index: usize,
    },
    /// Selects the search result citation case.
    SearchResultCitation {
        /// The cited text.
        cited_text: String,
        /// The search result index.
        search_result_index: usize,
        /// The source.
        source: String,
        /// The title.
        title: Option<String>,
        /// The start block index.
        start_block_index: usize,
        /// The end block index.
        end_block_index: usize,
    },
    /// Selects the web search result citation case.
    WebSearchResultCitation {
        /// The cited text.
        cited_text: String,
        /// The encrypted index.
        encrypted_index: String,
        /// The title.
        title: Option<String>,
        /// The URL.
        url: String,
    },
}

/// Semantic role of text preserved in a completed model step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStepContentPhase {
    /// Selects the reasoning case.
    Reasoning,
    /// Selects the commentary case.
    Commentary,
    /// Selects the final answer case.
    FinalAnswer,
}

/// A restored transcript kept distinct from live turn lifecycle events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHistoryEvent {
    /// The events.
    pub events: Vec<EventMsg>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for model changed event.
pub struct ModelChangedEvent {
    /// The route.
    pub route: String,
    /// The model.
    pub model: String,
    /// The reasoning effort.
    pub reasoning_effort: Option<String>,
    /// The model context window.
    pub model_context_window: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for session resume requested event.
pub struct SessionResumeRequestedEvent {
    /// The session identifier.
    pub session_id: String,
    /// The context.
    pub context: SessionContext,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Data for tool call begin event.
pub struct ToolCallBeginEvent {
    /// The turn identifier.
    pub turn_id: String,
    /// The call identifier.
    pub call_id: String,
    /// The name.
    pub name: String,
    /// The arguments.
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for tool call end event.
pub struct ToolCallEndEvent {
    /// The turn identifier.
    pub turn_id: String,
    /// The call identifier.
    pub call_id: String,
    /// The name.
    pub name: String,
    /// The output.
    pub output: super::ToolContent,
    /// The is error.
    pub is_error: bool,
}

/// Deferred tool schemas materialized at one model-context position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLoadEvent {
    /// The turn identifier.
    pub turn_id: String,
    /// The load identifier.
    pub load_id: String,
    /// The catalog revision.
    pub catalog_revision: String,
    /// The tools.
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Data for exec approval request event.
pub struct ExecApprovalRequestEvent {
    /// The identifier.
    pub id: String,
    /// The turn identifier.
    pub turn_id: String,
    /// The calls.
    pub calls: Vec<ApprovalCall>,
    /// The reason.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Data for approval call.
pub struct ApprovalCall {
    /// The call identifier.
    pub call_id: String,
    /// The name.
    pub name: String,
    /// The arguments.
    pub arguments: serde_json::Value,
}

/// A user's decision for a paused tool batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    /// Selects the approved case.
    Approved,
    /// Selects the approved for session case.
    ApprovedForSession,
    /// The approval was denied.
    Denied {
        /// The denial reason.
        rejection: String,
    },
    /// Selects the abort case.
    Abort,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
/// Data for token usage.
pub struct TokenUsage {
    /// The input tokens.
    pub input_tokens: i64,
    /// The cached input tokens.
    pub cached_input_tokens: i64,
    /// The cache write input tokens.
    pub cache_write_input_tokens: i64,
    /// The output tokens.
    pub output_tokens: i64,
    /// The reasoning output tokens.
    pub reasoning_output_tokens: i64,
    /// The total tokens.
    pub total_tokens: i64,
}

impl TokenUsage {
    /// Adds another response's usage, returning `None` on integer overflow.
    pub fn checked_add(&mut self, other: &Self) -> Option<()> {
        let input_tokens = self.input_tokens.checked_add(other.input_tokens)?;
        let cached_input_tokens = self
            .cached_input_tokens
            .checked_add(other.cached_input_tokens)?;
        let cache_write_input_tokens = self
            .cache_write_input_tokens
            .checked_add(other.cache_write_input_tokens)?;
        let output_tokens = self.output_tokens.checked_add(other.output_tokens)?;
        let reasoning_output_tokens = self
            .reasoning_output_tokens
            .checked_add(other.reasoning_output_tokens)?;
        let total_tokens = self.total_tokens.checked_add(other.total_tokens)?;
        *self = Self {
            input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            output_tokens,
            reasoning_output_tokens,
            total_tokens,
        };
        Some(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for token usage info.
pub struct TokenUsageInfo {
    /// The total token usage.
    pub total_token_usage: TokenUsage,
    /// The last token usage.
    pub last_token_usage: TokenUsage,
    /// The model context window.
    pub model_context_window: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for token count event.
pub struct TokenCountEvent {
    /// The info.
    pub info: Option<TokenUsageInfo>,
    /// The rate limits.
    pub rate_limits: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for web search begin event.
pub struct WebSearchBeginEvent {
    /// The session identifier.
    pub session_id: String,
    /// The turn identifier.
    pub turn_id: String,
    /// The model step identifier.
    pub model_step_id: String,
    /// The call identifier.
    pub call_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Data for web search end event.
pub struct WebSearchEndEvent {
    /// The session identifier.
    pub session_id: String,
    /// The turn identifier.
    pub turn_id: String,
    /// The model step identifier.
    pub model_step_id: String,
    /// The call identifier.
    pub call_id: String,
    /// The action.
    pub action: WebSearchAction,
}
