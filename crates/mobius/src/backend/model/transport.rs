use reqwest::RequestBuilder;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::error::Error as _;
use std::time::Duration;

use reqwest::Client;
use reqwest::Response;
use std::sync::Arc;

use super::provider::BaseUrl;
use crate::Error;
use crate::ProviderError;
use crate::Result;

/// Applies the provider headers and scoped bearer credential to one outbound HTTP request.
pub(super) fn provider_request(
    mut request: RequestBuilder,
    headers: Option<&BTreeMap<String, String>>,
    token: Option<&str>,
) -> RequestBuilder {
    for (name, value) in headers.into_iter().flatten() {
        request = request.header(name, value);
    }
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request
}

/// Sends an HTTP request whose adapter expects a successful status.
pub(super) async fn send_checked(request: RequestBuilder, provider: &str) -> Result<Response> {
    let response = request
        .send()
        .await
        .map_err(|error| request_error(error, provider))?;
    if !response.status().is_success() {
        return Err(status_error(response, provider).await);
    }
    Ok(response)
}

pub(crate) fn retry_delay(
    error: &crate::ProviderError,
    retry: usize,
    request_id: &str,
    transport: &crate::backend::model::ModelTransportSettings,
) -> Duration {
    let multiplier = 1_u64
        .checked_shl(u32::try_from(retry).unwrap_or(u32::MAX))
        .unwrap_or(u64::MAX);
    let exponential_ms = transport
        .stream_retry_backoff_ms
        .saturating_mul(multiplier)
        .min(transport.stream_retry_max_backoff_ms);
    let jitter = request_id
        .bytes()
        .fold(u64::try_from(retry).unwrap_or(u64::MAX), |value, byte| {
            value.wrapping_mul(16_777_619).wrapping_add(u64::from(byte))
        });
    let jitter_percent = 80 + jitter % 41;
    let backoff = Duration::from_millis(exponential_ms.saturating_mul(jitter_percent) / 100);
    bounded_retry_delay(
        error,
        backoff,
        Duration::from_millis(transport.stream_retry_max_backoff_ms),
    )
}

pub(super) fn bounded_retry_delay(
    error: &ProviderError,
    backoff: Duration,
    ceiling: Duration,
) -> Duration {
    let delay = server_retry_delay(error).unwrap_or_default();
    delay.max(backoff).min(ceiling)
}

fn server_retry_delay(error: &ProviderError) -> Option<Duration> {
    let value = error.retry_after()?.trim();
    let delay = value
        .parse::<u64>()
        .map(Duration::from_secs)
        .ok()
        .or_else(|| {
            let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
            Some(
                (date.with_timezone(&chrono::Utc) - chrono::Utc::now())
                    .to_std()
                    .unwrap_or_default(),
            )
        })?;
    std::time::Instant::now().checked_add(delay).map(|_| delay)
}

pub(super) const MAX_ERROR_BYTES: usize = 64 * 1024;
pub(super) const MAX_SSE_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;
crate::embedded_config! {
    copy;
    /// Unvalidated wire shape of [`ModelTransportSettings`].
    /// Millisecond units allow short local test deadlines without changing wire protocols.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RawModelTransportSettings {
        /// Complete serialized outgoing request limit, including text and tool schemas.
        pub max_request_bytes: usize,
        /// HTTP connection deadline.
        pub http_connect_timeout_ms: u64,
        /// Maximum gap between HTTP response chunks.
        pub http_idle_timeout_ms: u64,
        /// WebSocket connection deadline.
        pub socket_connect_timeout_ms: u64,
        /// WebSocket write and close deadline.
        pub socket_io_timeout_ms: u64,
        /// Maximum gap between WebSocket stream events.
        pub socket_idle_timeout_ms: u64,
        /// Retries before a model request fails.
        pub stream_retry_limit: u32,
        /// Initial model retry backoff.
        pub stream_retry_backoff_ms: u64,
        /// Ceiling for retry waits, including jitter and server hints.
        pub stream_retry_max_backoff_ms: u64,
        /// Voice negotiation deadline.
        pub voice_start_timeout_ms: u64,
        /// Voice socket write and cleanup deadline.
        pub voice_io_timeout_ms: u64,
        /// Maximum duration of one voice call.
        pub voice_call_timeout_ms: u64,
        /// OAuth HTTP request deadline.
        pub oauth_request_timeout_ms: u64,
        /// Device-code authentication completion deadline.
        pub device_code_timeout_ms: u64,
    }
    defaults = include_str!("transport.toml");
}

