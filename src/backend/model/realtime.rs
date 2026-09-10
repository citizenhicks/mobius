//! Provider-owned WebRTC negotiation and authenticated voice sideband control.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Url};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{
    Message, client::IntoClientRequest, protocol::WebSocketConfig,
};

use super::openai_auth::OpenAiAuthorization;
use super::transport::{read_limited, status_error};
use crate::protocol::TokenUsage;
use crate::{Error, ProviderError, Result};

const MAX_SDP_BYTES: usize = 128 * 1024;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_EVENT_BYTES: usize = 256 * 1024;
const MAX_TURNS: usize = 1_024;
const COMMAND_CAPACITY: usize = 32;
const START_TIMEOUT: Duration = Duration::from_secs(30);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const CALL_TIMEOUT: Duration = Duration::from_secs(3_600);

pub(super) const VOICES: &[&str] = &[
    "marin", "alloy", "ash", "ballad", "coral", "echo", "sage", "shimmer", "verse", "cedar",
    "quartz", "ripple", "vesper", "willow", "stone", "gleam", "meridian", "bossa", "tempo",
    "beacon", "delta", "cinder",
];

// AVAS/FramelessBidi uses upstream's V3 transport with the ChatGPT (v1) voice family.
// Its quicksilver=v2 header is not the separate public Realtime V2 protocol.
pub(super) const CODEX_VOICES: &[&str] = &[
    "cove", "juniper", "maple", "spruce", "ember", "vale", "breeze", "arbor", "sol",
];

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Frontend SDP and gateway-owned instructions for one voice call.
pub struct RealtimeVoiceRequest {
    pub session_id: String,
    /// A provider-advertised voice, or `None` for its default.
    pub voice: Option<String>,
    pub offer_sdp: String,
    pub instructions: String,
}

/// A voice call whose provider credentials and call identity remain private.
/// Retain the whole value while using its channels; dropping it hangs up the call.
pub struct RealtimeVoiceCall {
    pub answer_sdp: String,
    pub commands: mpsc::Sender<RealtimeVoiceCommand>,
    pub events: mpsc::Receiver<Result<RealtimeVoiceEvent>>,
    _cancel: oneshot::Sender<()>,
}

impl RealtimeVoiceCall {
    pub(super) fn cleanup_deadline(expires_at: std::time::SystemTime) -> std::time::SystemTime {
        // Closing the socket and authenticating the hangup must finish before key expiry.
        expires_at
            .checked_sub(3 * IO_TIMEOUT)
            .unwrap_or(std::time::UNIX_EPOCH)
    }

    pub(super) fn limit_credential(&mut self, credential: super::ModelCredentialLifetime) {
        if credential.expires_at.is_none() && credential.revoked.is_none() {
            return;
        }
        let (cancel, dropped) = oneshot::channel::<()>();
        let provider_cancel = std::mem::replace(&mut self._cancel, cancel);
        tokio::spawn(async move {
            tokio::select! {
                _ = credential.ended() => {},
                _ = dropped => {},
            }
            drop(provider_cancel);
        });
    }

    /// Creates a provider call with validated audio SDP and bounded Tokio channels.
    /// The provider must stop and hang up when the cancellation receiver resolves,
    /// including when this value drops its sender without sending a message.
    pub fn new(
        answer_sdp: String,
        commands: mpsc::Sender<RealtimeVoiceCommand>,
        events: mpsc::Receiver<Result<RealtimeVoiceEvent>>,
        cancellation: oneshot::Sender<()>,
    ) -> Result<Self> {
        validate_sdp(&answer_sdp)?;
        Ok(Self {
            answer_sdp,
            commands,
            events,
            _cancel: cancellation,
        })
    }
}

/// The coding agent's response to one normalized voice handoff.
pub enum RealtimeVoiceCommand {
    /// Close the provider session, draining its final events before disconnecting.
    Close,
    Reply {
        handoff_id: String,
        text: String,
    },
    /// Background Bot context or progress; it must not initiate a voice response.
    Context {
        text: String,
    },
}

