//! First-party OpenAI Responses WebSocket transport.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::io;
use std::io::Write;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::Ordering;
#[cfg(test)]
use std::time::Duration;

use futures_util::future::join_all;
use serde_json::Value;
use tokio::sync::Mutex;
#[cfg(test)]
use tokio::sync::mpsc;
use tokio::time::Instant;
#[cfg(test)]
use tokio::time::timeout;
#[cfg(test)]
use tokio_tungstenite::tungstenite::Message;

use self::connection::Exchange;
use self::connection::OpenAiWsConnection;
#[cfg(test)]
use self::connection::SocketEvent;
use self::connection::connect;
use self::connection::exchange;
#[cfg(test)]
use self::connection::failed_exchange;
#[cfg(test)]
use self::connection::read_exchange;
#[cfg(test)]
use self::connection::websocket_error_cause;
use super::GeneratedImage;
use super::ImageGenerationRequest;
use super::Model;
use super::ModelEventSink;
use super::ModelOutput;
use super::ModelRequest;
use super::PromptCacheMode;
use super::openai::decode_response;
use super::openai::wire_tools;
use super::openai::{CATALOG, OpenAi};
use super::openai_auth::ApiKeyAuthorization;
use super::openai_auth::OpenAiAuthorization;
#[cfg(test)]
use super::openai_auth::ResolvedAuthorization;
use super::provider::HostedWebSearch;
use super::provider::ProviderBuildConfig;
use super::provider::ProviderDefinition;
use super::responses_wire::{ResponsesBody, WireInput};
use super::{RealtimeVoiceCall, RealtimeVoiceRequest};
use crate::BoxFuture;
use crate::Error;
use crate::ProviderError;
use crate::Result;
use crate::protocol::ModelInfo;
use crate::protocol::ToolDiscoveryMode;

mod connection;

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| {
        crate::config::embedded(include_str!("openai_socket_provider.toml"))
    });

const MAX_SESSION_ENTRIES: usize = 128;

/// OpenAI's persistent Responses WebSocket transport.
pub struct OpenAiSocket {
    auth: Arc<dyn OpenAiAuthorization>,
    transport: super::ModelTransportSettings,
    socket_url: String,
    model: String,
    reasoning_effort: Option<String>,
    hosted_tools: Vec<Value>,
    explicit_prompt_cache: bool,
    sessions: Mutex<BTreeMap<String, Arc<Mutex<SocketState>>>>,
    http: OpenAi,
}

struct SocketState {
    connection: Option<OpenAiWsConnection>,
    continuation: Option<Continuation>,
    use_http: bool,
    last_used_at: Instant,
}

struct Continuation {
    response_id: String,
    known_items: usize,
    fingerprint: u64,
    envelope_fingerprint: u64,
}

