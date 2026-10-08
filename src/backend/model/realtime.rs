//! Provider-owned WebRTC negotiation and authenticated voice sideband control.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VoiceManifest {
    openai: VoiceProvider,
    codex: VoiceProvider,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VoiceProvider {
    models: Vec<super::provider::MediaModelPreset>,
    calls_path: String,
    sideband_base_url: Option<String>,
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
}

static MANIFEST: std::sync::LazyLock<VoiceManifest> = std::sync::LazyLock::new(|| {
    crate::config::overridable(
        "realtime.toml",
        include_str!("realtime.toml"),
        VoiceManifest::validate,
    )
});

impl VoiceProvider {
    /// Every voice of every model, the default model's default voice first.
    fn voices(&'static self) -> Vec<&'static str> {
        let mut voices = Vec::new();
        for voice in self.models.iter().flat_map(|model| &model.variants) {
            if !voices.contains(&voice.id.as_str()) {
                voices.push(voice.id.as_str());
            }
        }
        voices
    }
}

impl VoiceManifest {
    fn validate(&self) -> Result<()> {
        for api in [&self.openai, &self.codex] {
            if api.models.is_empty() || api.models.iter().any(|model| model.variants.is_empty()) {
                return Err(Error::Config(
                    "realtime APIs require a voice model and a voice".into(),
                ));
            }
            super::provider::unique_ids(api.models.iter().map(|model| model.id.as_str()))?;
            for model in &api.models {
                super::provider::unique_ids(model.variants.iter().map(|voice| voice.id.as_str()))?;
            }
        }
        Ok(())
    }
}

pub(super) static OPENAI_MODELS: std::sync::LazyLock<&'static [super::provider::MediaModelPreset]> =
    std::sync::LazyLock::new(|| MANIFEST.openai.models.as_slice());
pub(super) static CODEX_MODELS: std::sync::LazyLock<&'static [super::provider::MediaModelPreset]> =
    std::sync::LazyLock::new(|| MANIFEST.codex.models.as_slice());

pub(super) static VOICES: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| MANIFEST.openai.voices());
pub(super) static CODEX_VOICES: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| MANIFEST.codex.voices());

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Frontend SDP and gateway-owned instructions for one voice call.
pub struct RealtimeVoiceRequest {
    /// The session identifier.
    pub session_id: String,
    /// A provider-advertised voice model, or `None` for its default.
    pub model: Option<String>,
    /// A provider-advertised voice, or `None` for its default.
    pub voice: Option<String>,
    /// The offer sdp.
    pub offer_sdp: String,
    /// The instructions.
    pub instructions: String,
}

/// A voice call whose provider credentials and call identity remain private.
/// Retain the whole value while using its channels; dropping it hangs up the call.
pub struct RealtimeVoiceCall {
    /// The provider-selected voice, including default selection.
    pub voice: String,
    /// The answer sdp.
    pub answer_sdp: String,
    /// The commands.
    pub commands: mpsc::Sender<RealtimeVoiceCommand>,
    /// The events.
    pub events: mpsc::Receiver<Result<RealtimeVoiceEvent>>,
    _cancel: oneshot::Sender<()>,
}