/// Provider-normalized handoffs and usage for one voice call.
#[derive(Debug, PartialEq, Eq)]
pub enum RealtimeVoiceEvent {
    /// Incremental speech text and complete snapshots of provider turns or caption groups.
    Transcript {
        id: String,
        role: crate::protocol::ConversationRole,
        text: String,
        complete: bool,
    },
    Handoff {
        id: String,
        /// An optional provider utterance; use recent voice context to resolve the task.
        text: Option<String>,
    },
    /// Provider-reported tokens, without a local estimate of audio pricing.
    Usage(TokenUsage),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum VoiceApi {
    OpenAi,
    Codex,
}

#[derive(Clone)]
pub(super) struct RealtimeTransport {
    api: VoiceApi,
    client: Client,
    auth: Arc<dyn OpenAiAuthorization>,
    calls_url: Url,
    api_url: Url,
}

impl RealtimeTransport {
    pub(super) fn new(api: VoiceApi, auth: Arc<dyn OpenAiAuthorization>) -> Result<Self> {
        let calls_url = match api {
            VoiceApi::OpenAi => "https://api.openai.com/v1/live/sessions",
            VoiceApi::Codex => {
                "https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas"
            }
        };
        Ok(Self {
            api,
            // Voice credentials must never follow a provider redirect.
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(START_TIMEOUT)
                .build()?,
            auth,
            calls_url: Url::parse(calls_url)
                .map_err(|_| invalid("invalid voice calls endpoint"))?,
            api_url: Url::parse(match api {
                VoiceApi::OpenAi => "https://api.openai.com/v1/live/sessions",
                VoiceApi::Codex => "https://api.openai.com/v1/live",
            })
            .map_err(|_| invalid("invalid voice sideband endpoint"))?,
        })
    }

    pub(super) async fn start(&self, request: RealtimeVoiceRequest) -> Result<RealtimeVoiceCall> {
        timeout(START_TIMEOUT, self.start_inner(request))
            .await
            .map_err(|_| invalid("voice negotiation timed out"))?
    }

    async fn start_inner(&self, request: RealtimeVoiceRequest) -> Result<RealtimeVoiceCall> {
        validate_text(&request.session_id, 256, "session identity")?;
        validate_sdp(&request.offer_sdp)?;
        validate_text(&request.instructions, MAX_TEXT_BYTES, "voice instructions")?;
        if let Some(voice) = request.voice.as_deref()
            && !self.voices().contains(&voice)
        {
            return Err(invalid(
                "the selected voice is not supported by this provider",
            ));
        }
        let session = self.session(&request);
        let body = serde_json::to_vec(&match self.api {
            VoiceApi::Codex => json!({"sdp":request.offer_sdp,"session":session}),
            VoiceApi::OpenAi => {
                json!({"transport":{"type":"webrtc","sdp":request.offer_sdp},"session":session})
            }
        })?;
        if body.len() > 2 * MAX_EVENT_BYTES {
            return Err(invalid("voice request exceeded size limit"));
        }
        let response = self
            .post(
                self.calls_url.clone(),
                body,
                "application/json",
                &request.session_id,
            )
            .await?;
        if !response.status().is_success() {
            return Err(status_error(response, "Realtime").await);
        }
        let (cleanup, answer_sdp) = self.negotiate(response, &request.session_id).await?;
        validate_sdp(&answer_sdp)?;
        let mut socket = self.connect(&cleanup.call_id, &cleanup.session_id).await?;
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, events) = mpsc::channel(16);
        let (cancel, cancelled) = oneshot::channel();
        let api = self.api;
        tokio::spawn(async move {
            let result = tokio::select! {
                _ = cancelled => {
                    let _ = send(&mut socket, json!({"type":"session.close"})).await;
                    Ok(())
                },
                result = timeout(CALL_TIMEOUT, drive(&mut socket, api, command_rx, &event_tx)) => {
                    result.unwrap_or_else(|_| Err(invalid("voice call reached its time limit")))
                }
            };
            if let Err(error) = result {
                let _ = event_tx.send(Err(error)).await;
            }
            let _ = timeout(IO_TIMEOUT, socket.close(None)).await;
            drop(cleanup);
        });
        RealtimeVoiceCall::new(answer_sdp, commands, events, cancel)
    }