/// Runtime policy for model HTTP, sockets, voice and authentication, validated once
/// when constructed or deserialized; fields are read through `Deref`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    try_from = "RawModelTransportSettings",
    into = "RawModelTransportSettings"
)]
pub struct ModelTransportSettings(RawModelTransportSettings);

impl Default for ModelTransportSettings {
    fn default() -> Self {
        crate::embedded_config! {
            static DEFAULTS: ModelTransportSettings = include_str!("transport.toml");
        }
        *DEFAULTS
    }
}

impl std::ops::Deref for ModelTransportSettings {
    type Target = RawModelTransportSettings;

    fn deref(&self) -> &RawModelTransportSettings {
        &self.0
    }
}

impl From<ModelTransportSettings> for RawModelTransportSettings {
    fn from(settings: ModelTransportSettings) -> Self {
        settings.0
    }
}

impl TryFrom<RawModelTransportSettings> for ModelTransportSettings {
    type Error = Error;

    /// Rejects zero deadlines, excessive retry budgets and overflowing backoff policy.
    fn try_from(raw: RawModelTransportSettings) -> Result<Self> {
        if raw.max_request_bytes == 0 {
            return Err(Error::Invalid(
                "model request limit must be positive".into(),
            ));
        }
        let deadlines = [
            raw.http_connect_timeout_ms,
            raw.http_idle_timeout_ms,
            raw.socket_connect_timeout_ms,
            raw.socket_io_timeout_ms,
            raw.socket_idle_timeout_ms,
            raw.stream_retry_backoff_ms,
            raw.stream_retry_max_backoff_ms,
            raw.voice_start_timeout_ms,
            raw.voice_io_timeout_ms,
            raw.voice_call_timeout_ms,
            raw.oauth_request_timeout_ms,
            raw.device_code_timeout_ms,
        ];
        if deadlines
            .into_iter()
            .any(|value| value == 0 || value > 365 * 24 * 60 * 60 * 1_000)
        {
            return Err(Error::Invalid(
                "model transport deadlines must be positive and at most one year".into(),
            ));
        }
        if raw.stream_retry_limit > 100 {
            return Err(Error::Invalid(
                "model transport retry limits must be at most 100".into(),
            ));
        }
        if raw.stream_retry_backoff_ms > raw.stream_retry_max_backoff_ms {
            return Err(Error::Invalid(
                "initial model retry backoff exceeds its ceiling".into(),
            ));
        }
        Ok(Self(raw))
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct HttpClientKey {
    connect_timeout_ms: Option<u64>,
    read_timeout_ms: Option<u64>,
    request_timeout_ms: Option<u64>,
}

const MAX_CACHED_HTTP_CLIENTS: usize = 16;

pub(super) fn cached_no_redirect_client(
    connect_timeout_ms: Option<u64>,
    read_timeout_ms: Option<u64>,
    request_timeout_ms: Option<u64>,
) -> Result<Client> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<HttpClientKey, Client>>> = OnceLock::new();
    let key = HttpClientKey {
        connect_timeout_ms,
        read_timeout_ms,
        request_timeout_ms,
    };
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(client) = cache
        .lock()
        .map_err(|_poisoned| Error::Poisoned("model HTTP client cache"))?
        .get(&key)
    {
        return Ok(Client::clone(client)); // rules-allow: reqwest::Client shares its pooled transport via Arc
    }
    // Provider credentials and account headers must never follow redirects.
    let mut builder = crate::backend::model::provider::http_client_builder()
        .redirect(reqwest::redirect::Policy::none());
    if let Some(ms) = connect_timeout_ms {
        builder = builder.connect_timeout(Duration::from_millis(ms));
    }
    if let Some(ms) = read_timeout_ms {
        builder = builder.read_timeout(Duration::from_millis(ms));
    }
    if let Some(ms) = request_timeout_ms {
        builder = builder.timeout(Duration::from_millis(ms));
    }
    let client = builder.build()?;
    let mut guard = cache
        .lock()
        .map_err(|_poisoned| Error::Poisoned("model HTTP client cache"))?;
    if guard.len() >= MAX_CACHED_HTTP_CLIENTS {
        guard.clear();
    }
    guard.insert(key, Client::clone(&client)); // rules-allow: reqwest::Client shares its pooled transport via Arc
    drop(guard);
    Ok(client)
}

impl ModelTransportSettings {
    /// Builds or reuses a cached HTTP streaming pool for this validated policy.
    /// # Errors
    /// Returns HTTP client construction errors.
    pub fn streaming_client(&self) -> Result<Client> {
        cached_no_redirect_client(
            Some(self.http_connect_timeout_ms),
            Some(self.http_idle_timeout_ms),
            None,
        )
    }
}

pub(super) fn account_stream_bytes(total: &mut usize, added: usize, provider: &str) -> Result<()> {
    *total = total
        .checked_add(added)
        .filter(|total| *total <= MAX_STREAM_BYTES)
        .ok_or_else(|| Error::Provider(format!("{provider} stream exceeded size limit").into()))?;
    Ok(())
}

#[derive(Default)]
pub(super) struct SseDecoder {
    bytes: Vec<u8>,
    frame_start: usize,
    scan: usize,
    total: usize,
}

impl SseDecoder {
    pub(super) fn push(&mut self, chunk: &[u8], provider: &str) -> Result<()> {
        account_stream_bytes(&mut self.total, chunk.len(), provider)?;
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    pub(super) fn next_frame(&mut self) -> Result<Option<&str>> {
        self.compact();
        let start = self.frame_start;
        let mut index = self.scan.max(start);
        while index < self.bytes.len() {
            let rest = self.bytes.get(index..).unwrap_or_default();
            let width = if rest.starts_with(b"\r\n\r\n") {
                4
            } else if rest.starts_with(b"\n\n") {
                2
            } else {
                index += 1;
                continue;
            };
            if index - start > MAX_SSE_FRAME_BYTES {
                return Err(Error::Provider("SSE frame exceeded size limit".into()));
            }
            self.frame_start = index + width;
            self.scan = self.frame_start;
            return std::str::from_utf8(self.bytes.get(start..index).unwrap_or_default())
                .map(Some)
                .map_err(|error| Error::Provider(format!("invalid SSE UTF-8: {error}").into()));
        }
        self.scan = self.bytes.len().saturating_sub(3).max(start);
        let unfinished = self.bytes.get(start..).unwrap_or_default();
        // An incomplete delimiter is framing, rather than part of the payload budget.
        let delimiter_prefix = if unfinished.ends_with(b"\r\n\r") {
            3
        } else if unfinished.ends_with(b"\r\n") {
            2
        } else {
            usize::from(unfinished.ends_with(b"\r") || unfinished.ends_with(b"\n"))
        };
        if unfinished.len() - delimiter_prefix > MAX_SSE_FRAME_BYTES {
            return Err(Error::Provider("SSE frame exceeded size limit".into()));
        }
        Ok(None)
    }

    fn compact(&mut self) {
        let remaining = self.bytes.len() - self.frame_start;
        if self.frame_start == 0 || self.frame_start < remaining {
            return;
        }
        self.bytes.drain(..self.frame_start);
        self.scan -= self.frame_start;
        self.frame_start = 0;
    }
}

pub(super) fn frame_data(frame: &str) -> Option<Cow<'_, str>> {
    // SSE streams may open with a UTF-8 BOM; stripping it per frame also covers the first one.
    let frame = frame.trim_prefix('\u{feff}');
    let mut data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start));
    let mut payload = Cow::Borrowed(data.next()?);
    for line in data {
        let payload = payload.to_mut();
        payload.push('\n');
        payload.push_str(line);
    }
    Some(payload)
}

