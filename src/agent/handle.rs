//! The frontend handle to a running agent.

use std::sync::Arc;

use tokio::sync::mpsc;

use super::AgentSender;
use crate::backend::checkpoint::JournalEvent;
use crate::backend::model::ModelRouter;
use crate::middleware::FrontendExtensions;
use crate::protocol::Event;
use crate::protocol::SessionConfiguredEvent;

pub(super) const EVENT_QUEUE_CAPACITY: usize = 256;

impl std::fmt::Debug for Agent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Agent")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for AgentEvents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentEvents")
            .finish_non_exhaustive()
    }
}

/// Bidirectional handle consumed by a frontend.
pub struct Agent {
    pub(super) sender: AgentSender,
    pub(super) events: mpsc::Receiver<JournalEvent>,
    pub(super) model_router: Arc<ModelRouter>,
    pub(super) frontend: FrontendExtensions,
    pub(super) frontend_sink: crate::middleware::FrontendEventSink,
    pub(super) session: Arc<SessionConfiguredEvent>,
    pub(super) next_before_sequence: Option<u64>,
}

impl Agent {
    /// Returns a cloneable command sender.
    #[must_use]
    pub fn sender(&self) -> AgentSender {
        AgentSender {
            ingress: Arc::clone(&self.sender.ingress),
        }
    }

    /// Returns the immutable provider router owned by this runtime.
    #[must_use]
    pub fn model_router(&self) -> Arc<ModelRouter> {
        Arc::clone(&self.model_router)
    }

    /// Receives the next agent event.
    ///
    /// Frontends must keep draining events while the agent is running.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await.map(|record| record.event)
    }

    /// Returns the commands and status data exported by installed middleware.
    #[must_use]
    pub fn frontend(&self) -> &FrontendExtensions {
        &self.frontend
    }

    /// Returns the existing recorder for capability-owned frontend updates.
    /// This sink does not keep a stopped Agent or its event stream alive.
    #[must_use]
    pub fn frontend_sink(&self) -> crate::middleware::FrontendEventSink {
        Arc::clone(&self.frontend_sink)
    }

    /// Returns the immutable session descriptor emitted at startup.
    #[must_use]
    pub fn session(&self) -> &SessionConfiguredEvent {
        &self.session
    }

    /// Returns the selected model route at frontend startup.
    #[must_use]
    pub fn model_route(&self) -> &str {
        &self.session.model.route
    }

    /// Returns the cursor immediately preceding the initial transcript replay.
    #[must_use]
    pub const fn next_before_sequence(&self) -> Option<u64> {
        self.next_before_sequence
    }

    /// Separates command and event halves for a frontend event loop.
    ///
    /// The returned event receiver must be drained while commands are active.
    #[must_use]
    pub fn into_parts(self) -> (AgentSender, AgentEvents) {
        (self.sender, AgentEvents { inner: self.events })
    }

    /// Moves command/event halves and startup descriptors into a durable frontend owner.
    #[must_use]
    pub fn into_recorded_parts(
        self,
    ) -> (
        AgentSender,
        mpsc::Receiver<JournalEvent>,
        Arc<SessionConfiguredEvent>,
        FrontendExtensions,
    ) {
        (self.sender, self.events, self.session, self.frontend)
    }
}

/// Event receiver for frontends that do not own durable event recording.
pub struct AgentEvents {
    inner: mpsc::Receiver<JournalEvent>,
}

impl AgentEvents {
    /// Receives the next event.
    pub async fn recv(&mut self) -> Option<Event> {
        self.inner.recv().await.map(|record| record.event)
    }
}