    fn voices(&self) -> &'static [&'static str] {
        match self.api {
            VoiceApi::OpenAi => VOICES,
            VoiceApi::Codex => CODEX_VOICES,
        }
    }

    fn session(&self, request: &RealtimeVoiceRequest) -> Value {
        let voice = request.voice.as_deref().unwrap_or(self.voices()[0]);
        let model = match self.api {
            VoiceApi::OpenAi => "gpt-live-1",
            VoiceApi::Codex => "gpt-live-1-codex",
        };
        json!({"model":model,"instructions":request.instructions,
            "audio":{"output":{"voice":voice}},"delegation":{"type":"client"}})
    }

    async fn negotiate(
        &self,
        response: reqwest::Response,
        session_id: &str,
    ) -> Result<(CallCleanup, String)> {
        let call_id = match self.api {
            VoiceApi::Codex => self.call_id(&response)?,
            VoiceApi::OpenAi => {
                let body: Value = serde_json::from_slice(
                    &read_limited(response, MAX_EVENT_BYTES, "Live session").await?,
                )?;
                let id = field(&body["session"], "id")?;
                validate_call_id(id)?;
                let cleanup = CallCleanup {
                    transport: self.clone(),
                    call_id: id.into(),
                    session_id: session_id.into(),
                };
                if body["transport"]["type"] != "webrtc" {
                    return Err(invalid("voice response omitted its WebRTC transport"));
                }
                let sdp = body["transport"]["sdp"]
                    .as_str()
                    .ok_or_else(|| invalid("voice response omitted its SDP"))?;
                return Ok((cleanup, sdp.into()));
            }
        };
        let cleanup = CallCleanup {
            transport: self.clone(),
            call_id,
            session_id: session_id.into(),
        };
        let answer =
            String::from_utf8(read_limited(response, MAX_SDP_BYTES, "Realtime SDP").await?)
                .map_err(|_| invalid("voice answer SDP is not UTF-8"))?;
        Ok((cleanup, answer))
    }

    async fn post(
        &self,
        url: Url,
        body: Vec<u8>,
        content_type: &str,
        session_id: &str,
    ) -> Result<reqwest::Response> {
        for attempt in 0..2 {
            let auth = self.auth.authorize_http(false, Some(session_id)).await?;
            let mut request = self
                .client
                .post(url.clone())
                .bearer_auth(&auth.token)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(body.clone())
                .timeout(START_TIMEOUT);
            for (name, value) in auth.headers {
                request = request.header(name, value);
            }
            if self.api == VoiceApi::Codex {
                request = request
                    .header("openai-alpha", "quicksilver=v2")
                    .header("x-session-id", session_id);
            }
            let response = request.send().await?;
            if response.status() != reqwest::StatusCode::UNAUTHORIZED
                || attempt == 1
                || !self.auth.recover_unauthorized(&auth.token).await?
            {
                return Ok(response);
            }
        }
        unreachable!("authorization retry is bounded")
    }

    fn call_id(&self, response: &reqwest::Response) -> Result<String> {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| invalid("voice response omitted a valid Location"))?;
        let location = self
            .calls_url
            .join(location)
            .map_err(|_| invalid("invalid voice Location"))?;
        if ![self.calls_url.origin(), self.api_url.origin()].contains(&location.origin())
            || !location.username().is_empty()
            || location.password().is_some()
            || location.fragment().is_some()
            || location.query().is_some()
        {
            return Err(invalid("voice Location did not identify the provider call"));
        }
        let (_, id) = location
            .path()
            .rsplit_once('/')
            .ok_or_else(|| invalid("voice Location omitted call identity"))?;
        // Forwarded provider paths vary; credentials only use our fixed endpoint and this ID.
        if id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || !(id.starts_with("rtc_") && id.len() > 4 || uuid::Uuid::parse_str(id).is_ok())
        {
            return Err(invalid("invalid provider voice call identity"));
        }
        Ok(id.into())
    }

    async fn connect(&self, call_id: &str, session_id: &str) -> Result<Socket> {
        let mut url = self.api_url.clone();
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| invalid("invalid voice socket scheme"))?;
        if self.api == VoiceApi::Codex {
            url.set_path(&format!("{}/{call_id}", url.path()));
        } else {
            url.set_path(&format!("{}/{call_id}/attach", url.path()));
        }
        for attempt in 0..2 {
            // Reuse the call-create identity, including the signed-in ChatGPT account header.
            let auth = self.auth.authorize_http(false, Some(session_id)).await?;
            let mut request = url.as_str().into_client_request().map_err(socket_error)?;
            for (name, value) in
                std::iter::once(("authorization".into(), format!("Bearer {}", auth.token)))
                    .chain(auth.headers)
            {
                request.headers_mut().insert(
                    tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes())
                        .map_err(|_| invalid("invalid voice authorization header"))?,
                    value
                        .parse()
                        .map_err(|_| invalid("invalid voice authorization value"))?,
                );
            }
            if self.api == VoiceApi::Codex {
                request.headers_mut().insert(
                    "openai-alpha",
                    "quicksilver=v2".parse().expect("static header"),
                );
                request.headers_mut().insert(
                    "x-session-id",
                    session_id
                        .parse()
                        .map_err(|_| invalid("invalid voice session header"))?,
                );
            }
            let config = WebSocketConfig::default()
                .max_message_size(Some(MAX_EVENT_BYTES))
                .max_frame_size(Some(MAX_EVENT_BYTES));
            match tokio_tungstenite::connect_async_with_config(request, Some(config), false).await {
                Ok((socket, _)) => return Ok(socket),
                Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                    if response.status().as_u16() == 401
                        && attempt == 0
                        && self.auth.recover_unauthorized(&auth.token).await?
                    {
                        continue;
                    }
                    let body = response.body().as_deref().unwrap_or_default();
                    let body = &body[..body.len().min(super::transport::MAX_ERROR_BYTES)];
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    return Err(Error::Provider(ProviderError::http(
                        format!(
                            "Realtime sideband HTTP {}: {}",
                            response.status(),
                            String::from_utf8_lossy(body)
                        ),
                        response.status().as_u16(),
                        retry_after,
                    )));
                }
                Err(error) => return Err(socket_error(error)),
            }
        }
        unreachable!("authorization retry is bounded")
    }
}