impl RealtimeVoiceCall {
    pub(super) fn cleanup_deadline(
        expires_at: std::time::SystemTime,
        io_timeout: Duration,
    ) -> std::time::SystemTime {
        // Sending close, draining final events, closing the socket and authenticated hangup
        // each need one I/O window before key expiry.
        expires_at
            .checked_sub(4 * io_timeout)
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
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or a required resource cannot be initialized.
    pub fn new(
        answer_sdp: String,
        voice: String,
        commands: mpsc::Sender<RealtimeVoiceCommand>,
        events: mpsc::Receiver<Result<RealtimeVoiceEvent>>,
        cancellation: oneshot::Sender<()>,
    ) -> Result<Self> {
        validate_sdp(&answer_sdp)?;
        validate_text(&voice, 256, "voice name")?;
        Ok(Self {
            voice,
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
    /// Selects the reply case.
    Reply {
        /// The handoff identifier.
        handoff_id: String,
        /// The text.
        text: String,
    },
    /// Background agent context or progress; it must not initiate a voice response.
    Context {
        /// The text.
        text: String,
    },
}

/// Provider-normalized handoffs and usage for one voice call.
#[derive(Debug, PartialEq, Eq)]
pub enum RealtimeVoiceEvent {
    /// Incremental speech text and complete snapshots of provider turns or caption groups.
    Transcript {
        /// The identifier.
        id: String,
        /// The role.
        role: crate::protocol::ConversationRole,
        /// The text.
        text: String,
        /// The complete.
        complete: bool,
    },
    /// Selects the handoff case.
    Handoff {
        /// The identifier.
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
    settings: super::ModelTransportSettings,
}

impl RealtimeTransport {
    pub(super) fn new_openai(
        base_url: &str,
        auth: Arc<dyn OpenAiAuthorization>,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        super::provider::validate_base_url(base_url)?;
        let endpoint = format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            MANIFEST.openai.calls_path
        );
        let sideband = MANIFEST
            .openai
            .sideband_base_url
            .as_deref()
            .unwrap_or(&endpoint);
        Self::with_endpoints(VoiceApi::OpenAi, auth, &endpoint, sideband, settings)
    }

    pub(super) fn new_codex(
        base_url: &str,
        auth: Arc<dyn OpenAiAuthorization>,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        super::provider::validate_base_url(base_url)?;
        let calls_url = format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            MANIFEST.codex.calls_path
        );
        let api_url = if super::openai_codex::provider().uses_default_endpoint(Some(base_url)) {
            std::borrow::Cow::Borrowed(
                MANIFEST
                    .codex
                    .sideband_base_url
                    .as_deref()
                    .expect("Codex native sideband endpoint is required"),
            )
        } else {
            std::borrow::Cow::Owned(format!("{}/live", base_url.trim_end_matches('/')))
        };
        Self::with_endpoints(VoiceApi::Codex, auth, &calls_url, &api_url, settings)
    }

    fn with_endpoints(
        api: VoiceApi,
        auth: Arc<dyn OpenAiAuthorization>,
        calls_url: &str,
        api_url: &str,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        Ok(Self {
            api,
            // Voice credentials must never follow a provider redirect.
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_millis(settings.voice_start_timeout_ms))
                .build()?,
            auth,
            calls_url: Url::parse(calls_url)
                .map_err(|_| invalid("invalid voice calls endpoint"))?,
            api_url: Url::parse(api_url).map_err(|_| invalid("invalid voice sideband endpoint"))?,
            settings,
        })
    }

    pub(super) fn with_settings(mut self, settings: super::ModelTransportSettings) -> Result<Self> {
        if settings.voice_start_timeout_ms != self.settings.voice_start_timeout_ms {
            self.client = Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_millis(settings.voice_start_timeout_ms))
                .build()?;
        }
        self.settings = settings;
        Ok(self)
    }

    pub(super) async fn start(&self, request: RealtimeVoiceRequest) -> Result<RealtimeVoiceCall> {
        timeout(
            Duration::from_millis(self.settings.voice_start_timeout_ms),
            self.start_inner(request),
        )
        .await
        .map_err(|_| invalid("voice negotiation timed out"))?
    }

    async fn start_inner(&self, request: RealtimeVoiceRequest) -> Result<RealtimeVoiceCall> {
        validate_text(&request.session_id, 256, "session identity")?;
        validate_sdp(&request.offer_sdp)?;
        validate_text(&request.instructions, MAX_TEXT_BYTES, "voice instructions")?;
        let session = self.session(&request)?;
        let voice = field(&session["audio"]["output"], "voice")?.to_owned();
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
        let (cleanup, answer_sdp) = self.negotiate(response, request.session_id).await?;
        validate_sdp(&answer_sdp)?;
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, events) = mpsc::channel(16);
        let (cancel, cancelled) = oneshot::channel();
        let api = self.api;
        tokio::spawn(async move {
            let transport = &cleanup.transport;
            let mut cancelled = cancelled;
            let mut pending = VecDeque::new();
            let io_timeout = Duration::from_millis(transport.settings.voice_io_timeout_ms);
            let mut command_rx = command_rx;
            let mut socket = {
                let connect = timeout(
                    Duration::from_millis(transport.settings.voice_start_timeout_ms),
                    transport.connect(&cleanup.call_id, &cleanup.session_id),
                );
                tokio::pin!(connect);
                loop {
                    tokio::select! {
                        _ = &mut cancelled => return,
                        result = &mut connect => break match result {
                            Ok(Ok(socket)) => socket,
                            Ok(Err(error)) => {
                                let _ = event_tx.send(Err(error)).await;
                                return;
                            }
                            Err(_) => {
                                let _ = event_tx
                                    .send(Err(invalid("voice sideband negotiation timed out")))
                                    .await;
                                return;
                            }
                        },
                        command = command_rx.recv(), if pending.len() < COMMAND_CAPACITY => match command {
                            Some(RealtimeVoiceCommand::Close) | None => return,
                            Some(command) => pending.push_back(command),
                        },
                    }
                }
            };
            let result = tokio::select! {
                biased;
                _ = &mut cancelled => {
                    close_session(&mut socket, api, &mut VoiceTurns::default(), None, io_timeout).await
                },
                result = timeout(Duration::from_millis(transport.settings.voice_call_timeout_ms), drive(&mut socket, api, command_rx, pending, &event_tx, io_timeout)) => {
                    result.unwrap_or_else(|_| Err(invalid("voice call reached its time limit")))
                }
            };
            let _ = timeout(io_timeout, socket.close(None)).await;
            drop(cleanup);
            if let Err(error) = result {
                // A retained full event queue must never delay hangup or task cleanup.
                let _ = timeout(io_timeout, event_tx.send(Err(error))).await;
            }
        });
        RealtimeVoiceCall::new(answer_sdp, voice, commands, events, cancel)
    }

