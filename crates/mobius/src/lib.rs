//! A small, modular Rust framework for one linear agent session.
//!
//! Applications compose an [`agent::Agent`] from explicit model, sandbox, checkpoint, and
//! middleware adapters. Frontends remain separate: they submit [`protocol::Op`] values and
//! render the frontend-neutral [`protocol::Event`] stream.
//!
//! # Embedded composition
//!
//! The caller owns every runtime dependency. Include exactly one message-handling middleware,
//! give new sessions a non-empty [`protocol::SessionContext::owner_id`], and keep draining events
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
//!     state_dir: &Path,
//!     api_key: String,
//!     model_id: &str,
//! ) -> Result<Agent> {
//!     let model: Arc<dyn Model> = Arc::new(OpenAi::new(
//!         api_key,
//!         "https://api.openai.com/v1".parse()?,
//!         model_id,
//!     )?);
//!     let files = mobius::backend::session_files::SessionFileStore::new(state_dir, None);
//!     let models = Arc::new(ModelRouter::new("default", model).session_files(files.clone()));
//!     let sandbox = Arc::new(Sandbox::new(
//!         Arc::new(LocalSandbox::new(workspace)?),
//!         ApprovalPolicy::Ask,
//!     ));
//!     let checkpoints: Arc<dyn CheckpointStore> =
//!         Arc::new(SqliteCheckpoint::new(state_dir.join("mobius.sqlite3"))?);
//!     let middleware: Vec<Arc<dyn Middleware>> = vec![
//!         Arc::new(Messages::default()),
//!         Arc::new(Tools::coding(files)),
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
//!             owner_id: "embedded".into(),
//!             ..SessionContext::default()
//!         }),
//!     )
//!     .await
//! }
//! ```
//!
//! A custom provider implements [`backend::model::Model`] and must return normalized output.
//! [`backend::model::ModelEventSink`] returns a fallible future: await each event and propagate
//! its error, as [its contract](backend::model::ModelEventSink) requires for backpressure. This
//! example also uses `serde_json`.
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
//!             events(ModelEvent::TextDelta("done".into())).await?;
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
pub mod bounded_json;
pub(crate) use bounded_json::serialized_len;
pub mod config;
pub mod identifier;
pub mod middleware;
pub mod owner_only;
pub mod protocol;
mod session_id;
pub use session_id::SessionId;

/// A boxed asynchronous operation used by runtime-pluggable interfaces.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A model-provider failure with retry metadata preserved for callers.
#[derive(Debug, thiserror::Error)]
#[error("{}", .0.message)]
pub struct ProviderError(Box<ProviderErrorInner>);

#[derive(Debug)]
struct ProviderErrorInner {
    message: String,
    status: Option<u16>,
    retryable: bool,
    retry_after: Option<String>,
    class: FailureClass,
}

/// Why a provider call failed, so callers and frontends pick a recovery without parsing
/// messages. Classified once, where the provider's status and error code arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FailureClass {
    /// A timeout, connection failure, 408 or 5xx: the provider itself may be down.
    Transient,
    /// The provider is rate limiting this account.
    RateLimited,
    /// A response stream ended before its completion record.
    StreamInterrupted,
    /// The credential was rejected or has expired.
    Authentication,
    /// The provider refused this account or request.
    Forbidden,
    /// The account has no remaining quota or credit.
    OutOfCredits,
    /// The request exceeded the model's context window.
    ContextLengthExceeded,
    /// The provider refused the content.
    ContentPolicy,
    /// Any other request the provider rejected.
    InvalidRequest,
    /// Not classified.
    Other,
}