struct CallCleanup {
    transport: RealtimeTransport,
    call_id: String,
    session_id: String,
}

impl Drop for CallCleanup {
    fn drop(&mut self) {
        let transport = self.transport.clone();
        let mut url = transport.api_url.clone();
        if transport.api == VoiceApi::Codex {
            let prefix = url.path().rsplit_once('/').map_or("", |(prefix, _)| prefix);
            url.set_path(&format!("{prefix}/realtime/calls/{}/hangup", self.call_id));
        } else {
            url.set_path(&format!("{}/{}/hangup", url.path(), self.call_id));
        }
        let session_id = self.session_id.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = timeout(
                    IO_TIMEOUT,
                    transport.post(url, Vec::new(), "application/json", &session_id),
                )
                .await;
            });
        }
    }
}

fn validate_call_id(id: &str) -> Result<()> {
    validate_text(id, 256, "voice call identity")?;
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(invalid("invalid provider voice call identity"));
    }
    Ok(())
}

fn validate_sdp(sdp: &str) -> Result<()> {
    validate_text(sdp, MAX_SDP_BYTES, "SDP")?;
    if sdp.lines().next() != Some("v=0") || !sdp.lines().any(|line| line.starts_with("m=audio ")) {
        return Err(invalid("voice SDP must contain an audio session"));
    }
    Ok(())
}

fn validate_text(text: &str, limit: usize, label: &str) -> Result<()> {
    if text.trim().is_empty() || text.len() > limit || text.contains('\0') {
        return Err(invalid(&format!("invalid {label} or size limit exceeded")));
    }
    Ok(())
}

fn invalid(message: &str) -> Error {
    Error::Provider(message.into())
}
fn socket_error(_: tokio_tungstenite::tungstenite::Error) -> Error {
    Error::Provider(ProviderError::stream_interrupted(None))
}

async fn send(socket: &mut Socket, value: Value) -> Result<()> {
    let text = serde_json::to_string(&value)?;
    if text.len() > MAX_EVENT_BYTES {
        return Err(invalid("voice command exceeded size limit"));
    }
    timeout(IO_TIMEOUT, socket.send(Message::text(text)))
        .await
        .map_err(|_| invalid("voice send timed out"))?
        .map_err(socket_error)
}

