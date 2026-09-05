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

use super::ToolDefinition;
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
    pub handoff_tool: ToolDefinition,
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
    /// Incremental speech text, followed by its authoritative complete transcript.
    Transcript {
        id: String,
        role: crate::protocol::ConversationRole,
        text: String,
        complete: bool,
    },
    Handoff {
        id: String,
        text: String,
        /// The provider supplied an utterance that needs its private conversation context.
        needs_context: bool,
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
            VoiceApi::OpenAi => "https://api.openai.com/v1/realtime/calls",
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
                VoiceApi::OpenAi => "https://api.openai.com/v1/realtime",
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
        validate_text(&request.handoff_tool.name, 128, "voice tool name")?;
        if let Some(voice) = request.voice.as_deref()
            && !self.voices().contains(&voice)
        {
            return Err(invalid(
                "the selected voice is not supported by this provider",
            ));
        }
        let session = self.session(&request);
        let (body, content_type) = match self.api {
            VoiceApi::Codex => (
                serde_json::to_vec(&json!({"sdp":request.offer_sdp,"session":session}))?,
                "application/json".into(),
            ),
            VoiceApi::OpenAi => multipart(&request.offer_sdp, &session)?,
        };
        if body.len() > 2 * MAX_EVENT_BYTES {
            return Err(invalid("voice request exceeded size limit"));
        }
        let response = self
            .post(
                self.calls_url.clone(),
                body,
                &content_type,
                &request.session_id,
            )
            .await?;
        if !response.status().is_success() {
            return Err(status_error(response, "Realtime").await);
        }
        let call_id = self.call_id(&response)?;
        // The lease also hangs up if negotiation is cancelled after allocation but before return.
        let cleanup = CallCleanup {
            transport: self.clone(),
            call_id,
            session_id: request.session_id,
        };
        let answer_sdp =
            String::from_utf8(read_limited(response, MAX_SDP_BYTES, "Realtime SDP").await?)
                .map_err(|_| invalid("voice answer SDP is not UTF-8"))?;
        validate_sdp(&answer_sdp)?;
        let mut socket = self.connect(&cleanup.call_id, &cleanup.session_id).await?;
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, events) = mpsc::channel(16);
        let (cancel, cancelled) = oneshot::channel();
        let api = self.api;
        tokio::spawn(async move {
            let result = tokio::select! {
                _ = cancelled => Ok(()),
                result = timeout(CALL_TIMEOUT, drive(&mut socket, api, &request.handoff_tool.name, command_rx, &event_tx)) => {
                    result.unwrap_or_else(|_| Err(invalid("voice call reached its time limit")))
                }
            };
            if let Err(error) = result {
                let _ = event_tx.try_send(Err(error));
            }
            if api == VoiceApi::Codex {
                let _ = send(&mut socket, json!({"type":"session.close"})).await;
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
        if self.api == VoiceApi::Codex {
            // AVAS owns native delegation; public Realtime tool/session fields do not apply.
            return json!({"model":"gpt-live-1-codex","instructions":request.instructions,
                "audio":{"output":{"voice":voice}},"delegation":{"type":"client"}});
        }
        json!({"type":"realtime","model":"gpt-realtime-2.1-mini","instructions":request.instructions,
            "audio":{"input":{"transcription":{"model":"gpt-live-transcribe"},
                "turn_detection":{"type":"server_vad","create_response":true,"interrupt_response":true}},
                "output":{"voice":voice}},
            "tools":[{"type":"function","name":request.handoff_tool.name,"description":request.handoff_tool.description,"parameters":request.handoff_tool.parameters}],
            "tool_choice":"auto"
        })
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
            url.query_pairs_mut().append_pair("call_id", call_id);
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
        let prefix = url.path().rsplit_once('/').map_or("", |(prefix, _)| prefix);
        url.set_path(&format!("{prefix}/realtime/calls/{}/hangup", self.call_id));
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

fn multipart(sdp: &str, session: &Value) -> Result<(Vec<u8>, String)> {
    let boundary = format!("mobius-{}", uuid::Uuid::new_v4());
    let session = serde_json::to_string(session)?;
    Ok((format!("--{boundary}\r\nContent-Disposition: form-data; name=\"sdp\"\r\nContent-Type: application/sdp\r\n\r\n{sdp}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"session\"\r\nContent-Type: application/json\r\n\r\n{session}\r\n--{boundary}--\r\n").into_bytes(), format!("multipart/form-data; boundary={boundary}")))
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
    tool: &str,
    mut commands: mpsc::Receiver<RealtimeVoiceCommand>,
    events: &mpsc::Sender<Result<RealtimeVoiceEvent>>,
) -> Result<()> {
    let mut turns = VoiceTurns::default();
    loop {
        tokio::select! {
            _ = events.closed() => return Ok(()),
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()); };
                let mut next = Some(command);
                // Several handoffs may share one coding reply; append every output before speaking.
                for index in 0..COMMAND_CAPACITY {
                    let Some(command) = next else { break; };
                    match command {
                        RealtimeVoiceCommand::Context { text } => {
                            validate_text(&text, MAX_TEXT_BYTES, "voice context")?;
                            if api == VoiceApi::Codex {
                                for chunk in context_chunks(&text) {
                                    send(socket, json!({"type":"session.context.append","channel":"commentary","content":[{"type":"input_text","text":chunk}]})).await?;
                                }
                            } else {
                                send(socket, json!({"type":"conversation.item.create","item":{"type":"message","role":"system","content":[{"type":"input_text","text":text}]}})).await?;
                            }
                        }
                        RealtimeVoiceCommand::Reply { handoff_id, text } => {
                            validate_text(&text, MAX_TEXT_BYTES, "voice reply")?;
                            if !turns.reply_pending.remove(&handoff_id) { return Err(invalid("voice reply has no pending handoff")); }
                            if api == VoiceApi::Codex {
                                for chunk in context_chunks(&text) {
                                    send(socket, json!({"type":"delegation.context.append","delegation_item_id":handoff_id,"channel":"speakable","content":[{"type":"input_text","text":chunk}]})).await?;
                                }
                            } else {
                                send(socket, json!({"type":"conversation.item.create","item":{"type":"function_call_output","call_id":handoff_id,"output":text}})).await?;
                                turns.reply_ready = true;
                            }
                        }
                    }
                    next = if index + 1 < COMMAND_CAPACITY { commands.try_recv().ok() } else { None };
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
                for event in turns.observe(api, tool, &value)? {
                    events.try_send(Ok(event)).map_err(|_| invalid("voice event consumer stopped or fell behind"))?;
                }
                if value["type"] == "response.done" && value["response"]["status"] == "failed" {
                    return Err(invalid(&super::openai::response_error(&value["response"]["status_details"])));
                }
            }
        }
        if turns.take_reply_response() {
            send(
                socket,
                json!({"type":"response.create","response":{"tool_choice":"none"}}),
            )
            .await?;
        }
    }
}

fn context_chunks(mut text: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    while !text.is_empty() {
        let mut end = text.len().min(500);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&text[..end]);
        text = &text[end..];
    }
    chunks
}