impl OpenAiSocket {
    /// Creates the first-party Responses transport with HTTP fallback.
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or a required resource cannot be initialized.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        Self::with_client(
            api_key,
            MANIFEST.base_url.as_str(),
            model,
            super::transport::streaming_client()?,
            super::ModelTransportSettings::default(),
        )
    }

    /// Creates a native Responses provider with explicit operational policy.
    /// # Errors
    /// Returns invalid policy or HTTP client construction errors.
    pub fn new_with_transport(
        api_key: impl Into<String>,
        base_url: &str,
        model: impl Into<String>,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        Self::with_client(
            api_key,
            base_url,
            model,
            settings.streaming_client()?,
            settings,
        )
    }

    fn with_client(
        api_key: impl Into<String>,
        base_url: &str,
        model: impl Into<String>,
        client: reqwest::Client,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        let api_key = api_key.into();
        let model = model.into();
        if api_key.trim().is_empty() {
            return Err(Error::Config("provider API key cannot be empty".into()));
        }
        super::provider::validate_base_url(base_url)?;
        let mut socket_url =
            reqwest::Url::parse(&format!("{}/responses", base_url.trim_end_matches('/')))
                .map_err(|_| Error::Config("invalid OpenAI endpoint".into()))?;
        let scheme = if socket_url.scheme() == "https" {
            "wss"
        } else {
            "ws"
        };
        socket_url
            .set_scheme(scheme)
            .map_err(|_| Error::Config("invalid OpenAI socket scheme".into()))?;
        Self::with_authorization(
            Arc::new(ApiKeyAuthorization::new(api_key)),
            base_url,
            socket_url.as_str(),
            model,
            client,
            settings,
        )
    }

    pub(super) fn with_authorization(
        auth: Arc<dyn OpenAiAuthorization>,
        http_url: &str,
        socket_url: impl Into<String>,
        model: impl Into<String>,
        client: reqwest::Client,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        let model = model.into();
        let http = OpenAi::with_authorization(
            Arc::clone(&auth),
            http_url,
            model.clone(),
            client,
            settings,
        )?
        .with_tool_discovery(ToolDiscoveryMode::Native);
        Ok(Self {
            auth,
            transport: settings,
            socket_url: socket_url.into(),
            model,
            reasoning_effort: None,
            hosted_tools: Vec::new(),
            explicit_prompt_cache: false,
            sessions: Mutex::new(BTreeMap::new()),
            http,
        })
    }

    pub(super) fn with_service_tier(mut self, service_tier: Option<String>) -> Self {
        self.http = self.http.with_service_tier(service_tier);
        self
    }

    /// Configures validated model sockets and realtime calls.
    /// # Errors
    /// Returns an error for invalid operational settings.
    pub fn with_transport_settings(
        mut self,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        self.http = self.http.with_transport_settings(settings)?;
        self.transport = settings;
        Ok(self)
    }

    pub(super) fn with_codex_realtime_voice(mut self, base_url: &str) -> Result<Self> {
        self.http = self.http.with_codex_realtime_voice(base_url)?;
        Ok(self)
    }

    pub(super) fn with_image_api(
        mut self,
        api: Option<&'static super::image_generation::ImageApi>,
    ) -> Self {
        self.http = self.http.with_image_api(api);
        self
    }

    /// Uses explicit caching at user/developer messages and tool-result endpoints.
    /// Preserves earlier endpoints for prefix reuse instead of relying on implicit caching.
    /// Applies to WebSocket requests and HTTP fallback.
    pub fn with_explicit_prompt_cache(mut self) -> Self {
        self.explicit_prompt_cache = true;
        self.http = self.http.with_explicit_prompt_cache();
        self
    }

    /// Selects a Responses reasoning effort.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Result<Self> {
        let effort = effort.into();
        let supported = CATALOG
            .models
            .iter()
            .find(|model| model.id == self.model)
            .is_some_and(|model| model.reasoning.iter().any(|preset| preset.id == effort));
        if !supported {
            return Err(Error::Config(format!(
                "model `{}` does not support reasoning effort `{effort}`",
                self.model
            )));
        }
        self.http = self
            .http
            .with_reasoning_effort(effort.clone())?
            .with_reasoning_summary();
        self.reasoning_effort = Some(effort);
        Ok(self)
    }

    /// Enables provider-hosted live web search.
    #[must_use]
    pub fn with_web_search(mut self) -> Self {
        let tool = serde_json::json!({"type": "web_search"});
        self.hosted_tools.push(tool.clone());
        self.http = self.http.with_hosted_tool(tool);
        self
    }

    /// Enables provider-hosted cached-only web search.
    #[must_use]
    pub fn with_cached_web_search(mut self) -> Self {
        let tool = serde_json::json!({
            "type": "web_search",
            "external_web_access": false
        });
        self.hosted_tools.push(tool.clone());
        self.http = self.http.with_hosted_tool(tool);
        self
    }

    async fn send_response(
        &self,
        request: ModelRequest<'_>,
        events: ModelEventSink,
        media: Option<super::MediaPreparation<'_>>,
    ) -> Result<ModelOutput> {
        let session = self.session(request.session_id).await?;
        // A connection and its continuation cursor form one ordered session exchange.
        let mut state = session.lock().await;
        state.last_used_at = Instant::now();
        if state.use_http {
            drop(state);
            return self.send_http_response(request, events, media).await;
        }

        let mut rebuilt_context = false;
        loop {
            if !state
                .connection
                .as_ref()
                .is_some_and(OpenAiWsConnection::is_usable)
            {
                if let Some(connection) = state.connection.take() {
                    connection.close().await;
                }
                state.continuation = None;
                state.connection = Some(
                    match connect(
                        self.auth.as_ref(),
                        &self.socket_url,
                        request.session_id,
                        &self.transport,
                    )
                    .await
                    {
                        Ok(connection) => connection,
                        Err(Error::Provider(error)) if error.status() == Some(426) => {
                            state.use_http = true;
                            drop(state);
                            return self.send_http_response(request, events, media).await;
                        }
                        Err(Error::Provider(error)) if error.is_stream_interrupted() => {
                            state.last_used_at = Instant::now();
                            return Err(Error::Provider(error));
                        }
                        Err(error) => return Err(error),
                    },
                );
            }
            let envelope_fingerprint = envelope_fingerprint(
                &self.model,
                &request,
                self.reasoning_effort.as_deref(),
                &self.hosted_tools,
            )?;
            let (previous_response_id, input) = response_input(
                &mut state,
                request.input,
                request.allow_continuation,
                envelope_fingerprint,
            )?;
            let used_previous_response = previous_response_id.is_some();
            let body = super::media::encode_request(
                self,
                ModelRequest { input, ..request },
                media,
                !used_previous_response,
                |input| {
                    Ok(serde_json::to_vec(&self.prepared_body(
                        &request,
                        input,
                        previous_response_id.as_deref(),
                    )?)?)
                },
            )
            .await?;
            let body = String::from_utf8(body).expect("JSON serialization produces valid UTF-8");
            // Until sending begins, cancellation or preparation failure must retain the live connection.
            let mut connection = state.connection.take().ok_or_else(|| {
                Error::Provider("model connection disappeared before send".into())
            })?;
            match exchange(&mut connection, body, &events, request.cancellation).await? {
                Exchange::Completed(response) => {
                    let response_id = response
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| Error::Provider("response omitted id".into()))?
                        .to_string();
                    let output = decode_response(response)?;
                    let known_items = request.input.len() + output.output().len();
                    state.continuation = if request.allow_continuation {
                        Some(Continuation {
                            response_id,
                            known_items,
                            fingerprint: fingerprint(
                                request.input.iter().chain(output.output().iter()),
                            )?,
                            envelope_fingerprint,
                        })
                    } else {
                        None
                    };
                    state.last_used_at = Instant::now();
                    state.connection = Some(connection);
                    return Ok(output);
                }
                Exchange::PreviousMissing {
                    output_delivered: false,
                } if used_previous_response && !rebuilt_context => {
                    state.continuation = None;
                    state.connection = Some(connection);
                    rebuilt_context = true;
                }
                Exchange::PreviousMissing { .. } => {
                    state.continuation = None;
                    state.connection = Some(connection);
                    return Err(websocket_failure(&mut state, None));
                }
                Exchange::Retry { retry_after } => {
                    state.continuation = None;
                    state.connection = Some(connection);
                    return Err(websocket_failure(&mut state, retry_after));
                }
                Exchange::ConnectionLimit { retry_after } => {
                    state.continuation = None;
                    connection.close().await;
                    let error = websocket_failure(&mut state, retry_after);
                    drop(state);
                    self.close_idle_connections(request.session_id).await;
                    return Err(error);
                }
                Exchange::Reconnect => {
                    state.continuation = None;
                    drop(connection);
                    return Err(websocket_failure(&mut state, None));
                }
            }
        }
    }

    async fn send_http_response(
        &self,
        request: ModelRequest<'_>,
        events: ModelEventSink,
        media: Option<super::MediaPreparation<'_>>,
    ) -> Result<ModelOutput> {
        let result = match media {
            Some(media) => self.http.respond_prepared(request, events, media).await,
            None => self.http.respond(request, events).await,
        };
        result.map_err(|error| match error {
            Error::Http(_) => Error::Provider(ProviderError::stream_interrupted(None)),
            error => error,
        })
    }

    fn prepared_body<'a>(
        &'a self,
        request: &ModelRequest<'a>,
        input: super::ModelInput<'a>,
        previous: Option<&str>,
    ) -> Result<ResponsesBody<'a>> {
        let mut body = response_body(
            &self.model,
            request,
            input,
            previous,
            self.reasoning_effort.as_deref(),
            &self.hosted_tools,
            self.explicit_prompt_cache,
        )?;
        self.http.apply_service_tier(&mut body.metadata);
        Ok(body)
    }

    async fn session(&self, session_id: &str) -> Result<Arc<Mutex<SocketState>>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(session_id) {
            return Ok(Arc::clone(session));
        }

        let mut close = Vec::new();
        while sessions.len() >= MAX_SESSION_ENTRIES {
            let idle = sessions
                .iter()
                .filter(|(_, session)| Arc::strong_count(session) == 1)
                .filter_map(|(id, session)| {
                    let state = session.try_lock().ok()?;
                    Some((id, state.last_used_at))
                })
                .min_by_key(|(_, last_used_at)| *last_used_at)
                .map(|(id, _)| id.clone());
            if let Some(idle) = idle {
                if let Some(session) = sessions.remove(&idle)
                    && let Ok(mut state) = session.try_lock()
                    && let Some(connection) = state.connection.take()
                {
                    close.push(connection);
                }
            } else {
                return Err(Error::Provider(
                    format!("all {MAX_SESSION_ENTRIES} model sessions are currently active").into(),
                ));
            }
        }
        let session = Arc::new(Mutex::new(SocketState {
            connection: None,
            continuation: None,
            use_http: false,
            last_used_at: Instant::now(),
        }));
        sessions.insert(session_id.to_string(), Arc::clone(&session));
        drop(sessions);
        close_connections(close).await;
        Ok(session)
    }

    async fn close_idle_connections(&self, current_session_id: &str) {
        let mut close = Vec::new();
        let sessions = self.sessions.lock().await;
        for (session_id, session) in sessions.iter() {
            if session_id == current_session_id || Arc::strong_count(session) != 1 {
                continue;
            }
            let Ok(mut state) = session.try_lock() else {
                continue;
            };
            state.continuation = None;
            if let Some(connection) = state.connection.take() {
                close.push(connection);
            }
        }
        drop(sessions);
        close_connections(close).await;
    }
}

