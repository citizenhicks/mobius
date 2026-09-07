//! A small, modular Rust framework for one linear agent session.
//!
//! Applications compose an [`agent::Agent`] from explicit model, sandbox, checkpoint, and
//! middleware adapters. Frontends remain separate: they submit [`protocol::Op`] values and
//! render the frontend-neutral [`protocol::Event`] stream.
//!
//! # Embedded composition
//!
//! The caller owns every runtime dependency. Include exactly one message-handling middleware,
//! give new sessions a non-empty [`protocol::SessionContext::bot_id`], and keep draining events
//! while commands are active.
//!
//! ```rust,no_run
//! use std::path::Path;
//! use std::sync::Arc;
//!
//! use mobius::Result;
//! use mobius::agent::{Agent, AgentConfig, create_agent};
//! use mobius::backend::checkpoint::{CheckpointStore, sqlite::SqliteCheckpoint};
//! use mobius::backend::model::{Model, ModelRouter, openai::OpenAi};
//! use mobius::backend::sandbox::{ApprovalPolicy, Sandbox, local::LocalSandbox};
//! use mobius::middleware::{Middleware, MiddlewareStack};
//! use mobius::middleware::{messages::Messages, tools::Tools};
//! use mobius::protocol::SessionContext;
//!
//! async fn build_agent(
//!     workspace: &Path,
//!     api_key: String,
//!     model_id: &str,
//! ) -> Result<Agent> {
//!     let model: Arc<dyn Model> = Arc::new(OpenAi::new(
//!         api_key,
//!         "https://api.openai.com/v1",
//!         model_id,
//!     )?);
//!     let models = Arc::new(ModelRouter::new("default", model));
//!     let sandbox = Arc::new(Sandbox::new(
//!         Arc::new(LocalSandbox::new(workspace)?),
//!         ApprovalPolicy::Ask,
//!     ));
//!     let checkpoints: Arc<dyn CheckpointStore> =
//!         Arc::new(SqliteCheckpoint::new(workspace.join("mobius.sqlite3"))?);
//!     let middleware: Vec<Arc<dyn Middleware>> = vec![
//!         Arc::new(Messages::default()),
//!         Arc::new(Tools::coding()),
//!     ];
//!
//!     create_agent(
//!         AgentConfig::new(
//!             models,
//!             sandbox,
//!             checkpoints,
//!             MiddlewareStack::new(middleware)?,
//!             "You are a concise coding agent.",
//!         )
//!         .session_context(SessionContext {
//!             bot_id: "embedded".into(),
//!             ..SessionContext::default()
//!         }),
//!     )
//!     .await
//! }
//! ```
//!
//! A custom provider implements [`backend::model::Model`] and must return normalized output.
//! [`backend::model::ModelEventSink`] is synchronous and fallible; propagate its error rather
//! than silently losing a streamed event. This example also uses `serde_json`.
//!
//! ```rust,no_run
//! use serde_json::json;
//!
//! use mobius::{BoxFuture, Result};
//! use mobius::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest};
//! use mobius::protocol::{ModelEvent, ModelInfo, TokenUsage};
//!
//! struct EchoModel;
//!
//! impl Model for EchoModel {
//!     fn info(&self) -> ModelInfo {
//!         ModelInfo {
//!             model: "echo".into(),
//!             reasoning_effort: None,
//!         }
//!     }
//!
//!     fn respond<'a>(
//!         &'a self,
//!         _request: ModelRequest<'a>,
//!         events: ModelEventSink,
//!     ) -> BoxFuture<'a, Result<ModelOutput>> {
//!         Box::pin(async move {
//!             events(ModelEvent::TextDelta("done".into()))?;
//!             ModelOutput::from_output(
//!                 vec![json!({
//!                     "type": "message",
//!                     "role": "assistant",
//!                     "content": [{"type": "output_text", "text": "done"}]
//!                 })],
//!                 true,
//!                 TokenUsage::default(),
//!             )
//!         })
//!     }
//! }
//! ```
//!
//! A capability implements [`middleware::Middleware`] and joins the declaration-ordered
//! [`middleware::MiddlewareStack`]. Static prompt sections are composed once at agent creation.
//!
//! ```rust,no_run
//! use std::sync::Arc;
//!
//! use mobius::Result;
//! use mobius::middleware::{Middleware, MiddlewareStack, PromptSection, RuntimeContext};
//! use mobius::middleware::messages::Messages;
//!
//! struct Policy;
//!
//! impl Middleware for Policy {
//!     fn name(&self) -> &'static str {
//!         "policy"
//!     }
//!
//!     fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
//!         Ok(Some(PromptSection::new("Follow the repository policy.")))
//!     }
//! }
//!
//! fn middleware_stack() -> Result<MiddlewareStack> {
//!     MiddlewareStack::new(vec![Arc::new(Messages::default()), Arc::new(Policy)])
//! }
//! ```
//!
//! # Runtime contracts
//!
//! - [`Error`] and [`ProviderError`] preserve actionable failure classes and retry metadata;
//!   callers should not infer policy by matching display strings.
//! - [`agent::create_agent`] validates composition and unwinds started middleware on startup
//!   failure. [`agent::AgentSender`] documents bounded submission and sender-drop shutdown;
//!   drain [`agent::AgentEvents::recv`] until the stream closes.
//! - [`backend::checkpoint::CheckpointStore::save_with_events`] is the atomic logical boundary
//!   for checkpoint, transcript, execution, and event state. Backend contracts specify durability
//!   and which optional history operations are supported.
//! - [`backend::sandbox::Sandbox`] owns approval and background-process cleanup around an
//!   injected [`backend::sandbox::SandboxBackend`]. Backends must keep cancellation cleanup for
//!   resources they launch; the default authorized path fails closed.
//! - In `mobius-gateway`, signal shutdown through `GatewayServer::serve_until` and await it;
//!   dropping the serving future does not perform graceful shutdown.