#[derive(Default)]
enum ResponseState {
    #[default]
    Idle,
    Requested,
    Active(String),
}

#[derive(Default)]
struct TranscriptState {
    text: String,
    complete: bool,
}

#[derive(Default)]
struct VoiceTurns {
    response: ResponseState,
    reply_ready: bool,
    emitted: BTreeSet<String>,
    reply_pending: BTreeSet<String>,
    usage_recorded: BTreeSet<(&'static str, String, u64)>,
    streams: BTreeMap<String, TranscriptState>,
    seen_events: BTreeSet<String>,
    codex_input: Option<String>,
    codex_output: Option<String>,
    codex_counter: u64,
    codex_completed_turns: BTreeSet<String>,
}

impl VoiceTurns {
    fn observe(
        &mut self,
        api: VoiceApi,
        tool: &str,
        event: &Value,
    ) -> Result<Vec<RealtimeVoiceEvent>> {
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
        if matches!(
            event["type"].as_str(),
            Some("error" | "conversation.item.input_audio_transcription.failed")
        ) {
            return Err(invalid(&super::openai::response_error(event)));
        }
        match api {
            VoiceApi::OpenAi => self.observe_public(tool, event, &mut events)?,
            VoiceApi::Codex => self.observe_codex(event, &mut events)?,
        }
        if self.streams.len() > MAX_TURNS * 2 || self.codex_completed_turns.len() > MAX_TURNS * 2 {
            return Err(invalid("voice call exceeded its turn limit"));
        }
        Ok(events)
    }

    fn observe_public(
        &mut self,
        tool: &str,
        event: &Value,
        events: &mut Vec<RealtimeVoiceEvent>,
    ) -> Result<()> {
        use crate::protocol::ConversationRole;
        match event["type"].as_str() {
            Some("response.created") => {
                let id = field(&event["response"], "id")?;
                self.response = ResponseState::Active(id.into());
            }
            Some("conversation.item.input_audio_transcription.delta") => {
                let id = field(event, "item_id")?;
                self.transcript(
                    id,
                    ConversationRole::User,
                    transcript_field(event, "delta")?,
                    false,
                    events,
                )?;
            }
            Some("conversation.item.input_audio_transcription.completed") => {
                let id = field(event, "item_id")?;
                let text = transcript_field(event, "transcript")?;
                self.transcript(id, ConversationRole::User, text, true, events)?;
                if event["usage"]["type"] == "tokens" {
                    let index = event["content_index"]
                        .as_u64()
                        .ok_or_else(|| invalid("voice transcription omitted its content index"))?;
                    self.record_usage(("transcript", id.into(), index), &event["usage"], events)?;
                }
            }
            Some("response.output_audio_transcript.delta" | "response.output_text.delta") => {
                self.public_output(event, false, events)?;
            }
            Some("response.output_audio_transcript.done" | "response.output_text.done") => {
                self.public_output(event, true, events)?;
            }
            Some("response.done") => self.public_response_done(tool, event, events)?,
            _ => {}
        }
        Ok(())
    }