pub(super) async fn read_limited(
    response: Response,
    limit: usize,
    provider: &str,
) -> Result<Vec<u8>> {
    read_bounded(response, limit, provider, || {
        Error::Provider(format!("{provider} response body exceeded size limit").into())
    })
    .await
}

pub(super) async fn read_bounded(
    mut response: Response,
    limit: usize,
    provider: &str,
    too_large: impl FnOnce() -> Error,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| request_error(error, provider))?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(super) fn request_error(error: reqwest::Error, provider: &str) -> Error {
    if error.is_timeout() {
        return ProviderError::retryable(format!("{provider} request timed out")).into();
    }
    let mut source = error.source();
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::NetworkDown
                    | std::io::ErrorKind::NetworkUnreachable
                    | std::io::ErrorKind::HostUnreachable
            )
        {
            return ProviderError::retryable(format!(
                "{provider} request connection failed: {}",
                io.kind()
            ))
            .into();
        }
        source = cause.source();
    }
    // ponytail: disconnects without an I/O cause stay terminal; add typed protocol checks if needed.
    Error::Http(error.without_url())
}

pub(super) async fn status_error(mut response: Response, provider: &str) -> Error {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut bytes = Vec::new();
    while bytes.len() < MAX_ERROR_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = MAX_ERROR_BYTES - bytes.len();
                bytes.extend_from_slice(chunk.get(..remaining).unwrap_or(&chunk));
            }
            Ok(None) => break,
            Err(error) => {
                return Error::Provider(ProviderError::http(
                    format!("{provider} HTTP {status}: {error}"),
                    status.as_u16(),
                    retry_after,
                ));
            }
        }
    }
    Error::Provider(http_provider_error(provider, status, &bytes, retry_after))
}

