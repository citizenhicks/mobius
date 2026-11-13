//! Gateway snapshots and transcript pages for snapshot scenes and tests.
//! Scripted pages use the gateway's own reducer; production receives gateway records.

use mobius::SessionId;
use mobius::protocol::{
    AssistantContentDeltaEvent, ErrorEvent, ErrorKind, Event, EventMsg, FrontendBlock,
    FrontendBlockFormat, FrontendBlockRole, FrontendBlockState, FrontendTone, MessageAuthor,
    MessageDelivery, MessageEvent, ModelStepContentPhase, RenderedBlock, SessionFileReference,
    ToolCallBeginEvent, ToolCallEndEvent, ToolContent, TurnCompleteEvent, TurnStartedEvent,
};
use mobius_gateway::TranscriptReducer;
use mobius_gateway::wire::TranscriptPage;

use super::TranscriptStore;

/// Events in order, reduced as the gateway reduces them.
#[derive(Debug)]
pub struct Script {
    reducer: TranscriptReducer,
    session: SessionId,
    sequence: u64,
}

impl Default for Script {
    fn default() -> Self {
        Self::inherited(0)
    }
}

impl Script {
    /// History of the chat `depth` forks up, as a fork inherits it.
    pub fn inherited(depth: u32) -> Self {
        Self {
            reducer: TranscriptReducer::new(depth),
            session: SessionId::generate(),
            sequence: 0,
        }
    }

    /// One scripted event and its capability-owned presentation.
    fn apply(&mut self, submission: Option<&str>, msg: EventMsg, blocks: Vec<RenderedBlock>) {
        self.sequence += 1;
        let at = 1_790_000_000_000 + self.sequence.cast_signed() * 4_000;
        let event = Event {
            submission_id: submission.map(Into::into),
            msg,
        };
        self.reducer.apply(self.sequence, at, &event, blocks);
    }

    /// Starts or ends the turn `id`.
    pub fn turn(&mut self, id: &str, start: bool) {
        let turn_id = id.into();
        let msg = if start {
            EventMsg::TurnStarted(TurnStartedEvent {
                idle: false,
                turn_id,
                model_context_window: None,
            })
        } else {
            EventMsg::TurnComplete(TurnCompleteEvent {
                idle: false,
                turn_id,
            })
        };
        self.apply(None, msg, Vec::new());
    }

    /// A user message, including its delivery mode and attachments.
    pub fn user(
        &mut self,
        id: &str,
        text: &str,
        delivery: MessageDelivery,
        files: Vec<SessionFileReference>,
    ) {
        self.message(
            id,
            MessageEvent {
                author: MessageAuthor::User,
                delivery,
                text: text.into(),
                attachments: files,
                reply: None,
                message_target: None,
            },
        );
    }

    /// One invocation with its capability-rendered block.
    pub fn tool(&mut self, tool: ToolFixture) {
        let ToolFixture { call, block } = tool;
        let event = if block.state == FrontendBlockState::Pending {
            EventMsg::ToolCallBegin(call)
        } else {
            EventMsg::ToolCallEnd(ToolCallEndEvent {
                turn_id: call.turn_id,
                call_id: call.call_id,
                name: call.name,
                output: ToolContent::default(),
                is_error: block.tone == FrontendTone::Error,
            })
        };
        self.apply(
            None,
            event,
            vec![RenderedBlock {
                capability: "tools".into(),
                block,
            }],
        );
    }

    /// A message under its submission id.
    pub fn message(&mut self, submission: &str, message: MessageEvent) {
        self.apply(Some(submission), EventMsg::Message(message), Vec::new());
    }

    /// Streams `text` onto the phase of `turn`'s model step `step`.
    pub fn text(&mut self, turn: &str, step: &str, phase: ModelStepContentPhase, text: &str) {
        let msg = EventMsg::AssistantContentDelta(AssistantContentDeltaEvent {
            session_id: self.session.share(),
            turn_id: turn.into(),
            model_step_id: step.into(),
            delta: text.into(),
            phase,
        });
        self.apply(None, msg, Vec::new());
    }

    /// A capability's block, replacing the block with its id.
    pub fn block(&mut self, capability: &str, block: FrontendBlock) {
        let rendered = RenderedBlock {
            capability: capability.into(),
            block,
        };
        self.apply(None, EventMsg::ContextCompacted, vec![rendered]);
    }

    /// An error no capability rendered.
    #[cfg_attr(
        not(feature = "snapshot"),
        expect(dead_code, reason = "only snapshot scenes build this")
    )]
    pub fn error(&mut self, text: &str) {
        let msg = EventMsg::Error(ErrorEvent {
            kind: ErrorKind::Provider,
            message: text.into(),
            retryable: true,
            status: None,
            retry_after: None,
        });
        self.apply(None, msg, Vec::new());
    }

    /// Where a fork of `parent` begins, above its own history.
    #[cfg_attr(
        not(feature = "snapshot"),
        expect(dead_code, reason = "only snapshot scenes build this")
    )]
    pub fn fork_point(&mut self, parent: SessionId) {
        self.reducer.fork_point(parent);
    }

    /// The page the gateway would send for these events.
    pub fn page(self) -> TranscriptPage {
        self.reducer.page()
    }

    pub fn store(self) -> TranscriptStore {
        TranscriptStore::from(self.page())
    }
}

/// A tool invocation paired with the presentation a scene can customize.
#[derive(Debug)]
pub struct ToolFixture {
    call: ToolCallBeginEvent,
    pub block: FrontendBlock,
}

/// One completed tool call with the supplied stable call/turn identities.
pub fn tool(
    id: &str,
    turn: &str,
    title: &str,
    text: &str,
    format: FrontendBlockFormat,
) -> ToolFixture {
    ToolFixture {
        call: ToolCallBeginEvent {
            turn_id: turn.into(),
            call_id: id.into(),
            name: "fixture_tool".into(),
            arguments: serde_json::json!({}),
        },
        block: FrontendBlock {
            id: Some(id.into()),
            group: Some(turn.into()),
            role: FrontendBlockRole::Tool,
            title: title.into(),
            text: text.into(),
            format,
            ..FrontendBlock::default()
        },
    }
}

#[cfg(test)]
pub(crate) fn opened(session: &str, latest: u64) -> mobius_gateway::wire::ServerMessage {
    let frame: mobius_gateway::wire::ServerFrame = serde_json::from_value(serde_json::json!({
        "version": mobius_gateway::wire::PROTOCOL_VERSION,
        "type": "session_opened",
        "request_id": "open",
        "payload": {
            "active_turn_ids": [], "pending_approvals": [], "latest_sequence": latest,
            "attached_folders": [],
            "workspace": {"id": "workspace", "path": "/workspace"}, "git": null,
            "session": {
                "session_id": session, "context": {"owner_id": "bot"},
                "model": {
                    "route": "default", "model": "model",
                    "reasoning_effort": null, "model_context_window": null
                }
            },
            "contributions": [], "widgets": [],
            "context_limit_tokens": null, "active_message_delivery": "steer",
            "run_stats": mobius_gateway::wire::RunStats::default(),
            "pinned_messages": []
        }
    }))
    .unwrap();
    frame.message
}
