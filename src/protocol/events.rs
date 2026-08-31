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
pub struct ErrorEvent {
    pub kind: ErrorKind,
    pub message: String,
    pub retryable: bool,
    pub status: Option<u16>,
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
    Configuration,
    DuplicateRegistration,
    UnknownRegistration,
    Provider,
    Authentication,
    Sandbox,
    Tool,
    Checkpoint,
    Busy,
    Stopped,
    Rollback,
    Io,
    Http,
    Json,
    Storage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarningEvent {
    pub message: String,
}

/// A submission the agent rejected without changing durable turn state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionRejectedEvent {
    pub message: String,
}

/// Immutable session data emitted once when an agent starts or resumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfiguredEvent {
    pub session_id: String,
    pub context: SessionContext,
    pub model: ModelChangedEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStartedEvent {
    pub turn_id: String,
    pub model_context_window: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCompleteEvent {
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAbortedEvent {
    pub turn_id: String,
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
    Turn,
    Steer,
    Queue,
}

/// One accepted conversation message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageEvent {
    pub author: MessageAuthor,
    pub delivery: MessageDelivery,
    pub text: String,
    pub attachments: Vec<SessionFileReference>,
    #[serde(deserialize_with = "required_option")]
    pub message_target: Option<MessageTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessageEvent {
    pub session_id: String,
    pub turn_id: String,
    pub model_step_id: String,
    pub content: Vec<ModelStepContent>,
    #[serde(deserialize_with = "required_option")]
    pub message_target: Option<MessageTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantContentDeltaEvent {
    pub session_id: String,
    pub turn_id: String,
    pub model_step_id: String,
    pub delta: String,
    pub phase: ModelStepContentPhase,
}

/// One provider request becoming active within a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStepStartedEvent {
    pub session_id: String,
    pub turn_id: String,
    pub model_step_id: String,
    pub step_index: usize,
    pub started_at_ms: i64,
}

/// The terminal record for one provider request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStepCompletedEvent {
    pub session_id: String,
    pub turn_id: String,
    pub model_step_id: String,
    pub step_index: usize,
    pub started_at_ms: i64,
    pub completed_at_ms: i64,
    pub outcome: ModelStepOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<ModelStepDiagnostics>,
}

/// Provider-owned cost and prompt-cache observations for one completed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStepDiagnostics {
    pub provider: String,
    pub prompt_cache: PromptCacheDiagnostics,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_microusd: Option<u64>,
}

/// Prompt-cache behavior observed for one completed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheDiagnostics {
    pub capability: PromptCacheMode,
    pub context_epoch: u64,
    pub outcome: PromptCacheOutcome,
    pub rewrite_reasons: Vec<String>,
}

/// Provider-advertised prompt-cache behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheMode {
    Unsupported,
    Implicit,
    Explicit,
}

/// Cache result inferred from provider usage and local rewrite metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheOutcome {
    Unsupported,
    Hit,
    Write,
    Miss,
    ContextRewrite,
}

/// Provider-neutral outcome of a completed model step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ModelStepOutcome {
    Completed {
        end_turn: bool,
        tool_call_ids: Vec<String>,
        usage: TokenUsage,
    },
    Failed,
    Interrupted,
    /// The response stream failed and the logical step will restart with a fresh ID.
    Retrying,
}

/// One complete normalized text item produced by a model step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStepContent {
    pub output_index: usize,
    pub part_index: usize,
    pub phase: ModelStepContentPhase,
    pub text: String,
    pub annotations: Vec<ModelStepAnnotation>,
}

/// A provider-neutral annotation attached to one complete text part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelStepAnnotation {
    UrlCitation {
        url: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        start_index: usize,
        end_index: usize,
    },
    FileCitation {
        file_id: String,
        filename: String,
        index: usize,
    },
    ContainerFileCitation {
        container_id: String,
        file_id: String,
        filename: String,
        start_index: usize,
        end_index: usize,
    },
    FilePath {
        file_id: String,
        index: usize,
    },
    DocumentCharacterCitation {
        cited_text: String,
        document_index: usize,
        document_title: Option<String>,
        file_id: Option<String>,
        start_char_index: usize,
        end_char_index: usize,
    },
    DocumentPageCitation {
        cited_text: String,
        document_index: usize,
        document_title: Option<String>,
        file_id: Option<String>,
        start_page_number: usize,
        end_page_number: usize,
    },
    DocumentContentBlockCitation {
        cited_text: String,
        document_index: usize,
        document_title: Option<String>,
        file_id: Option<String>,
        start_block_index: usize,
        end_block_index: usize,
    },
    SearchResultCitation {
        cited_text: String,
        search_result_index: usize,
        source: String,
        title: Option<String>,
        start_block_index: usize,
        end_block_index: usize,
    },
    WebSearchResultCitation {
        cited_text: String,
        encrypted_index: String,
        title: Option<String>,
        url: String,
    },
}

/// Semantic role of text preserved in a completed model step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStepContentPhase {
    Reasoning,
    Commentary,
    FinalAnswer,
}

/// A restored transcript kept distinct from live turn lifecycle events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHistoryEvent {
    pub events: Vec<EventMsg>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChangedEvent {
    pub route: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub model_context_window: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResumeRequestedEvent {
    pub session_id: String,
    pub context: SessionContext,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallBeginEvent {
    pub turn_id: String,
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallEndEvent {
    pub turn_id: String,
    pub call_id: String,
    pub name: String,
    pub output: String,
    pub is_error: bool,
}

/// Deferred tool schemas materialized at one model-context position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLoadEvent {
    pub turn_id: String,
    pub load_id: String,
    pub catalog_revision: String,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecApprovalRequestEvent {
    pub id: String,
    pub turn_id: String,
    pub calls: Vec<ApprovalCall>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecApprovalReviewEvent {
    pub id: String,
    pub turn_id: String,
    pub calls: Vec<ApprovalCall>,
    pub status: ApprovalReviewStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<ApprovalReviewEscalation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReviewStatus {
    Reviewing,
    Approved,
    Escalated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReviewEscalation {
    ReviewerAsked,
    ReviewDataUnavailable,
    ReviewerUnavailable,
    InvalidResponse,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalCall {
    pub call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// A user's decision for a paused tool batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ApprovedForSession,
    Denied { rejection: String },
    Abort,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
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
pub struct TokenUsageInfo {
    pub total_token_usage: TokenUsage,
    pub last_token_usage: TokenUsage,
    pub model_context_window: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCountEvent {
    pub info: Option<TokenUsageInfo>,
    pub rate_limits: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchBeginEvent {
    pub session_id: String,
    pub turn_id: String,
    pub model_step_id: String,
    pub call_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchEndEvent {
    pub session_id: String,
    pub turn_id: String,
    pub model_step_id: String,
    pub call_id: String,
    pub action: WebSearchAction,
}