pub(super) fn http_provider_error(
    provider: &str,
    status: reqwest::StatusCode,
    body: &[u8],
    retry_after: Option<String>,
) -> ProviderError {
    let event = serde_json::from_slice::<serde_json::Value>(body).ok();
    let message = http_error_message_from_json(provider, status, event.as_ref(), body);
    ProviderError::http(message, status.as_u16(), retry_after)
        .with_code(event.as_ref().and_then(response_error_code))
}

/// Maps a rejected WebSocket handshake to the provider's HTTP error.
pub(super) type ModelWebSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(super) enum WebSocketConnectError {
    TimedOut,
    Unauthorized(Box<tokio_tungstenite::tungstenite::http::Response<Option<Vec<u8>>>>),
    Http(Box<tokio_tungstenite::tungstenite::http::Response<Option<Vec<u8>>>>),
    Transport(tokio_tungstenite::tungstenite::Error),
    Request(tokio_tungstenite::tungstenite::Error),
    InvalidHeader(String),
    Other(Error),
}

pub(super) async fn connect_authorized_websocket<'a, F, Fut>(
    auth: Option<&'a dyn super::authorization::OpenAiAuthorization>,
    url: &str,
    max_message_bytes: usize,
    connect_timeout: Option<Duration>,
    mut authorize: F,
    extra_headers: &[(&'a str, &'a str)],
) -> std::result::Result<ModelWebSocket, WebSocketConnectError>
where
    F: FnMut(&'a dyn super::authorization::OpenAiAuthorization) -> Fut,
    Fut: std::future::Future<Output = Result<super::authorization::ResolvedAuthorization<'a>>>,
{
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, HeaderName, HeaderValue};
    use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

    let mut retry = super::authorization::UnauthorizedRetry::default();
    loop {
        let authorization = match auth {
            Some(auth) => Some(
                authorize(auth)
                    .await
                    .map_err(WebSocketConnectError::Other)?,
            ),
            None => None,
        };
        let mut request = url
            .into_client_request()
            .map_err(WebSocketConnectError::Request)?;
        if let Some(authorization) = &authorization {
            let bearer = HeaderValue::from_str(&format!("Bearer {}", authorization.token))
                .map_err(|error| {
                    WebSocketConnectError::InvalidHeader(format!(
                        "access token is not a valid header value: {error}"
                    ))
                })?;
            request.headers_mut().insert(AUTHORIZATION, bearer);
        }
        for (name, value) in authorization
            .iter()
            .flat_map(|a| &a.headers)
            .map(|(k, v)| (*k, v.as_ref()))
            .chain(extra_headers.iter().copied())
        {
            let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                WebSocketConnectError::InvalidHeader(format!(
                    "{name} is not a valid header name: {error}"
                ))
            })?;
            let header_value = HeaderValue::from_str(value).map_err(|error| {
                WebSocketConnectError::InvalidHeader(format!(
                    "authorization header value is invalid: {error}"
                ))
            })?;
            request.headers_mut().insert(header_name, header_value);
        }
        let config = WebSocketConfig::default()
            .max_message_size(Some(max_message_bytes))
            .max_frame_size(Some(max_message_bytes));
        let attempt = tokio_tungstenite::connect_async_with_config(request, Some(config), false);
        let result = if let Some(limit) = connect_timeout {
            let Ok(result) = tokio::time::timeout(limit, attempt).await else {
                return Err(WebSocketConnectError::TimedOut);
            };
            result
        } else {
            attempt.await
        };
        match result {
            Ok((socket, _)) => return Ok(socket),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                if response.status().as_u16() == 401 {
                    if let (Some(auth), Some(authorization)) = (auth, &authorization)
                        && retry
                            .retry_unauthorized(auth, &authorization.token)
                            .await
                            .map_err(WebSocketConnectError::Other)?
                    {
                        continue;
                    }
                    return Err(WebSocketConnectError::Unauthorized(response));
                }
                return Err(WebSocketConnectError::Http(response));
            }
            Err(error) => return Err(WebSocketConnectError::Transport(error)),
        }
    }
}

