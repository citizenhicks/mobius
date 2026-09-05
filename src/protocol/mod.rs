//! The small event protocol shared by agent frontends.

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use uuid::Uuid;

pub use self::replay::events as replay_events;
pub(crate) use self::replay::{
    ATTACHMENT_CONTEXT_MARKER, ATTACHMENTS_FIELD, CONTEXT_COMPACTED_MARKER, INTERNAL_MESSAGE_FIELD,
    MESSAGE_METADATA_FIELD, REPLAY_REASONING_FIELD, TOOL_ERROR_FIELD, internal_message_kind,
    is_internal_message, message_metadata, tool_complete_boundaries,
};

mod events;
mod frontend;
mod replay;

pub use self::events::*;
pub use self::frontend::*;

/// Maximum total UTF-8 bytes accepted in one user-input submission.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Maximum UTF-8 bytes accepted in capability command input or a queued message edit.
pub const MAX_CAPABILITY_INPUT_BYTES: usize = 64 * 1024;

/// One immutable, session-bound file addressed by an opaque reference.
///
/// Only upload-origin references are valid in `MessageSubmission::attachments`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFileReference {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub media_type: String,
}

/// Session-file policy advertised to frontends by the owning runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFileLimits {
    pub max_attachment_references: usize,
    pub max_file_bytes: u64,
    pub max_session_files: usize,
    pub max_session_bytes: u64,
    pub max_upload_chunk_bytes: usize,
}

/// Which side of a session produced one stored file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFileOrigin {
    User,
    Agent,
}

/// One stored session file together with its producer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionFileRecord {
    pub origin: SessionFileOrigin,
    pub file: SessionFileReference,
}

/// A command submitted by a frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    /// Correlates all events produced by this command.
    pub id: String,
    /// Command payload.
    pub op: Op,
}

/// Frontend-visible context for the session owner, workspace, and origin.
///
/// These values are correlation metadata, not authentication or authorization.
/// A remote host must derive them after authentication and inject tenant-scoped
/// backends when it creates the agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionContext {
    /// Immutable identity of the Bot that owns this session.
    pub bot_id: String,
    /// Opaque tenant or organization identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// Opaque identifier for the user who owns the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Optional display label, such as the local operating-system user name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    /// Opaque workspace identifier; this is not a filesystem path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Optional frontend-facing workspace label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_label: Option<String>,
    /// Optional label describing what created the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_label: Option<String>,
}

/// Human-readable model settings exposed to frontends.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub model: String,
    pub reasoning_effort: Option<String>,
}

/// How a model route makes newly discovered tool schemas available.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDiscoveryMode {
    /// Append tool schemas at the discovery point without rebuilding the cached prefix.
    Native,
    /// Reissue the active context with a changed top-level tool envelope.
    #[default]
    Rebuild,
}

/// One selectable runtime model route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    pub route: String,
    pub group: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub context_window: Option<i64>,
    pub supports_image_input: bool,
    pub supports_realtime_voice: bool,
    pub tool_discovery: ToolDiscoveryMode,
}

/// Who submitted one conversation message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MessageAuthor {
    User,
    Peer {
        message_id: String,
        session_id: String,
        handle: String,
        /// Optional semantic icon for the sending peer.
        #[serde(skip_serializing_if = "Option::is_none")]
        symbol: Option<FrontendSymbol>,
    },
}

/// Requested delivery for a message submitted while a turn is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveMessageDelivery {
    Steer,
    Queue,
}

/// One provider-neutral conversation message submitted to an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageSubmission {
    pub author: MessageAuthor,
    pub text: String,
    pub attachments: Vec<SessionFileReference>,
    #[serde(deserialize_with = "required_option")]
    pub reply: Option<MessageReply>,
    #[serde(deserialize_with = "required_option")]
    pub requested_delivery: Option<ActiveMessageDelivery>,
    #[serde(deserialize_with = "required_option")]
    pub target_turn_id: Option<String>,
}

