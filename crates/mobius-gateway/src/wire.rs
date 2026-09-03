//! Versioned, bounded JSON frames shared by gateway clients and the server.

mod codec;
mod messages;
mod records;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use futures_util::{Sink, SinkExt as _, Stream, StreamExt as _};
use mobius::backend::checkpoint::StreamMetrics;
use mobius::backend::model::provider::HostedWebSearch;
use mobius::protocol::{
    Event, EventMsg, FrontendContribution, FrontendPreviewUpdate, FrontendSettingOption,
    FrontendSettingValue, FrontendSymbol, FrontendWidget, MiddlewareFeature, ModelChoice, Op,
    RenderedBlock, SessionConfiguredEvent, SessionFileLimits, SessionFileRecord,
    SessionFileReference, Submission, TokenUsage, ToolDiscoveryMode,
};
use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::error::Error as WebSocketError;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::{Error, Result};

use self::codec::deserialize_frame;
pub use self::codec::{FrameReader, read_frame, validate_version, write_frame};
pub(crate) use self::codec::{
    framed_to_websocket, read_frame_with_limit, validate_session_id, websocket_error,
    websocket_to_framed,
};
pub use self::messages::{ClientFrame, ClientMessage, ServerFrame, ServerMessage};
pub use self::records::*;

mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub(super) fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(serde::de::Error::custom)
    }
}

/// Current gateway protocol version.
pub const PROTOCOL_VERSION: u16 = 63;
/// Maximum encoded JSON payload accepted in one frame.
pub const MAX_FRAME_BYTES: usize = 50 * 1024 * 1024;
const WEBSOCKET_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests;