use std::future::Future;
use std::pin::Pin;

pub mod agent;
pub mod backend;
pub mod middleware;
pub mod protocol;

/// A boxed asynchronous operation used by runtime-pluggable interfaces.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A model-provider failure with retry metadata preserved for callers.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ProviderError {
    message: String,
    status: Option<u16>,
    retryable: bool,
    retry_after: Option<String>,
    kind: ProviderErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderErrorKind {
    Other,
    StreamInterrupted,
}

impl ProviderError {
    /// Creates a non-retryable provider failure without an HTTP response.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
            retryable: false,
            retry_after: None,
            kind: ProviderErrorKind::Other,
        }
    }

    /// Creates a retryable provider failure without an HTTP response.
    #[must_use]
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            retryable: true,
            ..Self::new(message)
        }
    }

    /// Creates a retryable response-stream interruption without exposing transport details.
    #[must_use]
    pub fn stream_interrupted(retry_after: Option<String>) -> Self {
        Self {
            message: "model response stream was interrupted".into(),
            status: None,
            retryable: true,
            retry_after,
            kind: ProviderErrorKind::StreamInterrupted,
        }
    }

    pub(crate) fn http(
        message: impl Into<String>,
        status: u16,
        retry_after: Option<String>,
    ) -> Self {
        Self {
            message: message.into(),
            status: Some(status),
            retryable: status == 408 || status == 429 || (500..=599).contains(&status),
            retry_after,
            kind: ProviderErrorKind::Other,
        }
    }

    /// Returns the provider's HTTP status code, when one was received.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        self.status
    }

    /// Reports whether the failure is classified as retryable.
    ///
    /// This does not prove that the original request was unprocessed. Before
    /// replaying, account for partial output and possible remote side effects.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    /// Reports whether a response ended before its completion record arrived.
    #[must_use]
    pub fn is_stream_interrupted(&self) -> bool {
        self.kind == ProviderErrorKind::StreamInterrupted
    }

    /// Returns the provider's raw `Retry-After` header value.
    #[must_use]
    pub fn retry_after(&self) -> Option<&str> {
        self.retry_after.as_deref()
    }
}

impl From<String> for ProviderError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for ProviderError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// Errors returned by möbius modules.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("duplicate registration: {0}")]
    Duplicate(String),
    #[error("unknown registration: {0}")]
    Unknown(String),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("authentication error: {0}")]
    Auth(String),
    #[error("sandbox rejected path: {0}")]
    Sandbox(String),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("checkpoint error: {0}")]
    Checkpoint(String),
    #[error("agent busy: {0}")]
    Busy(String),
    #[error("agent stopped: {0}")]
    Stopped(String),
    #[error("{primary}; rollback failed: {rollback}")]
    Rollback {
        primary: Box<Error>,
        rollback: Box<Error>,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("checkpoint storage error")]
    Sqlite(
        #[source]
        #[from]
        rusqlite::Error,
    ),
}

/// Result type shared by möbius modules.
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn preview_json(value: &serde_json::Value) -> String {
    let value = value.to_string();
    if value.len() <= 10_000 {
        return value;
    }
    format!("{}…", truncate_utf8(&value, 10_000))
}

pub(crate) fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    &value[..value.floor_char_boundary(max_bytes)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_errors_do_not_expose_engine_messages() {
        let error = Error::from(rusqlite::Error::InvalidQuery);

        assert_eq!(error.to_string(), "checkpoint storage error");
    }
}