async fn close_connections(connections: Vec<OpenAiWsConnection>) {
    join_all(connections.into_iter().map(OpenAiWsConnection::close)).await;
}

fn websocket_failure(state: &mut SocketState, retry_after: Option<String>) -> Error {
    state.last_used_at = Instant::now();
    Error::Provider(ProviderError::stream_interrupted(retry_after))
}

fn response_input<'a>(
    state: &mut SocketState,
    input: super::ModelInput<'a>,
    allow_continuation: bool,
    envelope_fingerprint: u64,
) -> Result<(Option<String>, super::ModelInput<'a>)> {
    if allow_continuation {
        continuation_input(state, input, envelope_fingerprint)
    } else {
        state.continuation = None;
        Ok((None, input))
    }
}

impl Model for OpenAiSocket {
    fn transport_settings(&self) -> super::ModelTransportSettings {
        self.transport
    }

    fn info(&self) -> ModelInfo {
        ModelInfo {
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }

    fn supports_tool_image_input(&self) -> bool {
        self.supports_image_input()
    }

    fn supports_image_input(&self) -> bool {
        true
    }

    fn supports_image_generation(&self) -> bool {
        self.http.supports_image_generation()
    }

    fn generate_image<'a>(
        &'a self,
        request: ImageGenerationRequest<'a>,
    ) -> BoxFuture<'a, Result<GeneratedImage>> {
        self.http.generate_image(request)
    }

    fn supports_realtime_voice(&self) -> bool {
        self.http.supports_realtime_voice()
    }

    fn start_realtime_voice(
        &self,
        request: RealtimeVoiceRequest,
    ) -> BoxFuture<'_, Result<RealtimeVoiceCall>> {
        self.http.start_realtime_voice(request)
    }

    fn prompt_cache_capability(&self) -> PromptCacheMode {
        if self.explicit_prompt_cache {
            PromptCacheMode::Explicit
        } else {
            PromptCacheMode::Implicit
        }
    }

    fn tool_discovery(&self) -> ToolDiscoveryMode {
        ToolDiscoveryMode::Native
    }

    fn respond_prepared<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
        media: super::MediaPreparation<'a>,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(self.send_response(request, events, Some(media)))
    }

    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(self.send_response(request, events, None))
    }

    fn fallback_transport<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let session = self.session(session_id).await?;
            let mut state = session.lock().await;
            if state.use_http {
                return Ok(false);
            }
            state.use_http = true;
            state.continuation = None;
            state.connection = None;
            Ok(true)
        })
    }
}