pub(super) fn handshake_error(
    provider: &str,
    response: &tokio_tungstenite::tungstenite::http::Response<Option<Vec<u8>>>,
) -> ProviderError {
    let retry_after = response
        .headers()
        .get(tokio_tungstenite::tungstenite::http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.body().as_deref().unwrap_or_default();
    let body = body.get(..MAX_ERROR_BYTES).unwrap_or(body);
    http_provider_error(provider, response.status(), body, retry_after)
}

/// Derives a provider WebSocket URL from an HTTP API URL, keeping TLS.
pub(super) fn socket_url(mut url: reqwest::Url) -> Result<reqwest::Url> {
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|()| Error::Invalid("invalid provider socket scheme".into()))?;
    Ok(url)
}

pub(super) fn response_error_code(event: &serde_json::Value) -> Option<&str> {
    let mut codes = [
        "/error/code",
        "/response/error/code",
        "/error/type",
        "/response/error/type",
    ]
    .into_iter()
    .filter_map(|pointer| event.pointer(pointer).and_then(serde_json::Value::as_str))
    .filter(|code| !code.is_empty());
    let first = codes.next()?;
    Some(
        codes
            .find(|code| ProviderError::is_quota_code(code))
            .unwrap_or(first),
    )
}

/// `{prefix} HTTP {status}`, followed by the human part of the body when it has one, so
/// frontends show the provider's message rather than its raw JSON.
fn http_error_message_from_json(
    prefix: &str,
    status: impl std::fmt::Display,
    json: Option<&serde_json::Value>,
    body: &[u8],
) -> String {
    let detail = error_detail(json, body);
    if detail.is_empty() {
        format!("{prefix} HTTP {status}")
    } else {
        format!("{prefix} HTTP {status}: {detail}")
    }
}

