//! The desktop app's gateway client: transport, the state the gateway reports, the requests
//! a window waits on, and this Mac's files. No interface code; `mobius-desktop` draws it.

mod client;
mod connection;
mod desktop;
mod files;
#[cfg(any(test, feature = "snapshot"))]
pub mod fixture;
pub mod local;
pub mod numeric;
pub mod preferences;
mod preview;
mod recap;
mod state;
pub mod storage;
mod transcript;
mod upload;

pub use client::{Arrival, Client, Link, PendingMessage};
pub use connection::{Connection, Gateway, Update, run_embedded, saved_endpoints, set_admission};
pub use desktop::{DesktopEvent, DesktopViewer};
pub use preview::{PreviewData, PreviewRequest};
pub use recap::RecentChat;
pub use state::{GatewayState, Preview};
pub use transcript::{RowChild, TranscriptEntry, TranscriptStore};
pub use upload::UploadFailure;

use std::sync::{Arc, OnceLock};

use mobius::SessionId;
use mobius::protocol::{Op, Submission};
use mobius_gateway::wire::ClientMessage;

static SHARED_RUNTIME: OnceLock<std::result::Result<tokio::runtime::Runtime, String>> =
    OnceLock::new();

/// Returns the process-wide Tokio runtime handle shared by the gateway client and desktop host.
///
/// # Errors
///
/// Returns an error if the shared Tokio runtime could not be initialized.
pub fn shared_runtime() -> mobius_gateway::Result<&'static tokio::runtime::Handle> {
    let runtime = SHARED_RUNTIME.get_or_init(|| {
        let workers =
            std::thread::available_parallelism().map_or(4, |count| count.get().clamp(2, 4));
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("mobius-async")
            .enable_all()
            .build()
            .map_err(|error| error.to_string())
    });
    match runtime {
        Ok(runtime) => Ok(runtime.handle()),
        Err(error) => Err(mobius_gateway::Error::Internal(format!(
            "failed to initialize shared async runtime: {error}"
        ))),
    }
}

pub fn request_id() -> Arc<str> {
    Arc::from(
        &*uuid::Uuid::new_v4()
            .hyphenated()
            .encode_lower(&mut uuid::Uuid::encode_buffer()),
    )
}

/// A side connection with no catalog: skipped at Ready and in later broadcasts.
fn catalog_free() -> mobius_gateway::client::ConnectOptions {
    use mobius_gateway::wire::{CatalogHint, GatewayNotification, READY_SECTIONS};
    mobius_gateway::client::ConnectOptions {
        catalog: CatalogHint {
            known: Default::default(),
            skip: READY_SECTIONS.into(),
        },
        pipelined: vec![ClientMessage::SetNotifications {
            request_id: request_id(),
            disabled: [GatewayNotification::Sessions, GatewayNotification::Bots].into(),
        }],
    }
}

pub fn submit(session_id: SessionId, op: Op) -> ClientMessage {
    ClientMessage::Submit {
        session_id,
        submission: Submission {
            id: request_id(),
            op,
        },
    }
}

/// What a hosted gateway answers while its machine wakes: the transport follows its retry hint.
pub const GATEWAY_STARTING: &str = "Your gateway is starting.";

/// A gateway error as a sentence for people, without the library's "gateway protocol
/// error:" framing that means nothing outside a log.
pub fn describe(error: &mobius_gateway::Error) -> String {
    use mobius_gateway::Error;
    match error {
        Error::Invalid(message) | Error::Protocol(message) => sentence(message),
        Error::Internal(_) => sentence(&error.to_string()),
        Error::WebSocketUpgrade { status, .. } => {
            format!("WebSocket handshake failed: HTTP {status}.")
        }
        Error::Unauthorized => "This gateway no longer accepts this Mac. Pair it again.".into(),
        Error::Io(error) => describe_io(error),
        other => sentence(&other.to_string()),
    }
}

/// A file error in plain words.
pub fn describe_io(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "The file no longer exists.".into(),
        std::io::ErrorKind::PermissionDenied => {
            "Möbius doesn’t have permission to open this file.".into()
        }
        _ => sentence(&error.to_string()),
    }
}

/// [`describe`] for a message that was already formatted, such as a queued toast.
pub fn friendly(message: &str) -> String {
    let message = ["gateway protocol error: ", "gateway configuration error: "]
        .iter()
        .find_map(|prefix| message.strip_prefix(prefix))
        .unwrap_or(message);
    sentence(message)
}

fn sentence(message: &str) -> String {
    let mut chars = message.trim().chars();
    let Some(first) = chars.next() else {
        return "Something went wrong.".into();
    };
    let mut text: String = first.to_uppercase().chain(chars).collect();
    if !text.ends_with(['.', '!', '?', '…']) {
        text.push('.');
    }
    text
}