impl MessageSubmission {
    /// Validates neutral message and file-reference invariants at the agent boundary.
    pub fn validate(&self, limits: SessionFileLimits) -> crate::Result<()> {
        const MAX_IDENTIFIER_BYTES: usize = 4 * 1024;

        if let Some(turn_id) = &self.target_turn_id {
            validate_message_identifier("target turn ID", turn_id, MAX_IDENTIFIER_BYTES)?;
        }
        if let Some(reply) = &self.reply {
            if reply.text.is_empty() {
                return Err(crate::Error::Config(
                    "quoted message cannot be empty".into(),
                ));
            }
            if reply.text.len() > MAX_MESSAGE_BYTES {
                return Err(crate::Error::Config(
                    "quoted message exceeds size limit".into(),
                ));
            }
        }
        validate_message_content(&self.author, &self.text, &self.attachments)?;
        validate_message_attachments(&self.attachments, limits)
    }
}

pub(crate) fn validate_message_content(
    author: &MessageAuthor,
    text: &str,
    attachments: &[SessionFileReference],
) -> crate::Result<()> {
    const MAX_IDENTIFIER_BYTES: usize = 4 * 1024;
    const MAX_HANDLE_BYTES: usize = 256;

    if text.len() > MAX_MESSAGE_BYTES {
        return Err(crate::Error::Config("message exceeds size limit".into()));
    }
    match author {
        MessageAuthor::User if text.trim().is_empty() && attachments.is_empty() => {
            return Err(crate::Error::Config("user message cannot be empty".into()));
        }
        MessageAuthor::Peer {
            message_id,
            session_id,
            handle,
            symbol,
        } => {
            validate_message_identifier("peer message ID", message_id, MAX_IDENTIFIER_BYTES)?;
            validate_message_identifier("peer session ID", session_id, MAX_IDENTIFIER_BYTES)?;
            validate_message_identifier("peer handle", handle, MAX_HANDLE_BYTES)?;
            if let Some(symbol) = symbol {
                validate_message_identifier("peer symbol", symbol.as_str(), MAX_HANDLE_BYTES)?;
            }
            if text.trim().is_empty() {
                return Err(crate::Error::Config("peer message cannot be empty".into()));
            }
            if !attachments.is_empty() {
                return Err(crate::Error::Config(
                    "peer messages cannot carry attachments".into(),
                ));
            }
        }
        MessageAuthor::User => {}
    }
    let mut ids = std::collections::BTreeSet::new();
    for attachment in attachments {
        if !ids.insert(&attachment.id) {
            return Err(crate::Error::Config(
                "attachment IDs must be unique per message".into(),
            ));
        }
        if Uuid::parse_str(&attachment.id).is_err() {
            return Err(crate::Error::Config("attachment ID must be a UUID".into()));
        }
        validate_message_identifier("attachment name", &attachment.name, 255)?;
        validate_message_identifier("attachment media type", &attachment.media_type, 127)?;
        if attachment.size == 0 {
            return Err(crate::Error::Config(
                "attachment size must be positive".into(),
            ));
        }
    }
    Ok(())
}

fn validate_message_attachments(
    attachments: &[SessionFileReference],
    limits: SessionFileLimits,
) -> crate::Result<()> {
    if attachments.len() > limits.max_attachment_references {
        return Err(crate::Error::Config(format!(
            "message cannot reference more than {} attachments",
            limits.max_attachment_references
        )));
    }
    let mut bytes = 0_u64;
    for attachment in attachments {
        if attachment.size > limits.max_file_bytes {
            return Err(crate::Error::Config(format!(
                "attachment size must be 1–{} bytes",
                limits.max_file_bytes
            )));
        }
        bytes = bytes
            .checked_add(attachment.size)
            .ok_or_else(|| crate::Error::Config("attachment sizes overflowed".into()))?;
    }
    if bytes > limits.max_session_bytes {
        return Err(crate::Error::Config(format!(
            "message attachments exceed the {}-byte session limit",
            limits.max_session_bytes
        )));
    }
    Ok(())
}

fn validate_message_identifier(name: &str, value: &str, limit: usize) -> crate::Result<()> {
    if value.trim().is_empty() {
        return Err(crate::Error::Config(format!("{name} cannot be empty")));
    }
    if value.len() > limit {
        return Err(crate::Error::Config(format!("{name} exceeds size limit")));
    }
    Ok(())
}