impl ProviderError {
    /// Creates a non-retryable provider failure without an HTTP response.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(Box::new(ProviderErrorInner {
            message: message.into(),
            status: None,
            retryable: false,
            retry_after: None,
            class: FailureClass::Other,
        }))
    }

    /// Creates a retryable provider failure without an HTTP response.
    #[must_use]
    pub fn retryable(message: impl Into<String>) -> Self {
        Self(Box::new(ProviderErrorInner {
            message: message.into(),
            status: None,
            retryable: true,
            retry_after: None,
            class: FailureClass::Transient,
        }))
    }

    pub(crate) fn circuit_open(message: impl Into<String>, retry_after: String) -> Self {
        Self(Box::new(ProviderErrorInner {
            message: message.into(),
            status: None,
            retryable: false,
            retry_after: Some(retry_after),
            class: FailureClass::Transient,
        }))
    }

    /// Creates a retryable response-stream interruption without exposing transport details.
    #[must_use]
    pub fn stream_interrupted(retry_after: Option<String>) -> Self {
        Self(Box::new(ProviderErrorInner {
            message: "model response stream was interrupted".into(),
            status: None,
            retryable: true,
            retry_after,
            class: FailureClass::StreamInterrupted,
        }))
    }

    pub(crate) fn http(
        message: impl Into<String>,
        status: u16,
        retry_after: Option<String>,
    ) -> Self {
        let message = message.into();
        let class = match status {
            401 => FailureClass::Authentication,
            402 => FailureClass::OutOfCredits,
            403 => FailureClass::Forbidden,
            429 => FailureClass::RateLimited,
            408 | 500..=599 => FailureClass::Transient,
            // ponytail: providers without a context-length code are matched on their message.
            400..=499 if Self::mentions_context_length(&message) => {
                FailureClass::ContextLengthExceeded
            }
            // ponytail: providers without a credit code are matched on their message too.
            400..=499 if Self::mentions_credit(&message) => FailureClass::OutOfCredits,
            400..=499 => FailureClass::InvalidRequest,
            _ => FailureClass::Other,
        };
        Self(Box::new(ProviderErrorInner {
            message,
            status: Some(status),
            retryable: status == 408 || status == 429 || (500..=599).contains(&status),
            retry_after,
            class,
        }))
    }

    fn mentions_context_length(message: &str) -> bool {
        let message = message.to_ascii_lowercase();
        ["prompt is too long", "context length", "context window"]
            .iter()
            .any(|needle| message.contains(needle))
    }

    fn mentions_credit(message: &str) -> bool {
        let message = message.to_ascii_lowercase();
        ["requires more credits", "insufficient credits"]
            .iter()
            .any(|needle| message.contains(needle))
    }

    pub(crate) fn with_code(mut self, code: Option<&str>) -> Self {
        let Some(code) = code else {
            return self;
        };
        if Self::is_quota_code(code) {
            self.0.retryable = false;
            self.0.class = FailureClass::OutOfCredits;
            return self;
        }
        match code {
            "context_length_exceeded" => self.0.class = FailureClass::ContextLengthExceeded,
            "content_policy_violation" | "content_filter" => {
                self.0.class = FailureClass::ContentPolicy;
            }
            "authentication_error"
            | "permission_error"
            | "invalid_api_key"
            | "expired_secret_key" => {
                self.0.class = FailureClass::Authentication;
            }
            "invalid_request_error" if self.0.class == FailureClass::Other => {
                self.0.class = FailureClass::InvalidRequest;
            }
            "rate_limit_error" | "rate_limit_exceeded" => {
                self.0.class = FailureClass::RateLimited;
                self.0.retryable = true;
            }
            "overloaded_error" | "server_error" => {
                self.0.class = FailureClass::Transient;
                self.0.retryable = true;
            }
            _ => {}
        }
        self
    }

    pub(crate) fn is_quota_code(code: &str) -> bool {
        matches!(
            code,
            "insufficient_quota"
                | "quota_exceeded"
                | "billing_hard_limit_reached"
                | "billing_not_active"
                | "usage_limit_reached"
                | "credit_limit_exceeded"
        )
    }

    /// Returns the provider's HTTP status code, when one was received.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        self.0.status
    }

    /// Reports whether the failure is classified as retryable.
    ///
    /// This does not prove that the original request was unprocessed. Before
    /// replaying, account for partial output and possible remote side effects.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.0.retryable
    }

    /// Reports whether a response ended before its completion record arrived.
    #[must_use]
    pub fn is_stream_interrupted(&self) -> bool {
        self.0.class == FailureClass::StreamInterrupted
    }

    /// Returns why the call failed.
    #[must_use]
    pub fn class(&self) -> FailureClass {
        self.0.class
    }

    /// Returns the provider's raw `Retry-After` header value.
    #[must_use]
    pub fn retry_after(&self) -> Option<&str> {
        self.0.retry_after.as_deref()
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
    /// An internal mutex was poisoned by a panic.
    #[error("internal lock poisoned: {0}")]
    Poisoned(&'static str),
    /// The gateway data allowance cannot admit another file.
    #[error("storage is full; free up space before adding files")]
    StorageFull,
    /// A request, setting or input the caller can fix; the message says what to change.
    #[error("{0}")]
    Invalid(String),
    /// A failure the caller cannot fix: a broken invariant, damaged saved state or failed system call.
    #[error("internal error: {0}")]
    Internal(String),
    #[error("duplicate registration: {0}")]
    /// Selects the duplicate case.
    Duplicate(String),
    #[error("unknown registration: {0}")]
    /// Selects the unknown case.
    Unknown(String),
    #[error("provider error: {0}")]
    /// Selects the provider case.
    Provider(#[from] ProviderError),
    #[error("authentication error: {0}")]
    /// Selects the auth case.
    Auth(String),
    #[error("sandbox: {0}")]
    /// Selects the sandbox case.
    Sandbox(String),
    #[error("tool error: {0}")]
    /// Selects the tool case.
    Tool(String),
    #[error("checkpoint error: {0}")]
    /// Selects the checkpoint case.
    Checkpoint(String),
    #[error("agent busy: {0}")]
    /// Selects the busy case.
    Busy(String),
    #[error("agent stopped: {0}")]
    /// Selects the stopped case.
    Stopped(String),
    #[error("{primary}; rollback failed: {rollback}")]
    /// Selects the rollback case.
    Rollback {
        /// The primary.
        primary: Box<Self>,
        /// The rollback.
        rollback: Box<Self>,
    },
    /// The destination was replaced, but its directory could not be synchronized.
    /// Callers must retain the applied state and must not blindly retry the mutation.
    #[error("state was replaced, but its durability could not be confirmed: {0}")]
    PublicationDurability(#[source] std::io::Error),
    #[error(transparent)]
    /// Selects the I/O case.
    Io(#[from] std::io::Error),
    #[error(transparent)]
    /// Selects the HTTP case.
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    /// Selects the JSON case.
    Json(#[from] serde_json::Error),
    #[error("checkpoint storage error")]
    /// Selects the SQLite case.
    Sqlite(
        #[source]
        #[from]
        rusqlite::Error,
    ),
}

impl Error {
    /// Why a model provider call failed, when this error is one.
    #[must_use]
    pub fn failure(&self) -> Option<FailureClass> {
        match self {
            Self::Provider(error) => Some(error.class()),
            Self::Auth(_) => Some(FailureClass::Authentication),
            _ => None,
        }
    }
}

/// Result type shared by möbius modules.
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) const MAX_IDENTIFIER_BYTES: usize = 4 * 1024;

/// Whether the path is relative and every component stays under its starting directory.
pub(crate) fn is_confined_relative(path: &std::path::Path) -> bool {
    use std::path::Component;
    path.components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
}

pub(crate) fn validate_identifier(name: &str, value: &str, limit: usize) -> Result<()> {
    if value.trim().is_empty() {
        return Err(Error::Invalid(format!("{name} cannot be empty")));
    }
    if value.len() > limit {
        return Err(Error::Invalid(format!("{name} exceeds size limit")));
    }
    Ok(())
}

/// Reads up to the byte limit and returns `None` when the input exceeds it.
///
/// # Errors
///
/// Returns the underlying I/O failure if reading the input fails.
pub fn read_within(reader: impl std::io::Read, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

pub(crate) fn preview_json(value: &serde_json::Value) -> String {
    let value = value.to_string();
    if value.len() <= 10_000 {
        return value;
    }
    format!("{}…", truncate_utf8(&value, 10_000))
}

pub(crate) fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    value.split_at(value.floor_char_boundary(max_bytes)).0
}

#[cfg(test)]
mod provider_error_tests {
    use super::*;

    #[test]
    fn provider_error_with_code_marks_transient_and_rate_limited_retryable() {
        let rate = ProviderError::new("slow down").with_code(Some("rate_limit_error"));
        assert!(rate.is_retryable(), "rate_limit_error should be retryable");
        assert_eq!(rate.class(), FailureClass::RateLimited);

        let overloaded = ProviderError::new("busy").with_code(Some("overloaded_error"));
        assert!(
            overloaded.is_retryable(),
            "overloaded_error should be retryable"
        );
        assert_eq!(overloaded.class(), FailureClass::Transient);

        let quota = ProviderError::retryable("quota").with_code(Some("insufficient_quota"));
        assert!(
            !quota.is_retryable(),
            "insufficient_quota must not be retryable"
        );
        assert_eq!(quota.class(), FailureClass::OutOfCredits);
    }
}