fn response_body<'a>(
    model: &str,
    request: &ModelRequest<'a>,
    input: super::ModelInput<'a>,
    previous_response_id: Option<&str>,
    reasoning_effort: Option<&str>,
    hosted_tools: &'a [Value],
    explicit_prompt_cache: bool,
) -> Result<ResponsesBody<'a>> {
    let mut body = serde_json::json!({
        "type": "response.create",
        "model": model,
        "instructions": request.instructions,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "include": ["reasoning.encrypted_content"],
        "store": false
    });
    if let Some(prompt_cache) = request.prompt_cache {
        body["prompt_cache_key"] = Value::String(prompt_cache.key.into());
    }
    if explicit_prompt_cache {
        body["prompt_cache_options"] = serde_json::json!({"mode": "explicit"});
    }
    if let Some(response_id) = previous_response_id {
        body["previous_response_id"] = Value::String(response_id.into());
    }
    if let Some(effort) = reasoning_effort {
        body["reasoning"] = serde_json::json!({"effort": effort, "summary": "auto"});
    }
    Ok(ResponsesBody {
        metadata: body,
        tools: wire_tools(request.tools, hosted_tools, request.allow_hosted_tools),
        input: WireInput::new(
            input,
            true,
            explicit_prompt_cache,
            request.catalog_revision,
            request.deferred_tools,
        )?,
    })
}