/// A JSON error's `message` (nested under `error` or top-level, or `error` itself when it is
/// a string), else the trimmed text.
fn error_detail(json: Option<&serde_json::Value>, body: &[u8]) -> String {
    let message = json.and_then(|value| {
        let error = value.get("error").unwrap_or(value);
        error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .or_else(|| error.as_str())
    });
    match message {
        Some(message) => message.trim().to_owned(),
        None => String::from_utf8_lossy(body).trim().to_owned(),
    }
}

pub(super) struct ApiKeyModel {
    pub(super) client: Client,
    pub(super) transport: ModelTransportSettings,
    pub(super) api_key: Option<String>,
    pub(super) base_url: BaseUrl,
    pub(super) model: Arc<str>,
    pub(super) reasoning_effort: Option<Arc<str>>,
}

impl std::fmt::Debug for ApiKeyModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyModel")
            .field("model", &self.model)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish_non_exhaustive()
    }
}

pub(super) fn parse_reasoning_effort(effort: impl Into<String>) -> Result<Arc<str>> {
    let effort = effort.into();
    if effort.trim().is_empty() {
        return Err(Error::Invalid("reasoning effort cannot be empty".into()));
    }
    Ok(effort.into())
}

impl ApiKeyModel {
    /// Wraps settings that provider assembly has already resolved and validated.
    pub(super) fn new(
        api_key: Option<String>,
        base_url: BaseUrl,
        model: String,
        client: Client,
        transport: ModelTransportSettings,
    ) -> Result<Self> {
        super::authorization::validate_api_key(api_key.as_deref())?;
        Ok(Self {
            client,
            api_key,
            base_url,
            model: model.into(),
            reasoning_effort: None,
            transport,
        })
    }

    /// Checks a library caller's model and builds its own streaming client.
    pub(super) fn from_settings(
        api_key: String,
        base_url: BaseUrl,
        model: String,
        transport: ModelTransportSettings,
    ) -> Result<Self> {
        if model.trim().is_empty() {
            return Err(Error::Invalid("model is empty".into()));
        }
        Self::new(
            Some(api_key),
            base_url,
            model,
            transport.streaming_client()?,
            transport,
        )
    }

    pub(super) fn set_reasoning(&mut self, effort: impl Into<String>) -> Result<()> {
        self.reasoning_effort = Some(parse_reasoning_effort(effort)?);
        Ok(())
    }

    pub(super) fn info(&self) -> crate::protocol::ModelInfo {
        crate::protocol::ModelInfo {
            model: Arc::clone(&self.model),
            reasoning_effort: self.reasoning_effort.as_ref().map(Arc::clone),
        }
    }
}

/// Consumes the data payloads of one server-sent event stream.
pub(super) trait SseHandler: Send {
    /// Applies one event's data.
    fn apply_data(
        &mut self,
        data: &str,
        events: &super::ModelEventSink,
    ) -> impl Future<Output = Result<()>> + Send;
}

pub(super) async fn read_sse(
    mut response: Response,
    provider: &str,
    events: &super::ModelEventSink,
    handler: &mut impl SseHandler,
) -> Result<()> {
    let mut decoder = SseDecoder::default();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| request_error(error, provider))?
    {
        decoder.push(&chunk, provider)?;
        while let Some(event) = decoder.next_frame()? {
            if let Some(data) = frame_data(event) {
                handler.apply_data(&data, events).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
