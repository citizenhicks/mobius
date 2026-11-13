//! Gateway rejection records and error classification.

use crate::Error;
use std::sync::Arc;
use tokio::sync::oneshot;
#[derive(Debug, Clone)]
pub(crate) struct Rejection {
    pub(crate) code: &'static str,
    pub(crate) message: Arc<str>,
    pub(crate) fatal: bool,
}

impl Rejection {
    pub(crate) fn new(code: &'static str, message: impl Into<Arc<str>>) -> Self {
        Self {
            code,
            message: message.into(),
            fatal: false,
        }
    }

    pub(crate) fn fatal(mut self) -> Self {
        self.fatal = true;
        self
    }
}

pub(super) fn session_limit(capacity: usize) -> Rejection {
    Rejection::new(
        "session_limit",
        format!("this gateway already has {capacity} connected or running chats"),
    )
}

pub(super) async fn receive<T>(
    receiver: oneshot::Receiver<std::result::Result<T, Rejection>>,
) -> std::result::Result<T, Rejection> {
    receiver.await.map_err(stopped_by)?
}

pub(super) fn stopped_by(error: impl std::fmt::Display) -> Rejection {
    Rejection::new(
        "gateway_stopped",
        format!("the gateway host stopped: {error}"),
    )
    .fatal()
}

pub(super) fn stopped() -> Rejection {
    Rejection::new("gateway_stopped", "the gateway host stopped").fatal()
}

pub(crate) fn internal(error: impl std::fmt::Display) -> Rejection {
    Rejection::new("gateway_error", error.to_string())
}

/// `harness_unavailable` for a harness without an adapter, else an internal error.
pub(super) fn harness_unavailable(error: Error) -> Rejection {
    match error {
        Error::HarnessUnavailable(message) => Rejection::new("harness_unavailable", message),
        error => internal(error),
    }
}

pub(super) fn invalid_config(error: impl std::fmt::Display) -> Rejection {
    Rejection::new("invalid_config", error.to_string())
}

pub(super) fn bot_delete_rejection(code: &'static str, message: &str) -> Rejection {
    Rejection::new(code, message)
}

pub(super) fn invalid_workspace(error: impl std::fmt::Display) -> Rejection {
    Rejection::new("invalid_workspace", error.to_string())
}

pub(super) fn invalid_session_workspace() -> Rejection {
    Rejection::new(
        "invalid_session_workspace",
        "the requested session belongs to another workspace",
    )
}

pub(super) fn invalid_session_bot() -> Rejection {
    Rejection::new(
        "invalid_session_bot",
        "the requested session Bot identity does not match its durable owner",
    )
}

pub(super) fn unknown_session() -> Rejection {
    Rejection::new("unknown_session", "the requested chat does not exist")
}

pub(super) fn invalid_bot(error: impl std::fmt::Display) -> Rejection {
    Rejection::new("invalid_bot", error.to_string())
}

pub(crate) fn invalid_routine(error: impl std::fmt::Display) -> Rejection {
    Rejection::new("invalid_routine", error.to_string())
}

pub(super) fn memory_error(error: mobius::Error) -> Rejection {
    match error {
        mobius::Error::Tool(message) => Rejection::new("invalid_memory", message),
        error => internal(error),
    }
}

pub(crate) fn session_file_rejection(error: mobius::Error) -> Rejection {
    let code = if matches!(&error, mobius::Error::StorageFull) {
        "storage_full"
    } else {
        "session_file_rejected"
    };
    Rejection {
        code,
        message: match error {
            mobius::Error::Tool(message) => message.into(),
            other => other.to_string().into(),
        },
        fatal: false,
    }
}