/// Commands supported by the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Op {
    /// Submit one conversation message for middleware-owned delivery.
    Message { message: MessageSubmission },
    /// Abort one active turn.
    Interrupt { turn_id: String },
    /// Resolve a paused tool batch.
    ExecApproval {
        id: String,
        decision: ReviewDecision,
    },
    /// Invokes a command owned by one capability.
    CapabilityCommand {
        capability: String,
        command: String,
        arguments: String,
        /// Optional caller-editable text kept separate from routing arguments.
        ///
        /// When embedded in a frontend action, a present value is its caller-editable text.
        #[serde(deserialize_with = "required_option")]
        input: Option<String>,
        #[serde(deserialize_with = "required_option")]
        target: Option<MessageTarget>,
    },
    /// Selects one immutable registered model route.
    SetModel { route: String },
    /// Requests that the frontend reopen an existing session.
    ResumeSession { session_id: String },
}

/// Which participant produced text in an externally hosted conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRole {
    User,
    Assistant,
}

fn required_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

/// An event emitted to a frontend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Submission ID that caused this event, if it was command-driven.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submission_id: Option<String>,
    /// Event payload.
    pub msg: EventMsg,
}

/// Events supported by the minimal frontend contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventMsg {
    MessageDelta(MessageDeltaEvent),
    Error(ErrorEvent),
    Warning(WarningEvent),
    SubmissionRejected(SubmissionRejectedEvent),
    SessionConfigured(SessionConfiguredEvent),
    #[serde(rename = "turn_started")]
    TurnStarted(TurnStartedEvent),
    #[serde(rename = "turn_complete")]
    TurnComplete(TurnCompleteEvent),
    TurnAborted(TurnAbortedEvent),
    Message(MessageEvent),
    AssistantMessage(AssistantMessageEvent),
    AssistantContentDelta(AssistantContentDeltaEvent),
    ModelStepStarted(ModelStepStartedEvent),
    ModelStepCompleted(ModelStepCompletedEvent),
    SessionHistory(SessionHistoryEvent),
    ModelChanged(ModelChangedEvent),
    SessionResumeRequested(SessionResumeRequestedEvent),
    ToolCallBegin(ToolCallBeginEvent),
    ToolCallEnd(ToolCallEndEvent),
    ToolLoad(ToolLoadEvent),
    ExecApprovalRequest(ExecApprovalRequestEvent),
    TokenCount(TokenCountEvent),
    ContextCompacted,
    WebSearchBegin(WebSearchBeginEvent),
    WebSearchEnd(WebSearchEndEvent),
    Frontend(FrontendEvent),
}

impl EventMsg {
    /// Returns the mutable durable transcript target carried by a complete message event.
    pub(crate) fn message_target_mut(&mut self) -> Option<&mut Option<MessageTarget>> {
        match self {
            Self::Message(message) => Some(&mut message.message_target),
            Self::AssistantMessage(message) => Some(&mut message.message_target),
            _ => None,
        }
    }

    pub(crate) fn message(&self) -> Option<&MessageEvent> {
        match self {
            Self::Message(message) => Some(message),
            _ => None,
        }
    }
}

/// Provider-neutral streaming output before submission correlation is attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEvent {
    TextDelta(String),
    CommentaryDelta(String),
    ReasoningDelta(String),
    WebSearchStarted {
        call_id: String,
    },
    WebSearchCompleted {
        call_id: String,
        action: WebSearchAction,
    },
}

/// Tracks streamed operations whose terminal event must be synthesized on failure.
#[derive(Clone, Default)]
pub(crate) struct ModelEventTracker {
    pending_web_searches: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
}