async fn drive(
    socket: &mut Socket,
    api: VoiceApi,
    mut commands: mpsc::Receiver<RealtimeVoiceCommand>,
    events: &mpsc::Sender<Result<RealtimeVoiceEvent>>,
) -> Result<()> {
    let mut turns = VoiceTurns::default();
    loop {
        tokio::select! {
            _ = events.closed() => return Ok(()),
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()); };
                if matches!(command, RealtimeVoiceCommand::Close) {
                    send(socket, json!({"type":"session.close"})).await?;
                    if api == VoiceApi::Codex { return Ok(()); }
                    return timeout(IO_TIMEOUT, drain_closed(socket, api, &mut turns, events))
                        .await.map_err(|_| invalid("voice session finalization timed out"))?;
                }
                let (handoff_id, text) = match command {
                    RealtimeVoiceCommand::Reply { handoff_id, text } => {
                        if !turns.reply_pending.remove(&handoff_id) { return Err(invalid("voice reply has no pending handoff")); }
                        (Some(handoff_id), text)
                    }
                    RealtimeVoiceCommand::Context { text } => (None, text),
                    RealtimeVoiceCommand::Close => unreachable!("handled above"),
                };
                validate_text(&text, MAX_TEXT_BYTES, "voice context")?;
                for chunk in context_chunks(&text) {
                    let value = match (api, handoff_id.as_deref()) {
                        (VoiceApi::Codex, Some(id)) => json!({"type":"delegation.context.append","delegation_item_id":id,"channel":"speakable","content":[{"type":"input_text","text":chunk}]}),
                        (VoiceApi::Codex, None) => json!({"type":"session.context.append","channel":"commentary","content":[{"type":"input_text","text":chunk}]}),
                        (VoiceApi::OpenAi, id) => json!({"type":if id.is_some() {"session.commentary.append"} else {"session.thinking.append"},"delegation_id":id,"content":chunk}),
                    };
                    send(socket, value).await?;
                }
            }

            message = socket.next() => {
                let Some(message) = message else { return Err(invalid("voice sideband closed")); };
                let value = match message.map_err(socket_error)? {
                    Message::Text(text) => serde_json::from_str(&text)?,
                    Message::Binary(bytes) => serde_json::from_slice(&bytes)?,
                    Message::Close(_) => return Ok(()),
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => { timeout(IO_TIMEOUT, socket.flush()).await.map_err(|_| invalid("voice flush timed out"))?.map_err(socket_error)?; continue; }
                };
                for event in turns.observe(api, &value)? {
                    events
                        .send(Ok(event))
                        .await
                        .map_err(|_| invalid("voice event consumer stopped"))?;
                }
                if value["type"] == "session.closed" { return Ok(()); }
            }
        }
    }
}

async fn drain_closed(
    socket: &mut Socket,
    api: VoiceApi,
    turns: &mut VoiceTurns,
    events: &mpsc::Sender<Result<RealtimeVoiceEvent>>,
) -> Result<()> {
    while let Some(message) = socket.next().await {
        let value: Value = match message.map_err(socket_error)? {
            Message::Text(text) => serde_json::from_str(&text)?,
            Message::Binary(bytes) => serde_json::from_slice(&bytes)?,
            Message::Close(_) => break,
            _ => continue,
        };
        for event in turns.observe(api, &value)? {
            events
                .send(Ok(event))
                .await
                .map_err(|_| invalid("voice event consumer stopped"))?;
        }
        if value["type"] == "session.closed" {
            return Ok(());
        }
    }
    Err(invalid("voice disconnected before session finalization"))
}

fn context_chunks(mut text: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    while !text.is_empty() {
        let end = text.floor_char_boundary(500);
        chunks.push(&text[..end]);
        text = &text[end..];
    }
    chunks
}

struct LiveCaption {
    id: String,
    start_ms: u64,
    end_ms: u64,
}

#[derive(Default)]
struct TranscriptState {
    text: String,
    complete: bool,
}

#[derive(Default)]
struct VoiceTurns {
    emitted: BTreeSet<String>,
    reply_pending: BTreeSet<String>,
    streams: BTreeMap<String, TranscriptState>,
    seen_events: BTreeSet<String>,
    live_captions: [Option<LiveCaption>; 2],
    live_counter: u64,
    codex_input: Option<String>,
    codex_output: Option<String>,
    codex_counter: u64,
    codex_completed_turns: BTreeSet<String>,
}

impl VoiceTurns {
    fn observe(&mut self, api: VoiceApi, event: &Value) -> Result<Vec<RealtimeVoiceEvent>> {
        let mut events = Vec::new();
        if let Some(id) = event.get("event_id").and_then(Value::as_str) {
            validate_text(id, 256, "voice event identity")?;
            if !self.seen_events.insert(id.into()) {
                return Ok(events);
            }
            if self.seen_events.len() > MAX_TURNS * 64 {
                return Err(invalid("voice call exceeded its event limit"));
            }
        }
        if matches!(event["type"].as_str(), Some("error")) {
            return Err(invalid(&super::openai::response_error(event)));
        }
        match api {
            VoiceApi::OpenAi => self.observe_live(event, &mut events)?,
            VoiceApi::Codex => self.observe_codex(event, &mut events)?,
        }
        if self.streams.len() > MAX_TURNS * 2 || self.codex_completed_turns.len() > MAX_TURNS * 2 {
            return Err(invalid("voice call exceeded its turn limit"));
        }
        Ok(events)
    }