    fn models(&self) -> &'static [super::provider::MediaModelPreset] {
        match self.api {
            VoiceApi::OpenAi => &OPENAI_MODELS,
            VoiceApi::Codex => &CODEX_MODELS,
        }
    }

    fn session(&self, request: &RealtimeVoiceRequest) -> Result<Value> {
        let preset = match request.model.as_deref() {
            Some(id) => self.models().iter().find(|model| model.id == id),
            None => self.models().first(),
        };
        let model = request
            .model
            .as_deref()
            .or_else(|| preset.map(|model| model.id.as_str()))
            .ok_or_else(|| invalid("select a voice model"))?;
        let voice = request
            .voice
            .as_deref()
            .or_else(|| {
                preset
                    .and_then(|model| model.variants.first())
                    .map(|variant| variant.id.as_str())
            })
            .ok_or_else(|| invalid("select a voice"))?;
        Ok(json!({"model":model,"instructions":request.instructions,
            "audio":{"output":{"voice":voice}},"delegation":{"type":"client"}}))
    }

    async fn negotiate(
        &self,
        response: reqwest::Response,
        session_id: String,
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
                    session_id,
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
            session_id,
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
        let request = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body)
            .timeout(Duration::from_millis(self.settings.voice_start_timeout_ms));
        for attempt in 0..2 {
            let auth = self.auth.authorize_http(false, Some(session_id)).await?;
            // Reqwest shares the owned byte body while each authorization attempt gets its own headers.
            let mut request = request
                .try_clone()
                .ok_or_else(|| invalid("voice request could not be replayed for authorization"))?
                .bearer_auth(&auth.token);
            for (name, value) in auth.headers {
                request = request.header(name, value.as_ref());
            }
            if self.api == VoiceApi::Codex {
                for (name, value) in &MANIFEST.codex.headers {
                    request = request.header(name, value);
                }
                request = request.header("x-session-id", session_id);
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
        if !crate::identifier::valid_ascii_identifier(
            id,
            128,
            crate::identifier::AsciiCase::Any,
            b"_-",
        ) || !(id.starts_with("rtc_") && id.len() > 4 || uuid::Uuid::parse_str(id).is_ok())
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
                std::iter::once(("authorization", format!("Bearer {}", auth.token).into()))
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
                for (name, value) in &MANIFEST.codex.headers {
                    request.headers_mut().insert(
                        tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(
                            name.as_bytes(),
                        )
                        .map_err(|_| invalid("invalid realtime manifest header"))?,
                        value
                            .parse()
                            .map_err(|_| invalid("invalid realtime manifest header value"))?,
                    );
                }
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
                    return Err(Error::Provider(super::transport::http_provider_error(
                        "Realtime sideband",
                        response.status(),
                        body,
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
        let session_id = std::mem::take(&mut self.session_id);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = timeout(
                    Duration::from_millis(transport.settings.voice_io_timeout_ms),
                    transport.post(url, Vec::new(), "application/json", &session_id),
                )
                .await;
            });
        }
    }
}