impl ModelEventTracker {
    pub(crate) fn observe(&self, event: &ModelEvent) -> crate::Result<()> {
        let mut pending = self
            .pending_web_searches
            .lock()
            .map_err(|_| crate::Error::Stopped("model event tracker is unavailable".into()))?;
        match event {
            ModelEvent::WebSearchStarted { call_id } => {
                pending.insert(call_id.clone());
            }
            ModelEvent::WebSearchCompleted { call_id, .. } => {
                pending.remove(call_id);
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn interrupted(&self) -> crate::Result<Vec<ModelEvent>> {
        let mut pending = self
            .pending_web_searches
            .lock()
            .map_err(|_| crate::Error::Stopped("model event tracker is unavailable".into()))?;
        Ok(std::mem::take(&mut *pending)
            .into_iter()
            .map(|call_id| ModelEvent::WebSearchCompleted {
                call_id,
                action: WebSearchAction::Interrupted,
            })
            .collect())
    }
}

/// Provider-neutral action reported by hosted web search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebSearchAction {
    Search {
        queries: Vec<String>,
    },
    OpenPage {
        url: Option<String>,
    },
    FindInPage {
        url: Option<String>,
        pattern: Option<String>,
    },
    Interrupted,
    Other,
}

impl ModelEvent {
    /// Converts one normalized provider event into the frontend protocol.
    #[must_use]
    pub fn into_event(self, session_id: &str, turn_id: &str, model_step_id: &str) -> EventMsg {
        match self {
            Self::TextDelta(delta) => EventMsg::AssistantContentDelta(AssistantContentDeltaEvent {
                session_id: session_id.into(),
                turn_id: turn_id.into(),
                model_step_id: model_step_id.into(),
                delta,
                phase: ModelStepContentPhase::FinalAnswer,
            }),
            Self::CommentaryDelta(delta) => {
                EventMsg::AssistantContentDelta(AssistantContentDeltaEvent {
                    session_id: session_id.into(),
                    turn_id: turn_id.into(),
                    model_step_id: model_step_id.into(),
                    delta,
                    phase: ModelStepContentPhase::Commentary,
                })
            }
            Self::ReasoningDelta(delta) => {
                EventMsg::AssistantContentDelta(AssistantContentDeltaEvent {
                    session_id: session_id.into(),
                    turn_id: turn_id.into(),
                    model_step_id: model_step_id.into(),
                    delta,
                    phase: ModelStepContentPhase::Reasoning,
                })
            }
            Self::WebSearchStarted { call_id } => EventMsg::WebSearchBegin(WebSearchBeginEvent {
                session_id: session_id.into(),
                turn_id: turn_id.into(),
                model_step_id: model_step_id.into(),
                call_id,
            }),
            Self::WebSearchCompleted { call_id, action } => {
                EventMsg::WebSearchEnd(WebSearchEndEvent {
                    session_id: session_id.into(),
                    turn_id: turn_id.into(),
                    model_step_id: model_step_id.into(),
                    call_id,
                    action,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn model_events_keep_typed_correlation_and_web_search_fields() {
        let delta = ModelEvent::CommentaryDelta("Checking".into()).into_event(
            "session-1",
            "turn-1",
            "step-1",
        );
        let search = ModelEvent::WebSearchCompleted {
            call_id: "search-1".into(),
            action: WebSearchAction::Search {
                queries: vec!["möbius framework".into(), "möbius gateway".into()],
            },
        }
        .into_event("session-1", "turn-1", "step-1");

        assert_eq!(
            serde_json::to_value(delta).expect("serialize delta"),
            json!({
                "type": "assistant_content_delta",
                "session_id": "session-1",
                "turn_id": "turn-1",
                "model_step_id": "step-1",
                "delta": "Checking",
                "phase": "commentary"
            })
        );
        assert_eq!(
            serde_json::to_value(search).expect("serialize web search"),
            json!({
                "type": "web_search_end",
                "session_id": "session-1",
                "turn_id": "turn-1",
                "model_step_id": "step-1",
                "call_id": "search-1",
                "action": {
                    "type": "search",
                    "queries": ["möbius framework", "möbius gateway"]
                }
            })
        );
    }

    #[test]
    fn interrupted_web_search_renders_a_terminal_warning_block() {
        let event = EventMsg::WebSearchEnd(WebSearchEndEvent {
            session_id: "session-1".into(),
            turn_id: "turn-1".into(),
            model_step_id: "step-1".into(),
            call_id: "search-1".into(),
            action: WebSearchAction::Interrupted,
        });

        assert_eq!(
            serde_json::to_value(&event).expect("serialize interrupted search"),
            json!({
                "type": "web_search_end",
                "session_id": "session-1",
                "turn_id": "turn-1",
                "model_step_id": "step-1",
                "call_id": "search-1",
                "action": {"type": "interrupted"}
            })
        );
        let block = event.presentation().expect("interrupted search renders");
        assert_eq!(block.capability, "web_search");
        assert_eq!(block.block.id.as_deref(), Some("step-1/search-1"));
        assert_eq!(block.block.state, FrontendBlockState::Complete);
        assert_eq!(block.block.tone, FrontendTone::Warning);
        assert_eq!(&*block.block.title, "Web search interrupted");
    }

    #[test]
    fn retrying_model_step_has_a_provider_neutral_reconnect_notice() {
        let event = EventMsg::ModelStepCompleted(ModelStepCompletedEvent {
            session_id: "session-1".into(),
            turn_id: "turn-1".into(),
            model_step_id: "step-1".into(),
            step_index: 0,
            started_at_ms: 1,
            completed_at_ms: 2,
            outcome: ModelStepOutcome::Retrying,
            diagnostics: None,
        });

        let rendered = event.presentation().expect("retry presentation");

        assert_eq!(rendered.capability, "agent");
        assert_eq!(rendered.block.id.as_deref(), Some("step-1/retry"));
        assert_eq!(rendered.block.title, "Reconnecting…");
        assert_eq!(rendered.block.tone, FrontendTone::Warning);
        assert_eq!(rendered.block.state, FrontendBlockState::Complete);
    }

    #[test]
    fn middleware_settings_have_a_generic_wire_shape() {
        let feature = MiddlewareFeature {
            id: "example".into(),
            label: "Example".into(),
            description: "Example capability".into(),
            required: false,
            settings: vec![FrontendSetting {
                id: "limit".into(),
                label: "Limit".into(),
                description: "Example limit".into(),
                composer: false,
                kind: FrontendSettingKind::Integer {
                    min: 1,
                    max: None,
                    step: 10,
                },
            }],
        };

        assert_eq!(
            serde_json::to_value(feature).expect("serialize middleware setting"),
            json!({
                "id": "example",
                "label": "Example",
                "description": "Example capability",
                "required": false,
                "settings": [{
                    "id": "limit",
                    "label": "Limit",
                    "description": "Example limit",
                    "composer": false,
                    "type": "integer",
                    "min": 1,
                    "step": 10
                }]
            })
        );
    }

    #[test]
    fn session_configured_has_a_stable_wire_shape() {
        let event = EventMsg::SessionConfigured(SessionConfiguredEvent {
            session_id: "session-1".into(),
            context: SessionContext {
                bot_id: "bot-1".into(),
                tenant_id: Some("tenant-1".into()),
                user_id: Some("user-1".into()),
                user_name: Some("Ada".into()),
                workspace_id: Some("workspace-1".into()),
                workspace_label: Some("Project One".into()),
                origin_label: Some("routine".into()),
            },
            model: ModelChangedEvent {
                route: "default".into(),
                model: "test-model".into(),
                reasoning_effort: Some("high".into()),
                model_context_window: Some(128_000),
            },
        });

        assert_eq!(
            serde_json::to_value(event).expect("serialize session event"),
            json!({
                "type": "session_configured",
                "session_id": "session-1",
                "context": {
                    "bot_id": "bot-1",
                    "tenant_id": "tenant-1",
                    "user_id": "user-1",
                    "user_name": "Ada",
                    "workspace_id": "workspace-1",
                    "workspace_label": "Project One",
                    "origin_label": "routine"
                },
                "model": {
                    "route": "default",
                    "model": "test-model",
                    "reasoning_effort": "high",
                    "model_context_window": 128_000
                }
            })
        );
    }

    #[test]
    fn session_resume_request_carries_the_target_context() {
        let event = EventMsg::SessionResumeRequested(SessionResumeRequestedEvent {
            session_id: "session-2".into(),
            context: SessionContext {
                bot_id: "bot-2".into(),
                workspace_label: Some("Project Two".into()),
                origin_label: Some("routine".into()),
                ..SessionContext::default()
            },
        });

        assert_eq!(
            serde_json::to_value(event).expect("serialize resume event"),
            json!({
                "type": "session_resume_requested",
                "session_id": "session-2",
                "context": {
                    "bot_id": "bot-2",
                    "workspace_label": "Project Two",
                    "origin_label": "routine"
                }
            })
        );
    }

    #[test]
    fn session_context_hard_requires_bot_ownership() {
        assert!(serde_json::from_value::<SessionContext>(json!({})).is_err());
        assert_eq!(
            serde_json::from_value::<SessionContext>(json!({"bot_id": "bot-1"}))
                .expect("required Bot context")
                .bot_id,
            "bot-1"
        );
    }

    #[test]
    fn frontend_event_has_a_distinct_nested_discriminator() {
        let event = EventMsg::Frontend(FrontendEvent::Widget {
            capability: "subagents".into(),
            item: FrontendWidget {
                id: "status".into(),
                slot: FrontendSlot::ComposerHeader,
                text: "2 agents".into(),
                tone: FrontendTone::Neutral,
                symbol: Some(FrontendSymbol::Agent),
                icon_only: true,
                progress: None,
                content: None,
                action: None,
            },
        });
        let value = serde_json::to_value(&event).expect("serialize frontend event");

        assert_eq!(value["type"], "frontend");
        assert_eq!(value["frontend_type"], "widget");
        assert_eq!(
            serde_json::from_value::<EventMsg>(value).expect("deserialize frontend event"),
            event
        );
    }

    #[test]
    fn capability_surface_slots_have_stable_wire_names() {
        assert_eq!(
            serde_json::to_value(FrontendSlot::Navigation).expect("navigation slot"),
            json!("navigation")
        );
        assert_eq!(
            serde_json::to_value(FrontendSlot::ChatMenu).expect("chat menu slot"),
            json!("chat_menu")
        );
        assert_eq!(
            serde_json::to_value(FrontendSlot::TranscriptTail).expect("transcript tail slot"),
            json!("transcript_tail")
        );
    }

    #[test]
    fn interrupt_has_a_targeted_wire_shape() {
        let submission = Submission {
            id: "cancel-1".into(),
            op: Op::Interrupt {
                turn_id: "turn-1".into(),
            },
        };

        assert_eq!(
            serde_json::to_value(submission).expect("serialize interrupt"),
            json!({
                "id": "cancel-1",
                "op": {
                    "type": "interrupt",
                    "turn_id": "turn-1"
                }
            })
        );
    }

    #[test]
    fn peer_message_symbol_is_optional_and_validated() {
        for (symbol, valid) in [(None, true), (Some("voice"), true), (Some(""), false)] {
            let author = MessageAuthor::Peer {
                message_id: "message".into(),
                session_id: "child".into(),
                handle: "voice agent".into(),
                symbol: symbol.map(|symbol| FrontendSymbol::Custom(symbol.into())),
            };
            let encoded = serde_json::to_value(&author).expect("encode");
            assert_eq!(
                serde_json::from_value::<MessageAuthor>(encoded).expect("decode"),
                author
            );
            assert_eq!(
                validate_message_content(&author, "Run the tests", &[]).is_ok(),
                valid
            );
        }
    }

    #[test]
    fn message_submission_has_one_typed_payload() {
        let submission = Submission {
            id: "input-1".into(),
            op: Op::Message {
                message: MessageSubmission {
                    author: MessageAuthor::User,
                    text: "hello".into(),
                    attachments: Vec::new(),
                    reply: Some(MessageReply {
                        target: MessageTarget {
                            checkpoint_sequence: 7,
                            batch_item_count: 2,
                        },
                        text: "earlier".into(),
                    }),
                    requested_delivery: Some(ActiveMessageDelivery::Queue),
                    target_turn_id: Some("turn-1".into()),
                },
            },
        };

        assert_eq!(
            serde_json::to_value(submission).expect("serialize input"),
            json!({
                "id": "input-1",
                "op": {
                    "type": "message",
                    "message": {
                        "author": {"type": "user"},
                        "text": "hello",
                        "attachments": [],
                        "reply": {
                            "target": {
                                "checkpoint_sequence": 7,
                                "batch_item_count": 2
                            },
                            "text": "earlier"
                        },
                        "requested_delivery": "queue",
                        "target_turn_id": "turn-1"
                    }
                }
            })
        );
    }

    #[test]
    fn conversation_events_use_turn_and_text_wire_names() {
        let events = [
            EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: "turn-1".into(),
                model_context_window: Some(128_000),
            }),
            EventMsg::Message(MessageEvent {
                author: MessageAuthor::User,
                delivery: MessageDelivery::Turn,
                text: "hello".into(),
                attachments: Vec::new(),
                reply: None,
                message_target: None,
            }),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".into(),
            }),
        ];

        assert_eq!(
            serde_json::to_value(events).expect("serialize conversation events"),
            json!([
                {
                    "type": "turn_started",
                    "turn_id": "turn-1",
                    "model_context_window": 128_000
                },
                {
                    "type": "message",
                    "author": {"type": "user"},
                    "delivery": "turn",
                    "text": "hello",
                    "attachments": [],
                    "reply": null,
                    "message_target": null
                },
                {
                    "type": "turn_complete",
                    "turn_id": "turn-1"
                }
            ])
        );
    }

    #[test]
    fn stored_message_event_without_reply_decodes_as_no_reply() {
        let event: MessageEvent = serde_json::from_value(json!({
            "author": {"type": "user"},
            "delivery": "turn",
            "text": "before replies existed",
            "attachments": [],
            "message_target": null
        }))
        .expect("decode old stored message event");

        assert_eq!(event.reply, None);
    }

    #[test]
    fn system_event_omits_submission_correlation() {
        let event = Event {
            submission_id: None,
            msg: EventMsg::Warning(WarningEvent {
                message: "system notice".into(),
            }),
        };

        assert_eq!(
            serde_json::to_value(event).expect("serialize system event"),
            json!({
                "msg": {
                    "type": "warning",
                    "message": "system notice"
                }
            })
        );
    }

    #[test]
    fn submission_rejection_has_a_typed_wire_shape() {
        assert_eq!(
            serde_json::to_value(EventMsg::SubmissionRejected(SubmissionRejectedEvent {
                message: "message queue is full".into(),
            }))
            .expect("serialize rejection"),
            json!({
                "type": "submission_rejected",
                "message": "message queue is full"
            })
        );
    }

    #[test]
    fn context_compacted_is_a_unit_event() {
        assert_eq!(
            serde_json::to_value(EventMsg::ContextCompacted).expect("serialize compaction"),
            json!({"type": "context_compacted"})
        );
    }

    #[test]
    fn token_usage_overflow_does_not_partially_update_the_total() {
        let mut total = TokenUsage {
            input_tokens: 7,
            total_tokens: i64::MAX,
            ..TokenUsage::default()
        };
        let original = total.clone();

        assert!(
            total
                .checked_add(&TokenUsage {
                    input_tokens: 1,
                    total_tokens: 1,
                    ..TokenUsage::default()
                })
                .is_none()
        );
        assert_eq!(total, original);
    }

    #[test]
    fn symbols_round_trip_and_keep_unknown_names() {
        for symbol in [
            FrontendSymbol::Agent,
            FrontendSymbol::Brain,
            FrontendSymbol::Branch,
            FrontendSymbol::Chat,
            FrontendSymbol::Delete,
            FrontendSymbol::Edit,
            FrontendSymbol::Promote,
            FrontendSymbol::Route,
            FrontendSymbol::Search,
            FrontendSymbol::Sparkle,
            FrontendSymbol::Storage,
            FrontendSymbol::Task,
        ] {
            let json = serde_json::to_string(&symbol).expect("symbol serializes");
            assert_eq!(json, format!("\"{}\"", symbol.as_str()));
            let decoded: FrontendSymbol = serde_json::from_str(&json).expect("symbol deserializes");
            assert_eq!(decoded, symbol);
        }

        // A name this build has never heard of survives instead of failing the frame.
        let custom: FrontendSymbol =
            serde_json::from_str("\"telescope\"").expect("unknown symbol deserializes");
        assert_eq!(custom, FrontendSymbol::Custom("telescope".into()));
        assert_eq!(custom.as_str(), "telescope");

        // A known name never lingers as a `Custom` once it has crossed the wire, so the two
        // spellings of one glyph cannot compare unequal.
        let normalized: FrontendSymbol = serde_json::from_str(
            &serde_json::to_string(&FrontendSymbol::Custom("edit".into()))
                .expect("custom serializes"),
        )
        .expect("custom deserializes");
        assert_eq!(normalized, FrontendSymbol::Edit);
    }
}