    fn observe_live(&mut self, event: &Value, events: &mut Vec<RealtimeVoiceEvent>) -> Result<()> {
        match event["type"].as_str() {
            Some("session.input_transcript.delta") => self.live_transcript(event, 0, events)?,
            Some("session.output_transcript.delta") => self.live_transcript(event, 1, events)?,
            Some("session.delegation.created") => {
                let delegation = &event["delegation"];
                if delegation["type"] != "delegation" || delegation["target"] != "client" {
                    return Ok(());
                }
                let id = field(delegation, "id")?;
                if self.emitted.contains(id) {
                    return Ok(());
                }
                let offset = event["offset_ms"]
                    .as_u64()
                    .ok_or_else(|| invalid("voice delegation omitted its timestamp"))?;
                for speaker in 0..2 {
                    if self.live_captions[speaker]
                        .as_ref()
                        .is_some_and(|caption| caption.end_ms <= offset)
                    {
                        self.finish_caption(speaker, events)?;
                    }
                }
                if let Some(event) = self.emit(id, None)? {
                    events.push(event);
                }
            }
            Some("session.closed") => {
                for speaker in 0..2 {
                    self.finish_caption(speaker, events)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn live_transcript(
        &mut self,
        event: &Value,
        speaker: usize,
        events: &mut Vec<RealtimeVoiceEvent>,
    ) -> Result<()> {
        let text = transcript_field(event, "delta")?;
        let start_ms = event["start_ms"]
            .as_u64()
            .ok_or_else(|| invalid("voice transcript omitted its timestamp"))?;
        let end_ms = event["end_ms"]
            .as_u64()
            .filter(|end| *end >= start_ms)
            .ok_or_else(|| invalid("invalid voice transcript interval"))?;
        if text.is_empty() {
            return Ok(());
        }
        // ponytail: caption groups use a one-second gap, not semantic turn detection.
        // Late fragments get a new group; add revisable timeline rows if needed.
        if self.live_captions[speaker].as_ref().is_some_and(|caption| {
            start_ms < caption.start_ms || start_ms > caption.end_ms.saturating_add(1_000)
        }) {
            self.finish_caption(speaker, events)?;
        }
        let caption = self.live_captions[speaker].get_or_insert_with(|| {
            self.live_counter += 1;
            LiveCaption {
                id: format!("live-{speaker}-{}", self.live_counter),
                start_ms,
                end_ms,
            }
        });
        caption.end_ms = caption.end_ms.max(end_ms);
        let id = caption.id.clone();
        self.transcript(&id, live_role(speaker), text, false, events)
    }

    fn finish_caption(
        &mut self,
        speaker: usize,
        events: &mut Vec<RealtimeVoiceEvent>,
    ) -> Result<()> {
        if let Some(caption) = self.live_captions[speaker].take() {
            let text = self
                .streams
                .get(&caption.id)
                .map(|stream| stream.text.clone())
                .unwrap_or_default();
            self.transcript(&caption.id, live_role(speaker), &text, true, events)?;
        }
        Ok(())
    }

    fn codex_id(&mut self, user: bool) -> String {
        self.codex_counter += 1;
        format!(
            "codex-{}-{}",
            if user { "user" } else { "assistant" },
            self.codex_counter
        )
    }

    fn observe_codex(&mut self, event: &Value, events: &mut Vec<RealtimeVoiceEvent>) -> Result<()> {
        use crate::protocol::ConversationRole;
        match event["type"].as_str() {
            Some("input_transcript.added") => {
                let id = self
                    .codex_input
                    .clone()
                    .unwrap_or_else(|| self.codex_id(true));
                self.codex_input = Some(id.clone());
                self.transcript(
                    &id,
                    ConversationRole::User,
                    transcript_field(&event["item"], "text")?,
                    false,
                    events,
                )?;
            }
            Some("output_transcript.added") => {
                let id = self
                    .codex_output
                    .clone()
                    .unwrap_or_else(|| self.codex_id(false));
                self.codex_output = Some(id.clone());
                self.transcript(
                    &id,
                    ConversationRole::Assistant,
                    transcript_field(&event["item"], "text")?,
                    false,
                    events,
                )?;
            }
            Some("turn.done") => self.codex_turn_done(event, events)?,
            Some("delegation.created") => {
                let item = &event["item"];
                if item["type"] != "delegation" || item["target"] != "client" {
                    return Ok(());
                }
                let id = field(item, "id")?;
                if self.emitted.contains(id) {
                    return Ok(());
                }
                let parts = item["content"]
                    .as_array()
                    .ok_or_else(|| invalid("voice delegation omitted its transcript"))?;
                let text = parts
                    .iter()
                    .filter(|part| part["type"] == "input_text")
                    .map(|part| transcript_field(part, "text"))
                    .collect::<Result<Vec<_>>>()?
                    .concat();
                if let Some(event) = self.emit(id, Some(&text))? {
                    events.push(event);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn codex_turn_done(
        &mut self,
        event: &Value,
        events: &mut Vec<RealtimeVoiceEvent>,
    ) -> Result<()> {
        use crate::protocol::ConversationRole;
        let turn = &event["turn"];
        let role = match turn["role"].as_str() {
            Some("user") => ConversationRole::User,
            Some("assistant") => ConversationRole::Assistant,
            _ => return Ok(()),
        };
        let text = transcript_field(turn, "transcript")?;
        if let Some(id) = turn.get("id").and_then(Value::as_str) {
            validate_text(id, 256, "voice turn identity")?;
            if !self.codex_completed_turns.insert(id.into()) {
                return Ok(());
            }
        }
        let id = if role == ConversationRole::User {
            self.codex_input
                .take()
                .unwrap_or_else(|| self.codex_id(true))
        } else {
            self.codex_output
                .take()
                .unwrap_or_else(|| self.codex_id(false))
        };
        self.transcript(&id, role, text, true, events)
    }

    fn transcript(
        &mut self,
        id: &str,
        role: crate::protocol::ConversationRole,
        text: &str,
        complete: bool,
        events: &mut Vec<RealtimeVoiceEvent>,
    ) -> Result<()> {
        validate_text(id, 256, "voice transcript identity")?;
        if text.len() > MAX_TEXT_BYTES || text.contains('\0') {
            return Err(invalid("voice transcript exceeded its size limit"));
        }
        let stream = self.streams.entry(id.into()).or_default();
        if stream.complete {
            return Ok(());
        }
        let had_draft = !stream.text.is_empty();
        if complete {
            stream.text = text.into();
            stream.complete = true;
        } else {
            if stream.text.len() + text.len() > MAX_TEXT_BYTES {
                return Err(invalid("voice transcript exceeded its size limit"));
            }
            stream.text.push_str(text);
        }
        let text = if complete {
            std::mem::take(&mut stream.text)
        } else {
            text.into()
        };
        if !text.is_empty() || complete && had_draft {
            events.push(RealtimeVoiceEvent::Transcript {
                id: id.into(),
                role,
                text,
                complete,
            });
        }
        Ok(())
    }

    fn emit(&mut self, id: &str, text: Option<&str>) -> Result<Option<RealtimeVoiceEvent>> {
        validate_text(id, 256, "voice handoff identity")?;
        if let Some(text) = text {
            validate_text(text, MAX_TEXT_BYTES, "voice transcript")?;
        }
        if self.emitted.contains(id) {
            return Ok(None);
        }
        if self.emitted.len() == MAX_TURNS {
            return Err(invalid("voice call exceeded its turn limit"));
        }
        self.emitted.insert(id.into());
        self.reply_pending.insert(id.into());
        Ok(Some(RealtimeVoiceEvent::Handoff {
            id: id.into(),
            text: text.map(str::to_owned),
        }))
    }
}

fn live_role(speaker: usize) -> crate::protocol::ConversationRole {
    if speaker == 0 {
        crate::protocol::ConversationRole::User
    } else {
        crate::protocol::ConversationRole::Assistant
    }
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let text = value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("voice event omitted a required field"))?;
    validate_text(text, 256, "voice event identity")?;
    Ok(text)
}

fn transcript_field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|text| text.len() <= MAX_TEXT_BYTES && !text.contains('\0'))
        .ok_or_else(|| invalid("invalid voice transcription"))
}

#[cfg(test)]
#[path = "realtime_tests.rs"]
mod tests;
