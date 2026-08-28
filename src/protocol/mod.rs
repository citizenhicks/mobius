//! The small event protocol shared by agent frontends.

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;

pub use self::replay::events as replay_events;
pub(crate) use self::replay::{
    ATTACHMENT_CONTEXT_MARKER, ATTACHMENTS_FIELD, CONTEXT_COMPACTED_MARKER, INTERNAL_MESSAGE_FIELD,
    PEER_MESSAGE_MARKER, PEER_METADATA_FIELD, REPLAY_REASONING_FIELD, TOOL_ERROR_FIELD,
    internal_message_kind, is_internal_message, strip_attachment_references,
    tool_complete_boundaries,
};

mod events;
mod frontend;
mod replay;

pub use self::events::*;
pub use self::frontend::*;

/// Maximum total UTF-8 bytes accepted in one user-input submission.
pub const MAX_USER_INPUT_BYTES: usize = 1024 * 1024;

/// Maximum UTF-8 bytes accepted in capability command input or queued active input.
pub const MAX_CAPABILITY_INPUT_BYTES: usize = 64 * 1024;

/// One immutable, session-bound file addressed by an opaque reference.
///
/// Only upload-origin references are valid in `Op::UserInput.attachments`.
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
    pub tool_discovery: ToolDiscoveryMode,
}

/// Commands supported by the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Op {
    /// Start a user turn.
    UserInput {
        text: String,
        attachments: Vec<SessionFileReference>,
    },
    /// Start a turn from advisory input sent by another agent session.
    PeerInput {
        message_id: String,
        source_session_id: String,
        source_handle: String,
        text: String,
    },
    /// Submit capability-owned input while a turn is active.
    ActiveInput {
        operation: String,
        turn_id: String,
        text: String,
    },
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
    Error(ErrorEvent),
    Warning(WarningEvent),
    SessionConfigured(SessionConfiguredEvent),
    #[serde(rename = "task_started")]
    TurnStarted(TurnStartedEvent),
    #[serde(rename = "task_complete")]
    TurnComplete(TurnCompleteEvent),
    TurnAborted(TurnAbortedEvent),
    UserMessage(UserMessageEvent),
    PeerMessage(PeerMessageEvent),
    AgentMessage(AgentMessageEvent),
    AgentMessageContentDelta(AgentMessageContentDeltaEvent),
    AgentReasoningContentDelta(AgentReasoningContentDeltaEvent),
    ModelStepStarted(ModelStepStartedEvent),
    ModelStepCompleted(ModelStepCompletedEvent),
    SessionHistory(SessionHistoryEvent),
    ModelChanged(ModelChangedEvent),
    SessionResumeRequested(SessionResumeRequestedEvent),
    ToolCallBegin(ToolCallBeginEvent),
    ToolCallEnd(ToolCallEndEvent),
    ToolLoad(ToolLoadEvent),
    ExecApprovalRequest(ExecApprovalRequestEvent),
    ExecApprovalReview(ExecApprovalReviewEvent),
    TokenCount(TokenCountEvent),
    ContextCompacted,
    WebSearchBegin(WebSearchBeginEvent),
    WebSearchEnd(WebSearchEndEvent),
    Frontend(FrontendEvent),
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
            Self::TextDelta(delta) => {
                EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent {
                    session_id: session_id.into(),
                    turn_id: turn_id.into(),
                    model_step_id: model_step_id.into(),
                    delta,
                    phase: AgentMessagePhase::FinalAnswer,
                })
            }
            Self::CommentaryDelta(delta) => {
                EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent {
                    session_id: session_id.into(),
                    turn_id: turn_id.into(),
                    model_step_id: model_step_id.into(),
                    delta,
                    phase: AgentMessagePhase::Commentary,
                })
            }
            Self::ReasoningDelta(delta) => {
                EventMsg::AgentReasoningContentDelta(AgentReasoningContentDeltaEvent {
                    session_id: session_id.into(),
                    turn_id: turn_id.into(),
                    model_step_id: model_step_id.into(),
                    delta,
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
                "type": "agent_message_content_delta",
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
                tenant_id: Some("tenant-1".into()),
                user_id: Some("user-1".into()),
                user_name: Some("Ada".into()),
                workspace_id: Some("workspace-1".into()),
                workspace_label: Some("Project One".into()),
                origin_label: Some("cron".into()),
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
                    "tenant_id": "tenant-1",
                    "user_id": "user-1",
                    "user_name": "Ada",
                    "workspace_id": "workspace-1",
                    "workspace_label": "Project One",
                    "origin_label": "cron"
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
                workspace_label: Some("Project Two".into()),
                origin_label: Some("cron".into()),
                ..SessionContext::default()
            },
        });

        assert_eq!(
            serde_json::to_value(event).expect("serialize resume event"),
            json!({
                "type": "session_resume_requested",
                "session_id": "session-2",
                "context": {
                    "workspace_label": "Project Two",
                    "origin_label": "cron"
                }
            })
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
    fn user_input_has_one_text_payload() {
        let submission = Submission {
            id: "input-1".into(),
            op: Op::UserInput {
                text: "hello".into(),
                attachments: Vec::new(),
            },
        };

        assert_eq!(
            serde_json::to_value(submission).expect("serialize input"),
            json!({
                "id": "input-1",
                "op": {
                    "type": "user_input",
                    "text": "hello",
                    "attachments": []
                }
            })
        );
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
    fn context_compacted_is_a_unit_event() {
        assert_eq!(
            serde_json::to_value(EventMsg::ContextCompacted).expect("serialize compaction"),
            json!({"type": "context_compacted"})
        );
    }

    #[test]
    fn approval_review_serializes_the_wire_contract() {
        let calls = || {
            vec![ApprovalCall {
                call_id: "call-1".into(),
                name: "bash".into(),
                arguments: json!({"command": "git status"}),
            }]
        };
        let reviewing = EventMsg::ExecApprovalReview(ExecApprovalReviewEvent {
            id: "approval-1".into(),
            turn_id: "turn-1".into(),
            calls: calls(),
            status: ApprovalReviewStatus::Reviewing,
            reason: None,
        });
        let escalated = EventMsg::ExecApprovalReview(ExecApprovalReviewEvent {
            id: "approval-1".into(),
            turn_id: "turn-1".into(),
            calls: calls(),
            status: ApprovalReviewStatus::Escalated,
            reason: Some(ApprovalReviewEscalation::ReviewerAsked),
        });

        // Clients reject a null reason for reviewing/approved, so the key stays absent.
        assert_eq!(
            serde_json::to_value(reviewing).expect("serialize reviewing"),
            json!({
                "type": "exec_approval_review",
                "id": "approval-1",
                "turn_id": "turn-1",
                "calls": [{"call_id": "call-1", "name": "bash", "arguments": {"command": "git status"}}],
                "status": "reviewing"
            })
        );
        assert_eq!(
            serde_json::to_value(escalated).expect("serialize escalated"),
            json!({
                "type": "exec_approval_review",
                "id": "approval-1",
                "turn_id": "turn-1",
                "calls": [{"call_id": "call-1", "name": "bash", "arguments": {"command": "git status"}}],
                "status": "escalated",
                "reason": "reviewer_asked"
            })
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