fn validate_call_id(id: &str) -> Result<()> {
    validate_text(id, 256, "voice call identity")?;
    if !crate::identifier::valid_ascii_identifier(id, 256, crate::identifier::AsciiCase::Any, b"_-")
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

async fn send(socket: &mut Socket, value: Value, io_timeout: Duration) -> Result<()> {
    let text = serde_json::to_string(&value)?;
    if text.len() > MAX_EVENT_BYTES {
        return Err(invalid("voice command exceeded size limit"));
    }
    timeout(io_timeout, socket.send(Message::text(text)))
        .await
        .map_err(|_| invalid("voice send timed out"))?
        .map_err(socket_error)
}

async fn drive(
    socket: &mut Socket,
    api: VoiceApi,
    mut commands: mpsc::Receiver<RealtimeVoiceCommand>,
    mut pending: VecDeque<RealtimeVoiceCommand>,
    events: &mpsc::Sender<Result<RealtimeVoiceEvent>>,
    io_timeout: Duration,
) -> Result<()> {
    let mut turns = VoiceTurns::default();
    loop {
        if let Some(command) = pending.pop_front() {
            if apply_command(socket, api, &mut turns, events, command, io_timeout).await? {
                return Ok(());
            }
            continue;
        }
        tokio::select! {
            _ = events.closed() => return close_session(socket, api, &mut turns, None, io_timeout).await,
            command = commands.recv() => {
                let Some(command) = command else {
                    return close_session(socket, api, &mut turns, Some(events), io_timeout).await;
                };
                if apply_command(socket, api, &mut turns, events, command, io_timeout).await? {
                    return Ok(());
                }
            }

            message = socket.next() => {
                let Some(message) = message else { return Err(invalid("voice sideband closed")); };
                let value = match message.map_err(socket_error)? {
                    Message::Text(text) => serde_json::from_str(&text)?,
                    Message::Binary(bytes) => serde_json::from_slice(&bytes)?,
                    Message::Close(_) => return Ok(()),
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => { timeout(io_timeout, socket.flush()).await.map_err(|_| invalid("voice flush timed out"))?.map_err(socket_error)?; continue; }
                };
                for event in turns.observe(api, &value)? {
                    if events.send(Ok(event)).await.is_err() {
                        return close_session(socket, api, &mut turns, None, io_timeout).await;
                    }
                }
                if value["type"] == "session.closed" { return Ok(()); }
            }
        }
    }
}

async fn apply_command(
    socket: &mut Socket,
    api: VoiceApi,
    turns: &mut VoiceTurns,
    events: &mpsc::Sender<Result<RealtimeVoiceEvent>>,
    command: RealtimeVoiceCommand,
    io_timeout: Duration,
) -> Result<bool> {
    if matches!(command, RealtimeVoiceCommand::Close) {
        close_session(socket, api, turns, Some(events), io_timeout).await?;
        return Ok(true);
    }
    let (handoff_id, text) = match command {
        RealtimeVoiceCommand::Reply { handoff_id, text } => {
            if !turns.reply_pending.remove(&handoff_id) {
                return Err(invalid("voice reply has no pending handoff"));
            }
            (Some(handoff_id), text)
        }
        RealtimeVoiceCommand::Context { text } => (None, text),
        RealtimeVoiceCommand::Close => unreachable!("handled above"),
    };
    validate_text(&text, MAX_TEXT_BYTES, "voice context")?;
    for chunk in context_chunks(&text) {
        let value = match (api, handoff_id.as_deref()) {
            (VoiceApi::Codex, Some(id)) => {
                json!({"type":"delegation.context.append","delegation_item_id":id,"channel":"speakable","content":[{"type":"input_text","text":chunk}]})
            }
            (VoiceApi::Codex, None) => {
                json!({"type":"session.context.append","channel":"commentary","content":[{"type":"input_text","text":chunk}]})
            }
            (VoiceApi::OpenAi, id) => {
                json!({"type":if id.is_some() {"session.commentary.append"} else {"session.thinking.append"},"delegation_id":id,"content":chunk})
            }
        };
        send(socket, value, io_timeout).await?;
    }
    Ok(false)
}

async fn close_session(
    socket: &mut Socket,
    api: VoiceApi,
    turns: &mut VoiceTurns,
    events: Option<&mpsc::Sender<Result<RealtimeVoiceEvent>>>,
    io_timeout: Duration,
) -> Result<()> {
    send(socket, json!({"type":"session.close"}), io_timeout).await?;
    if api == VoiceApi::OpenAi {
        timeout(io_timeout, drain_closed(socket, api, turns, events))
            .await
            .map_err(|_| invalid("voice session finalization timed out"))??;
    }
    Ok(())
}

async fn drain_closed(
    socket: &mut Socket,
    api: VoiceApi,
    turns: &mut VoiceTurns,
    events: Option<&mpsc::Sender<Result<RealtimeVoiceEvent>>>,
) -> Result<()> {
    while let Some(message) = socket.next().await {
        let value: Value = match message.map_err(socket_error)? {
            Message::Text(text) => serde_json::from_str(&text)?,
            Message::Binary(bytes) => serde_json::from_slice(&bytes)?,
            Message::Close(_) => break,
            _ => continue,
        };
        if let Some(events) = events.filter(|events| !events.is_closed()) {
            for event in turns.observe(api, &value)? {
                // The sideband must receive final usage even if the UI has stopped listening.
                let _ = events.send(Ok(event)).await;
            }
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
            return Err(Error::Provider(super::openai::response_provider_error(
                event, None,
            )));
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
        self.transcript(&id, live_role(speaker), text.into(), false, events)
    }

    fn finish_caption(
        &mut self,
        speaker: usize,
        events: &mut Vec<RealtimeVoiceEvent>,
    ) -> Result<()> {
        if let Some(caption) = self.live_captions[speaker].take() {
            let text = self
                .streams
                .get_mut(&caption.id)
                .map(|stream| std::mem::take(&mut stream.text))
                .unwrap_or_default();
            self.transcript(&caption.id, live_role(speaker), text.into(), true, events)?;
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
            Some(kind @ ("input_transcript.added" | "output_transcript.added")) => {
                let user = kind == "input_transcript.added";
                let (id, role) = if user {
                    (self.codex_input.take(), ConversationRole::User)
                } else {
                    (self.codex_output.take(), ConversationRole::Assistant)
                };
                let id = id.unwrap_or_else(|| self.codex_id(user));
                let result = transcript_field(&event["item"], "text")
                    .and_then(|text| self.transcript(&id, role, text.into(), false, events));
                if user {
                    self.codex_input = Some(id);
                } else {
                    self.codex_output = Some(id);
                }
                result?;
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
        self.transcript(&id, role, text.into(), true, events)
    }

    fn transcript(
        &mut self,
        id: &str,
        role: crate::protocol::ConversationRole,
        text: Cow<'_, str>,
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
            stream.text = String::new();
            stream.complete = true;
        } else {
            if stream.text.len() + text.len() > MAX_TEXT_BYTES {
                return Err(invalid("voice transcript exceeded its size limit"));
            }
            stream.text.push_str(&text);
        }
        let text = text.into_owned();
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