fn envelope_fingerprint(
    model: &str,
    request: &ModelRequest<'_>,
    reasoning_effort: Option<&str>,
    hosted_tools: &[Value],
) -> Result<u64> {
    #[derive(serde::Serialize)]
    struct Cache<'a> {
        context_epoch: u64,
        key: &'a str,
        mode: &'static str,
    }
    #[derive(serde::Serialize)]
    struct Settings<'a> {
        catalog_revision: &'a str,
        instructions: &'a str,
        model: &'a str,
        prompt_cache: Option<Cache<'a>>,
        reasoning_effort: Option<&'a str>,
        tools: super::responses_wire::WireTools<'a>,
    }
    // Keep the sorted field order of the previous JSON settings projection.
    let envelope = Settings {
        catalog_revision: request.catalog_revision,
        instructions: request.instructions,
        model,
        prompt_cache: request.prompt_cache.map(|cache| Cache {
            context_epoch: cache.context_epoch,
            key: cache.key,
            mode: "explicit",
        }),
        reasoning_effort,
        tools: wire_tools(request.tools, hosted_tools, request.allow_hosted_tools),
    };
    fingerprint(std::iter::once(&envelope))
}

fn continuation_input<'a>(
    state: &mut SocketState,
    input: super::ModelInput<'a>,
    envelope_fingerprint: u64,
) -> Result<(Option<String>, super::ModelInput<'a>)> {
    let Some(continuation) = &state.continuation else {
        return Ok((None, input));
    };
    if continuation.envelope_fingerprint == envelope_fingerprint
        && continuation.known_items <= input.len()
        && fingerprint(input.prefix(continuation.known_items).iter())? == continuation.fingerprint
    {
        return Ok((
            // Keep the live cursor intact if preparing this continuation fails or is cancelled.
            Some(continuation.response_id.clone()),
            input.suffix(continuation.known_items),
        ));
    }
    state.continuation = None;
    Ok((None, input))
}

fn fingerprint<'a, T: serde::Serialize + 'a>(
    items: impl IntoIterator<Item = &'a T>,
) -> Result<u64> {
    let mut hasher = DefaultHasher::new();
    for item in items {
        let mut item_hasher = DefaultHasher::new();
        serde_json::to_writer(HasherWriter(&mut item_hasher), item)?;
        hasher.write_u64(item_hasher.finish());
    }
    Ok(hasher.finish())
}

struct HasherWriter<'a>(&'a mut DefaultHasher);

impl Write for HasherWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        "openai_socket",
        &MANIFEST,
        MANIFEST.api_key_auth(),
        Some(&CATALOG),
        build_provider,
    )
    .with_image_input()
    .with_image_generation()
    .with_realtime_voices(&super::realtime::VOICES, &super::realtime::OPENAI_MODELS)
}

fn build_provider(config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
    let api_key = config.credential.into_api_key("openai_socket")?;
    let base_url = config
        .base_url
        .as_deref()
        .unwrap_or(MANIFEST.base_url.as_str());
    let provider = OpenAiSocket::with_client(
        api_key,
        base_url,
        config.model,
        config.http,
        config.transport,
    )?
    .with_service_tier(config.service_tier);
    let provider = match config.reasoning_effort {
        Some(effort) => provider.with_reasoning_effort(effort)?,
        None => provider,
    };
    let provider = match config.web_search {
        HostedWebSearch::Off => provider,
        HostedWebSearch::Cached => provider.with_cached_web_search(),
        HostedWebSearch::Live => provider.with_web_search(),
    };
    Ok(Arc::new(provider))
}

#[cfg(test)]
#[path = "openai_socket_tests.rs"]
mod tests;
