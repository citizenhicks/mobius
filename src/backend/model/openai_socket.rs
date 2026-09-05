//! First-party OpenAI Responses WebSocket transport.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::io;
use std::io::Write;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::Ordering;
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
use self::connection::STREAM_IDLE_TIMEOUT;
#[cfg(test)]
use self::connection::SocketEvent;
use self::connection::connect;
use self::connection::exchange;
#[cfg(test)]
use self::connection::failed_exchange;
#[cfg(test)]
use self::connection::read_exchange;
use super::CompactOutput;
use super::CompactRequest;
use super::Model;
use super::ModelEventSink;
use super::ModelOutput;
use super::ModelPricing;
use super::ModelRequest;
use super::PromptCacheMode;
use super::openai::OpenAi;
use super::openai::decode_response;
use super::openai::wire_input_with_cache;
use super::openai::wire_tools;
use super::openai_auth::ApiKeyAuthorization;
use super::openai_auth::OpenAiAuthorization;
#[cfg(test)]
use super::openai_auth::ResolvedAuthorization;
use super::provider::HostedWebSearch;
use super::provider::ModelPreset;
use super::provider::ProviderAuth;
use super::provider::ProviderBuildConfig;
use super::provider::ProviderDefinition;
use super::{RealtimeVoiceCall, RealtimeVoiceRequest};
use crate::BoxFuture;
use crate::Error;
use crate::ProviderError;
use crate::Result;
use crate::protocol::ModelInfo;
use crate::protocol::ToolDiscoveryMode;

mod connection;

mod manifest {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_backend_model_openai_socket_manifest.rs"
    ));
}

const OPENAI_HTTP_URL: &str = "https://api.openai.com/v1";
const OPENAI_SOCKET_URL: &str = "wss://api.openai.com/v1/responses";
const MAX_SOCKET_SESSIONS: usize = 128;
const COMPACTION_STREAM_RETRY_LIMIT: usize = 2;
const COMPACTION_RETRY_BASE_DELAY: Duration = Duration::from_millis(200);

