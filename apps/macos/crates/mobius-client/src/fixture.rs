//! Transcript pages for snapshot scenes and tests, reduced by the gateway's own
//! `TranscriptReducer` from scripted events. Production code only ever receives pages and ops.

use mobius::SessionId;
use mobius::protocol::{
    AssistantContentDeltaEvent, ErrorEvent, ErrorKind, Event, EventMsg, FrontendBlock,
    MessageEvent, ModelStepContentPhase, RenderedBlock, TurnCompleteEvent, TurnStartedEvent,
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
    pub fn apply(&mut self, submission: Option<&str>, msg: EventMsg, blocks: Vec<RenderedBlock>) {
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