    fn public_output(
        &mut self,
        event: &Value,
        complete: bool,
        events: &mut Vec<RealtimeVoiceEvent>,
    ) -> Result<()> {
        let item = field(event, "item_id")?;
        let index = event["content_index"]
            .as_u64()
            .ok_or_else(|| invalid("voice output omitted content index"))?;
        let id = format!("{item}:{index}");
        let key = if !complete {
            "delta"
        } else if event["type"] == "response.output_text.done" {
            "text"
        } else {
            "transcript"
        };
        self.transcript(
            &id,
            crate::protocol::ConversationRole::Assistant,
            transcript_field(event, key)?,
            complete,
            events,
        )
    }

    fn public_response_done(
        &mut self,
        tool: &str,
        event: &Value,
        events: &mut Vec<RealtimeVoiceEvent>,
    ) -> Result<()> {
        let response = &event["response"];
        let id = field(response, "id")?;
        if matches!(&self.response, ResponseState::Active(active) if active == id) {
            self.response = ResponseState::Idle;
        }
        if let Some(usage) = response.get("usage").filter(|usage| !usage.is_null()) {
            self.record_usage(("response", id.into(), 0), usage, events)?;
        }
        for output in response["output"].as_array().into_iter().flatten() {
            if output["type"] == "function_call"
                && output["name"] == tool
                && response["status"] == "completed"
            {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Task {
                    text: String,
                }
                let arguments = output["arguments"]
                    .as_str()
                    .ok_or_else(|| invalid("voice handoff omitted its task arguments"))?;
                let task: Task = serde_json::from_str(arguments)
                    .map_err(|_| invalid("voice handoff omitted its complete task text"))?;
                if let Some(event) = self.emit(field(output, "call_id")?, &task.text, false)? {
                    events.push(event);
                }
            } else if output["type"] == "message" && output["role"] == "assistant" {
                let item = field(output, "id")?;
                for (index, part) in output["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let key = match part["type"].as_str() {
                        Some("output_audio" | "audio") => "transcript",
                        Some("output_text" | "text") => "text",
                        _ => continue,
                    };
                    if let Some(text) = part.get(key).and_then(Value::as_str) {
                        self.transcript(
                            &format!("{item}:{index}"),
                            crate::protocol::ConversationRole::Assistant,
                            text,
                            true,
                            events,
                        )?;
                    }
                }
            }
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
                if let Some(event) = self.emit(id, &text, true)? {
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
        if complete {
            if !text.trim().is_empty() {
                stream.text = text.into();
            }
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
        if !text.is_empty() {
            events.push(RealtimeVoiceEvent::Transcript {
                id: id.into(),
                role,
                text,
                complete,
            });
        }
        Ok(())
    }

    fn take_reply_response(&mut self) -> bool {
        if self.reply_ready && matches!(self.response, ResponseState::Idle) {
            self.reply_ready = false;
            self.response = ResponseState::Requested;
            true
        } else {
            false
        }
    }
    fn record_usage(
        &mut self,
        key: (&'static str, String, u64),
        value: &Value,
        events: &mut Vec<RealtimeVoiceEvent>,
    ) -> Result<()> {
        if self.usage_recorded.contains(&key) {
            return Ok(());
        }
        let count = |pointer| {
            super::usage_i64(Some(value), pointer, "Realtime").map(|n| n.unwrap_or_default())
        };
        let usage = TokenUsage {
            input_tokens: count("/input_tokens")?,
            cached_input_tokens: count("/input_token_details/cached_tokens")?,
            output_tokens: count("/output_tokens")?,
            total_tokens: count("/total_tokens")?,
            ..TokenUsage::default()
        };
        super::validate_usage(&usage)?;
        if usage.cached_input_tokens > usage.input_tokens
            || usage.input_tokens.checked_add(usage.output_tokens) != Some(usage.total_tokens)
        {
            return Err(invalid("voice provider returned inconsistent token usage"));
        }
        if self.usage_recorded.len() == 2 * MAX_TURNS {
            return Err(invalid("voice call exceeded its usage event limit"));
        }
        self.usage_recorded.insert(key);
        events.push(RealtimeVoiceEvent::Usage(usage));
        Ok(())
    }

    fn emit(
        &mut self,
        id: &str,
        text: &str,
        needs_context: bool,
    ) -> Result<Option<RealtimeVoiceEvent>> {
        validate_text(id, 256, "voice handoff identity")?;
        validate_text(text, MAX_TEXT_BYTES, "voice transcript")?;
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
            text: text.into(),
            needs_context,
        }))
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