/// OpenAI's persistent Responses WebSocket transport.
pub struct OpenAiSocket {
    auth: Arc<dyn OpenAiAuthorization>,
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
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        Self::with_client(api_key, model, super::transport::streaming_client()?)
    }

    fn with_client(
        api_key: impl Into<String>,
        model: impl Into<String>,
        client: reqwest::Client,
    ) -> Result<Self> {
        let api_key = api_key.into();
        let model = model.into();
        if api_key.trim().is_empty() {
            return Err(Error::Config("OPENAI_API_KEY is empty".into()));
        }
        Self::with_authorization(
            Arc::new(ApiKeyAuthorization::new(api_key)),
            OPENAI_HTTP_URL,
            OPENAI_SOCKET_URL,
            model,
            client,
        )
        .map(Self::with_explicit_prompt_cache)
    }

    pub(super) fn with_authorization(
        auth: Arc<dyn OpenAiAuthorization>,
        http_url: &str,
        socket_url: impl Into<String>,
        model: impl Into<String>,
        client: reqwest::Client,
    ) -> Result<Self> {
        let model = model.into();
        let http = OpenAi::with_authorization(Arc::clone(&auth), http_url, model.clone(), client)?
            .with_tool_discovery(ToolDiscoveryMode::Native);
        Ok(Self {
            auth,
            socket_url: socket_url.into(),
            model,
            reasoning_effort: None,
            hosted_tools: Vec::new(),
            explicit_prompt_cache: false,
            sessions: Mutex::new(BTreeMap::new()),
            http,
        })
    }

    pub(super) fn with_codex_realtime_voice(mut self) -> Result<Self> {
        self.http = self.http.with_codex_realtime_voice()?;
        Ok(self)
    }

    fn with_explicit_prompt_cache(mut self) -> Self {
        self.explicit_prompt_cache = true;
        self.http = self.http.with_explicit_prompt_cache();
        self
    }

    /// Selects a Responses reasoning effort.
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Result<Self> {
        let effort = effort.into();
        let supported = manifest::MODELS
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
    ) -> Result<ModelOutput> {
        let session = self.session(request.session_id).await?;
        // A connection and its continuation cursor form one ordered session exchange.
        let mut state = session.lock().await;
        state.last_used_at = Instant::now();
        if state.use_http {
            drop(state);
            return self.send_http_response(request, events).await;
        }

        let mut rebuilt_context = false;
        loop {
            let mut connection = match state.connection.take() {
                Some(connection) if connection.is_usable() => connection,
                stale => {
                    if let Some(connection) = stale {
                        connection.close().await;
                    }
                    state.continuation = None;
                    match connect(self.auth.as_ref(), &self.socket_url, request.session_id).await {
                        Ok(connection) => connection,
                        Err(Error::Provider(error)) if error.status() == Some(426) => {
                            state.use_http = true;
                            state.continuation = None;
                            drop(state);
                            return self.send_http_response(request, events).await;
                        }
                        Err(Error::Provider(error)) if error.is_stream_interrupted() => {
                            return Err(websocket_failure(
                                &mut state,
                                error.retry_after().map(str::to_owned),
                            ));
                        }
                        Err(error) => return Err(error),
                    }
                }
            };
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
            let body = response_body(
                &self.model,
                &request,
                input,
                previous_response_id.as_deref(),
                self.reasoning_effort.as_deref(),
                &self.hosted_tools,
                self.explicit_prompt_cache,
            )?;
            match exchange(&mut connection, &body, &events).await? {
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
    ) -> Result<ModelOutput> {
        self.http
            .respond(request, events)
            .await
            .map_err(|error| match error {
                Error::Http(_) => Error::Provider("HTTPS fallback transport failed".into()),
                error => error,
            })
    }

    async fn compact_response(&self, request: CompactRequest<'_>) -> Result<CompactOutput> {
        let mut input = request.input.to_vec();
        input.push(serde_json::json!({"type": "compaction_trigger"}));
        let mut retries = 0;
        let output = loop {
            match self
                .send_response(
                    ModelRequest {
                        session_id: request.session_id,
                        prompt_cache: request.prompt_cache,
                        instructions: request.instructions,
                        input: &input,
                        catalog_revision: request.catalog_revision,
                        tools: request.tools,
                        deferred_tools: request.deferred_tools,
                        allow_hosted_tools: true,
                        allow_continuation: true,
                    },
                    Arc::new(|_| Ok(())),
                )
                .await
            {
                Ok(output) => break output,
                Err(Error::Provider(error))
                    if error.is_stream_interrupted() && retries < COMPACTION_STREAM_RETRY_LIMIT =>
                {
                    let delay = compaction_retry_delay(&error, retries);
                    retries += 1;
                    tokio::time::sleep(delay).await;
                }
                Err(error) => return Err(error),
            }
        };
        let compaction = output
            .output()
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
            .cloned()
            .collect::<Vec<_>>();
        if compaction.len() != 1 {
            return Err(Error::Provider(
                format!(
                    "Responses compaction expected exactly one compaction item, got {}",
                    compaction.len()
                )
                .into(),
            ));
        }
        CompactOutput::from_output(compaction, output.usage().clone())
    }

    async fn session(&self, session_id: &str) -> Result<Arc<Mutex<SocketState>>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(session_id) {
            return Ok(Arc::clone(session));
        }

        let mut close = Vec::new();
        let websocket_sessions = sessions
            .values()
            .filter(|session| session.try_lock().map_or(true, |state| !state.use_http))
            .count();
        if websocket_sessions >= MAX_SOCKET_SESSIONS {
            let idle = sessions
                .iter()
                .filter(|(_, session)| Arc::strong_count(session) == 1)
                .filter_map(|(id, session)| {
                    let state = session.try_lock().ok()?;
                    (!state.use_http).then_some((id.clone(), state.last_used_at))
                })
                .min_by_key(|(_, last_used_at)| *last_used_at)
                .map(|(id, _)| id);
            if let Some(idle) = idle {
                if let Some(session) = sessions.remove(&idle)
                    && let Ok(mut state) = session.try_lock()
                    && let Some(connection) = state.connection.take()
                {
                    close.push(connection);
                }
            } else {
                return Err(Error::Provider(
                    format!("all {MAX_SOCKET_SESSIONS} WebSocket sessions are currently active")
                        .into(),
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

fn compaction_retry_delay(error: &crate::ProviderError, retry: usize) -> Duration {
    let backoff = COMPACTION_RETRY_BASE_DELAY.saturating_mul(1_u32 << retry.min(4));
    error
        .retry_after()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .map_or(backoff, |retry_after| retry_after.max(backoff))
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
    input: &'a [Value],
    allow_continuation: bool,
    envelope_fingerprint: u64,
) -> Result<(Option<String>, &'a [Value])> {
    if allow_continuation {
        continuation_input(state, input, envelope_fingerprint)
    } else {
        state.continuation = None;
        Ok((None, input))
    }
}

impl Model for OpenAiSocket {
    fn info(&self) -> ModelInfo {
        ModelInfo {
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }

    fn supports_image_input(&self) -> bool {
        true
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

    fn pricing(&self) -> Option<ModelPricing> {
        self.http.pricing()
    }

    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(self.send_response(request, events))
    }

    fn compaction_endpoint(&self) -> bool {
        true
    }

    fn compact<'a>(&'a self, request: CompactRequest<'a>) -> BoxFuture<'a, Result<CompactOutput>> {
        Box::pin(self.compact_response(request))
    }
}

fn response_body(
    model: &str,
    request: &ModelRequest<'_>,
    input: &[Value],
    previous_response_id: Option<&str>,
    reasoning_effort: Option<&str>,
    hosted_tools: &[Value],
    explicit_prompt_cache: bool,
) -> Result<Value> {
    let mut body = serde_json::json!({
        "type": "response.create",
        "model": model,
        "instructions": request.instructions,
        "input": wire_input_with_cache(
            input,
            true,
            explicit_prompt_cache,
            request.catalog_revision,
            request.deferred_tools,
        )?,
        "tools": wire_tools(request.tools, hosted_tools, request.allow_hosted_tools),
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
    Ok(body)
}

fn envelope_fingerprint(
    model: &str,
    request: &ModelRequest<'_>,
    reasoning_effort: Option<&str>,
    hosted_tools: &[Value],
) -> Result<u64> {
    let envelope = serde_json::json!({
        "model": model,
        "instructions": request.instructions,
        "catalog_revision": request.catalog_revision,
        "tools": wire_tools(request.tools, hosted_tools, request.allow_hosted_tools),
        "reasoning_effort": reasoning_effort,
        "prompt_cache": request.prompt_cache.map(|cache| {
            serde_json::json!({
                "key": cache.key,
                "context_epoch": cache.context_epoch,
                "mode": "explicit"
            })
        })
    });
    fingerprint(std::iter::once(&envelope))
}

fn continuation_input<'a>(
    state: &mut SocketState,
    input: &'a [Value],
    envelope_fingerprint: u64,
) -> Result<(Option<String>, &'a [Value])> {
    let Some(continuation) = &state.continuation else {
        return Ok((None, input));
    };
    if continuation.envelope_fingerprint == envelope_fingerprint
        && continuation.known_items <= input.len()
        && fingerprint(input[..continuation.known_items].iter())? == continuation.fingerprint
    {
        return Ok((
            Some(continuation.response_id.clone()),
            &input[continuation.known_items..],
        ));
    }
    state.continuation = None;
    Ok((None, input))
}

fn fingerprint<'a>(items: impl IntoIterator<Item = &'a Value>) -> Result<u64> {
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

pub(super) const MODELS: &[ModelPreset] = manifest::MODELS;
pub(super) const DEFAULT_MODEL: Option<&str> = manifest::DEFAULT_MODEL;
pub(super) const SEARCH: &[HostedWebSearch] = manifest::SEARCH;

pub(super) const fn provider() -> ProviderDefinition {
    ProviderDefinition::new(
        "openai_socket",
        manifest::PROVIDER_LABEL,
        "chat_gpt",
        manifest::PROVIDER_DESCRIPTION,
        ProviderAuth::ApiKey("OPENAI_API_KEY"),
        MODELS,
        DEFAULT_MODEL,
        SEARCH,
        build_provider,
    )
    .with_image_input()
    .with_realtime_voice()
    .with_tool_discovery(
        manifest::TOOL_DISCOVERY,
        manifest::CUSTOM_ENDPOINT_TOOL_DISCOVERY,
    )
}

fn build_provider(config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
    let api_key = config.credential.into_api_key("openai_socket")?;
    let provider = OpenAiSocket::with_client(api_key, config.model, config.http)?;
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
