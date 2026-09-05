//! OpenAI Responses API Adapter.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use reqwest::Client;
use serde_json::Value;

use super::CompactOutput;
use super::CompactRequest;
use super::Model;
use super::ModelEventSink;
use super::ModelOutput;
use super::ModelPricing;
use super::ModelRequest;
use super::PROMPT_CACHE_BREAKPOINT_FIELD;
use super::PromptCacheMode;
use super::TOOLS_SEARCH_NAME;
use super::ToolDefinition;
use super::ToolLoad;
use super::image_data_url;
use super::image_input;
use super::openai_auth::ApiKeyAuthorization;
use super::openai_auth::OpenAiAuthorization;
#[cfg(test)]
use super::openai_auth::ResolvedAuthorization;
use super::provider::ProviderAuth;
use super::provider::ProviderBuildConfig;
use super::provider::ProviderDefinition;
use super::provider::validate_base_url;
use super::realtime::{RealtimeTransport, VoiceApi};
use super::transport::MAX_SSE_FRAME_BYTES;
use super::transport::frame_data;
use super::transport::push_sse_chunk;
use super::transport::read_limited;
use super::transport::status_error;
use super::transport::streaming_client;
use super::transport::take_sse_frame;
use super::usage_i64;
use super::{RealtimeVoiceCall, RealtimeVoiceRequest};
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::protocol::ModelEvent;
use crate::protocol::ModelInfo;
use crate::protocol::ModelStepAnnotation;
use crate::protocol::TokenUsage;
use crate::protocol::ToolDiscoveryMode;
use crate::protocol::WebSearchAction;

mod manifest {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_backend_model_openai_manifest.rs"
    ));
}

const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_STREAM_OUTPUT_ITEMS: usize = 1_024;
const REQUEST_MAX_RETRIES: u32 = 4;
const REQUEST_RETRY_BASE_DELAY_MS: u64 = 200;
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolDiscoveryWire {
    Rebuild,
    AdditionalTools,
    OpenRouter,
}

impl ToolDiscoveryWire {
    const fn mode(self) -> ToolDiscoveryMode {
        match self {
            Self::Rebuild => ToolDiscoveryMode::Rebuild,
            Self::AdditionalTools | Self::OpenRouter => ToolDiscoveryMode::Native,
        }
    }
}

fn openai_model_pricing(model: &str) -> Option<ModelPricing> {
    let pricing = match model {
        "gpt-6-astra" => ModelPricing::new(10_000_000, 1_000_000, 12_500_000, 50_000_000),
        "gpt-5.6-sol" => ModelPricing::new(5_000_000, 500_000, 6_250_000, 30_000_000),
        "gpt-5.6-terra" => ModelPricing::new(2_000_000, 200_000, 2_500_000, 12_000_000),
        "gpt-5.6-luna" => ModelPricing::new(200_000, 20_000, 250_000, 1_200_000),
        _ => return None,
    };
    Some(pricing.with_long_context(272_000, 2_000, 1_500))
}

/// OpenAI Responses API configuration.
pub struct OpenAi {
    client: Client,
    auth: Option<Arc<dyn OpenAiAuthorization>>,
    realtime: Option<RealtimeTransport>,
    base_url: String,
    model: String,
    reasoning_effort: Option<String>,
    reasoning_summary: bool,
    hosted_tools: Vec<Value>,
    compaction_endpoint: bool,
    image_input: bool,
    explicit_prompt_cache: bool,
    tool_discovery: ToolDiscoveryWire,
}

impl OpenAi {
    /// Creates an OpenAI or Responses-compatible provider.
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self> {
        Self::with_client(Some(api_key.into()), base_url, model, streaming_client()?)
    }

    pub(super) fn with_client(
        api_key: Option<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        client: Client,
    ) -> Result<Self> {
        if api_key.as_deref().is_some_and(|key| key.trim().is_empty()) {
            return Err(Error::Config("OPENAI_API_KEY is empty".into()));
        }
        let auth = api_key
            .map(|key| Arc::new(ApiKeyAuthorization::new(key)) as Arc<dyn OpenAiAuthorization>);
        Self::from_parts(auth, base_url, model, client)
    }

    pub(super) fn with_authorization(
        auth: Arc<dyn OpenAiAuthorization>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        client: Client,
    ) -> Result<Self> {
        Self::from_parts(Some(auth), base_url, model, client)
    }

    fn from_parts(
        auth: Option<Arc<dyn OpenAiAuthorization>>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        client: Client,
    ) -> Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let model = model.into();
        validate_base_url(&base_url)?;
        if model.trim().is_empty() {
            return Err(Error::Config("OPENAI_MODEL is empty".into()));
        }
        let realtime = if base_url == DEFAULT_BASE_URL {
            auth.as_ref()
                .map(|auth| RealtimeTransport::new(VoiceApi::OpenAi, Arc::clone(auth)))
                .transpose()?
        } else {
            None
        };
        Ok(Self {
            client,
            auth,
            realtime,
            base_url,
            model,
            reasoning_effort: None,
            reasoning_summary: false,
            hosted_tools: Vec::new(),
            compaction_endpoint: false,
            image_input: true,
            explicit_prompt_cache: false,
            tool_discovery: ToolDiscoveryWire::Rebuild,
        })
    }

    pub(super) fn with_codex_realtime_voice(mut self) -> Result<Self> {
        let auth = self
            .auth
            .as_ref()
            .ok_or_else(|| Error::Config("Codex voice requires authorization".into()))?;
        self.realtime = Some(RealtimeTransport::new(VoiceApi::Codex, Arc::clone(auth))?);
        Ok(self)
    }

    /// Selects a Responses reasoning effort.
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Result<Self> {
        let effort = effort.into();
        if effort.trim().is_empty() {
            return Err(Error::Config("reasoning effort cannot be empty".into()));
        }
        self.reasoning_effort = Some(effort);
        Ok(self)
    }

    /// Requests automatic reasoning summaries from an endpoint known to support them.
    #[must_use]
    pub fn with_reasoning_summary(mut self) -> Self {
        self.reasoning_summary = true;
        self
    }

    /// Enables provider-hosted live web search.
    #[must_use]
    pub fn with_web_search(self) -> Self {
        self.with_hosted_tool(serde_json::json!({"type": "web_search"}))
    }

    /// Enables provider-hosted cached-only web search.
    #[must_use]
    pub fn with_cached_web_search(self) -> Self {
        self.with_hosted_tool(serde_json::json!({
            "type": "web_search",
            "external_web_access": false
        }))
    }

    /// Adds one provider-specific hosted tool to Responses requests.
    #[must_use]
    pub fn with_hosted_tool(mut self, tool: Value) -> Self {
        self.hosted_tools.push(tool);
        self
    }

    /// Marks an endpoint that implements native Responses compaction.
    #[must_use]
    pub fn with_compaction_endpoint(mut self) -> Self {
        self.compaction_endpoint = true;
        self
    }

    /// Disables image input for a Responses-compatible endpoint that rejects it.
    #[must_use]
    pub(super) fn without_image_input(mut self) -> Self {
        self.image_input = false;
        self
    }

    pub(super) fn with_explicit_prompt_cache(mut self) -> Self {
        self.explicit_prompt_cache = true;
        self
    }

    pub(super) fn with_tool_discovery(mut self, mode: ToolDiscoveryMode) -> Self {
        self.tool_discovery = match mode {
            ToolDiscoveryMode::Native => ToolDiscoveryWire::AdditionalTools,
            ToolDiscoveryMode::Rebuild => ToolDiscoveryWire::Rebuild,
        };
        self
    }

    pub(super) fn with_openrouter_tool_search(mut self) -> Self {
        self.tool_discovery = ToolDiscoveryWire::OpenRouter;
        self
    }

    async fn send_response(
        &self,
        request: ModelRequest<'_>,
        events: ModelEventSink,
    ) -> Result<ModelOutput> {
        let session_id = request.session_id;
        let deferred_tools = request.deferred_tools;
        let body = self.response_body(request)?;
        let mut response = self
            .send_authorized("responses", &body, true, Some(session_id))
            .await?;
        if !response.status().is_success() {
            return Err(status_error(response, "Responses").await);
        }

        let mut bytes = Vec::new();
        let mut commentary = BTreeSet::new();
        let mut reasoning_part = None;
        let mut web_searches = BTreeSet::new();
        let mut output = BTreeMap::new();
        let mut stream_bytes = 0;
        while let Some(chunk) = response.chunk().await? {
            push_sse_chunk(&mut bytes, &mut stream_bytes, &chunk, "Responses")?;
            while let Some(frame) = take_sse_frame(&mut bytes)? {
                let Some(data) = frame_data(&frame) else {
                    continue;
                };
                if data == "[DONE]" {
                    continue;
                }
                let event: Value = serde_json::from_str(&data)?;
                collect_stream_output(&event, &mut output)?;
                if emit_web_event(&event, &mut web_searches, &events)? {
                    continue;
                }
                if emit_reasoning_event(&event, &mut reasoning_part, &events)? {
                    continue;
                }
                if emit_text_event(&event, &mut commentary, &events)? {
                    continue;
                }
                if let Some(response) = self.finish_stream_event(&event, &output, deferred_tools)? {
                    emit_citation_web_search(&response, &web_searches, &events)?;
                    return Ok(response);
                }
            }
            if bytes.len() > MAX_SSE_FRAME_BYTES {
                return Err(Error::Provider("SSE frame exceeded size limit".into()));
            }
        }
        Err(Error::Provider(
            "stream ended before response.completed".into(),
        ))
    }

    fn finish_stream_event(
        &self,
        event: &Value,
        output: &BTreeMap<u64, Value>,
        deferred_tools: &[ToolDefinition],
    ) -> Result<Option<ModelOutput>> {
        match event.get("type").and_then(Value::as_str) {
            Some("response.completed") => {
                let response = event
                    .get("response")
                    .cloned()
                    .map(|response| attach_stream_output(response, output))
                    .ok_or_else(|| Error::Provider("completion omitted response".into()))?;
                self.decode_response(response, deferred_tools).map(Some)
            }
            Some("error" | "response.failed" | "response.incomplete") => {
                Err(Error::Provider(response_error(event).into()))
            }
            _ => Ok(None),
        }
    }

    fn response_body(&self, request: ModelRequest<'_>) -> Result<Value> {
        let additional_tools = match self.tool_discovery {
            ToolDiscoveryWire::AdditionalTools => request.deferred_tools,
            ToolDiscoveryWire::Rebuild | ToolDiscoveryWire::OpenRouter => &[],
        };
        let mut body = serde_json::json!({
            "model": self.model,
            "instructions": request.instructions,
            "input": wire_input_with_cache(
                request.input,
                self.image_input,
                self.explicit_prompt_cache,
                request.catalog_revision,
                additional_tools,
            )?,
            "tools": self.wire_request_tools(&request),
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "include": ["reasoning.encrypted_content"],
            "store": false,
            "stream": true
        });
        if let Some(prompt_cache) = request.prompt_cache {
            body["prompt_cache_key"] = Value::String(prompt_cache.key.into());
        }
        if self.explicit_prompt_cache {
            body["prompt_cache_options"] = serde_json::json!({"mode": "explicit"});
        }
        if let Some(reasoning) = self.reasoning() {
            body["reasoning"] = reasoning;
        }
        Ok(body)
    }

    fn wire_request_tools(&self, request: &ModelRequest<'_>) -> Vec<Value> {
        if self.tool_discovery != ToolDiscoveryWire::OpenRouter {
            return wire_tools(
                request.tools,
                &self.hosted_tools,
                request.allow_hosted_tools,
            );
        }

        let mut tools = vec![serde_json::json!({"type": "openrouter:tool_search"})];
        tools.extend(
            request
                .tools
                .iter()
                .filter(|tool| tool.name != TOOLS_SEARCH_NAME)
                .map(wire_function_tool),
        );
        tools.extend(request.deferred_tools.iter().map(|tool| {
            let mut tool = wire_function_tool(tool);
            tool["defer_loading"] = Value::Bool(true);
            tool
        }));
        if request.allow_hosted_tools {
            tools.extend_from_slice(&self.hosted_tools);
        }
        tools
    }

    fn decode_response(
        &self,
        response: Value,
        deferred_tools: &[ToolDefinition],
    ) -> Result<ModelOutput> {
        let output = decode_response(response)?;
        if self.tool_discovery != ToolDiscoveryWire::OpenRouter {
            return Ok(output);
        }
        let deferred = deferred_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<BTreeSet<_>>();
        let loaded = output
            .tool_calls
            .iter()
            .filter(|call| deferred.contains(call.name.as_str()))
            .map(|call| call.name.clone())
            .collect::<Vec<_>>();
        output.with_materialized_tools(loaded)
    }

    async fn compact_response(&self, request: CompactRequest<'_>) -> Result<CompactOutput> {
        if !self.compaction_endpoint {
            return Err(Error::Provider(
                "OpenAI-compatible provider has no compaction endpoint".into(),
            ));
        }
        let session_id = request.session_id;
        let body = self.compact_body(request)?;
        let response = self
            .send_authorized("responses/compact", &body, false, Some(session_id))
            .await?;
        if !response.status().is_success() {
            return Err(status_error(response, "Responses").await);
        }
        let response: Value =
            serde_json::from_slice(&read_limited(response, MAX_JSON_BYTES, "Responses").await?)?;
        decode_compact_response(response)
    }

    async fn send_authorized(
        &self,
        endpoint: &str,
        body: &Value,
        streaming: bool,
        session_id: Option<&str>,
    ) -> Result<reqwest::Response> {
        for attempt in 0..2 {
            let mut request = self
                .client
                .post(format!("{}/{endpoint}", self.base_url))
                .json(body);
            let Some(auth) = &self.auth else {
                return Self::send_request(endpoint, request).await;
            };
            let authorization = auth.authorize_http(streaming, session_id).await?;
            let rejected_token = authorization.token.clone();
            request = request.bearer_auth(authorization.token);
            for (name, value) in authorization.headers {
                request = request.header(name, value);
            }
            let response = Self::send_request(endpoint, request).await?;
            if response.status() != reqwest::StatusCode::UNAUTHORIZED || attempt == 1 {
                return Ok(response);
            }
            if !auth.recover_unauthorized(&rejected_token).await? {
                return Ok(response);
            }
        }
        unreachable!("authorized request retry is bounded")
    }

    async fn send_request(
        endpoint: &str,
        mut request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let started = Instant::now();
        for attempt in 0..=REQUEST_MAX_RETRIES {
            let retry = request.try_clone();
            match request.send().await {
                Ok(response) => return Ok(response),
                Err(error) if attempt < REQUEST_MAX_RETRIES => {
                    let Some(next_request) = retry else {
                        return Err(Self::logged_http_error(endpoint, error, started.elapsed()));
                    };
                    let retry = attempt + 1;
                    let elapsed = started.elapsed();
                    let delay = Self::request_retry_delay(retry, elapsed);
                    Self::log_http_retry(endpoint, &error, elapsed, retry, delay);
                    tokio::time::sleep(delay).await;
                    request = next_request;
                }
                Err(error) => {
                    return Err(Self::logged_http_error(endpoint, error, started.elapsed()));
                }
            }
        }
        unreachable!("HTTP request retry is bounded")
    }

    fn request_retry_delay(retry: u32, elapsed: Duration) -> Duration {
        let exponential = 1_u64 << retry.saturating_sub(1).min(4);
        let jitter_percent = 90 + u64::from(elapsed.subsec_nanos() % 21);
        Duration::from_millis(
            REQUEST_RETRY_BASE_DELAY_MS
                .saturating_mul(exponential)
                .saturating_mul(jitter_percent)
                / 100,
        )
    }

    fn log_http_retry(
        endpoint: &str,
        error: &reqwest::Error,
        elapsed: Duration,
        retry: u32,
        delay: Duration,
    ) {
        let host = error
            .url()
            .and_then(|url| url.host_str())
            .unwrap_or("unknown");
        eprintln!(
            "model HTTP request failed; retrying: host={host} endpoint={endpoint} elapsed_ms={} connect={} timeout={} request={} retry={retry}/{REQUEST_MAX_RETRIES} delay_ms={} source={:?}",
            elapsed.as_millis(),
            error.is_connect(),
            error.is_timeout(),
            error.is_request(),
            delay.as_millis(),
            std::error::Error::source(error),
        );
    }

    fn logged_http_error(endpoint: &str, error: reqwest::Error, elapsed: Duration) -> Error {
        let host = error
            .url()
            .and_then(|url| url.host_str())
            .unwrap_or("unknown");
        eprintln!(
            "model HTTP request failed: host={host} endpoint={endpoint} elapsed_ms={} connect={} timeout={} request={} source={:?}",
            elapsed.as_millis(),
            error.is_connect(),
            error.is_timeout(),
            error.is_request(),
            std::error::Error::source(&error),
        );
        Error::Http(error)
    }

    fn compact_body(&self, request: CompactRequest<'_>) -> Result<Value> {
        let additional_tools = match self.tool_discovery {
            ToolDiscoveryWire::AdditionalTools => request.deferred_tools,
            ToolDiscoveryWire::Rebuild | ToolDiscoveryWire::OpenRouter => &[],
        };
        let mut body = serde_json::json!({
            "model": self.model,
            "instructions": request.instructions,
            "input": wire_input_with_cache(
                request.input,
                self.image_input,
                self.explicit_prompt_cache,
                request.catalog_revision,
                additional_tools,
            )?,
            "tools": wire_tools(request.tools, &self.hosted_tools, true),
            "parallel_tool_calls": true,
        });
        if let Some(prompt_cache) = request.prompt_cache {
            body["prompt_cache_key"] = Value::String(prompt_cache.key.into());
        }
        if self.explicit_prompt_cache {
            body["prompt_cache_options"] = serde_json::json!({"mode": "explicit"});
        }
        if let Some(reasoning) = self.reasoning() {
            body["reasoning"] = reasoning;
        }
        Ok(body)
    }

    fn reasoning(&self) -> Option<Value> {
        if self.reasoning_effort.is_none() && !self.reasoning_summary {
            return None;
        }
        let mut reasoning = serde_json::Map::new();
        if let Some(effort) = &self.reasoning_effort {
            reasoning.insert("effort".into(), Value::String(effort.clone()));
        }
        if self.reasoning_summary {
            reasoning.insert("summary".into(), Value::String("auto".into()));
        }
        Some(Value::Object(reasoning))
    }
}

impl Model for OpenAi {
    fn info(&self) -> ModelInfo {
        ModelInfo {
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }

    fn supports_image_input(&self) -> bool {
        self.image_input
    }

    fn supports_realtime_voice(&self) -> bool {
        self.realtime.is_some()
    }

    fn start_realtime_voice(
        &self,
        request: RealtimeVoiceRequest,
    ) -> BoxFuture<'_, Result<RealtimeVoiceCall>> {
        Box::pin(async move {
            self.realtime
                .as_ref()
                .ok_or_else(|| {
                    Error::Provider("realtime voice is unavailable for this provider".into())
                })?
                .start(request)
                .await
        })
    }

    fn prompt_cache_capability(&self) -> PromptCacheMode {
        if self.explicit_prompt_cache {
            PromptCacheMode::Explicit
        } else {
            PromptCacheMode::Implicit
        }
    }

    fn tool_discovery(&self) -> ToolDiscoveryMode {
        self.tool_discovery.mode()
    }

    fn pricing(&self) -> Option<ModelPricing> {
        (self.base_url == DEFAULT_BASE_URL)
            .then(|| openai_model_pricing(&self.model))
            .flatten()
    }

    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(self.send_response(request, events))
    }

    fn compaction_endpoint(&self) -> bool {
        self.compaction_endpoint
    }

    fn compact<'a>(&'a self, request: CompactRequest<'a>) -> BoxFuture<'a, Result<CompactOutput>> {
        Box::pin(self.compact_response(request))
    }
}

pub(super) fn wire_input_with_cache(
    input: &[Value],
    allow_images: bool,
    explicit_prompt_cache: bool,
    catalog_revision: &str,
    additional_tools: &[ToolDefinition],
) -> Result<Vec<Value>> {
    let mut wired = Vec::with_capacity(input.len());
    for item in input {
        if let Some(load) = ToolLoad::from_input(item)? {
            if load.catalog_revision == catalog_revision {
                let tools = load
                    .tools
                    .iter()
                    .filter_map(|name| additional_tools.iter().find(|tool| tool.name == *name))
                    .map(wire_function_tool)
                    .collect::<Vec<_>>();
                if !tools.is_empty() {
                    wired.push(serde_json::json!({
                        "type": "additional_tools",
                        "role": "developer",
                        "tools": tools,
                    }));
                }
            }
            continue;
        }
        let mut item = item.clone();
        if let Some(fields) = item.as_object_mut() {
            fields.retain(|name, _| !name.starts_with('_'));
        }
        strip_replay_wire_metadata(&mut item);
        let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
            wired.push(item);
            continue;
        };
        for part in content {
            let breakpoint = part
                .get(PROMPT_CACHE_BREAKPOINT_FIELD)
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Some(fields) = part.as_object_mut() {
                fields.retain(|name, _| !name.starts_with('_'));
                if breakpoint && explicit_prompt_cache {
                    fields.insert(
                        "prompt_cache_breakpoint".into(),
                        serde_json::json!({"mode": "explicit"}),
                    );
                }
            }
            if part.get("type").and_then(Value::as_str) != Some("input_image") {
                continue;
            }
            if !allow_images {
                return Err(Error::Provider(
                    "this model provider does not support image attachments".into(),
                ));
            }
            let Some((media_type, data)) = image_input(part, "Responses")? else {
                continue;
            };
            *part = serde_json::json!({
                "type": "input_image",
                "image_url": image_data_url(media_type, data)
            });
        }
        wired.push(item);
    }
    Ok(wired)
}

pub(super) fn collect_stream_output(
    event: &Value,
    output: &mut BTreeMap<u64, Value>,
) -> Result<()> {
    if event.get("type").and_then(Value::as_str) != Some("response.output_item.done") {
        return Ok(());
    }
    let item = event
        .get("item")
        .cloned()
        .ok_or_else(|| Error::Provider("completed output item omitted item".into()))?;
    let index = event
        .get("output_index")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            output
                .last_key_value()
                .map_or(0, |(index, _)| index.saturating_add(1))
        });
    if output.len() >= MAX_STREAM_OUTPUT_ITEMS && !output.contains_key(&index) {
        return Err(Error::Provider(
            format!("response returned more than {MAX_STREAM_OUTPUT_ITEMS} output items").into(),
        ));
    }
    if output.insert(index, item).is_some() {
        return Err(Error::Provider(
            format!("response repeated output item index {index}").into(),
        ));
    }
    Ok(())
}

pub(super) fn attach_stream_output(mut response: Value, output: &BTreeMap<u64, Value>) -> Value {
    if !output.is_empty() {
        response["output"] = Value::Array(output.values().cloned().collect());
    }
    response
}

pub(super) fn wire_tools(
    tools: &[ToolDefinition],
    hosted_tools: &[Value],
    allow_hosted_tools: bool,
) -> Vec<Value> {
    let mut tools = tools.iter().map(wire_function_tool).collect::<Vec<_>>();
    if allow_hosted_tools {
        tools.extend_from_slice(hosted_tools);
    }
    tools
}

fn wire_function_tool(tool: &ToolDefinition) -> Value {
    serde_json::json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
        "strict": false
    })
}

pub(super) const fn generic_provider() -> ProviderDefinition {
    ProviderDefinition::new(
        "responses",
        manifest::PROVIDER_LABEL,
        "storage",
        manifest::PROVIDER_DESCRIPTION,
        ProviderAuth::ApiKey("OPENAI_API_KEY"),
        manifest::MODELS,
        manifest::DEFAULT_MODEL,
        manifest::SEARCH,
        build_generic,
    )
    .with_image_input()
    .with_realtime_voices(super::realtime::VOICES)
    .with_tool_discovery(
        manifest::TOOL_DISCOVERY,
        manifest::CUSTOM_ENDPOINT_TOOL_DISCOVERY,
    )
    .with_base_url(DEFAULT_BASE_URL)
    .with_credentialless_endpoints()
}

fn build_generic(config: ProviderBuildConfig) -> Result<std::sync::Arc<dyn Model>> {
    let base_url = config
        .base_url
        .ok_or_else(|| Error::Config("Responses provider requires a base URL".into()))?;
    let api_key = config.credential.into_optional_api_key("responses")?;
    let provider = OpenAi::with_client(api_key, base_url, config.model, config.http)?;
    let provider = match config.reasoning_effort {
        Some(effort) => provider.with_reasoning_effort(effort)?,
        None => provider,
    };
    Ok(std::sync::Arc::new(provider))
}

fn web_search_item(event: &Value) -> Option<&Value> {
    event.get("item").filter(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("web_search_call" | "openrouter:web_search")
        )
    })
}

fn emit_citation_web_search(
    output: &ModelOutput,
    web_searches: &BTreeSet<String>,
    events: &ModelEventSink,
) -> Result<()> {
    if !web_searches.is_empty()
        || !output.content().iter().any(|part| {
            part.annotations
                .iter()
                .any(|annotation| matches!(annotation, ModelStepAnnotation::UrlCitation { .. }))
        })
    {
        return Ok(());
    }
    let call_id = "citations".to_string();
    events(ModelEvent::WebSearchStarted {
        call_id: call_id.clone(),
    })?;
    events(ModelEvent::WebSearchCompleted {
        call_id,
        action: WebSearchAction::Other,
    })
}

pub(super) fn emit_web_event(
    event: &Value,
    seen: &mut BTreeSet<String>,
    events: &ModelEventSink,
) -> Result<bool> {
    let Some(item) = web_search_item(event) else {
        return Ok(false);
    };
    let call_id = required_string(item, "id")?.to_string();
    let added = seen.insert(call_id.clone());
    if added {
        events(ModelEvent::WebSearchStarted {
            call_id: call_id.clone(),
        })?;
    }
    if event.get("type").and_then(Value::as_str) == Some("response.output_item.done") {
        events(ModelEvent::WebSearchCompleted {
            call_id,
            action: decode_web_action(item),
        })?;
    }
    Ok(true)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReasoningPartKind {
    Summary,
    Content,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct ReasoningPart {
    kind: ReasoningPartKind,
    output_index: usize,
    part_index: usize,
}

pub(super) fn emit_reasoning_event(
    event: &Value,
    previous_part: &mut Option<ReasoningPart>,
    events: &ModelEventSink,
) -> Result<bool> {
    let (kind, part_field) = match event.get("type").and_then(Value::as_str) {
        Some("response.reasoning_summary_text.delta") => {
            (ReasoningPartKind::Summary, "summary_index")
        }
        Some("response.reasoning_text.delta") => (ReasoningPartKind::Content, "content_index"),
        _ => return Ok(false),
    };
    let Some(delta) = event
        .get("delta")
        .and_then(Value::as_str)
        .filter(|delta| !delta.is_empty())
    else {
        return Ok(true);
    };
    let part = reasoning_part(event, kind, part_field);
    // The normalized delta is one text stream, so match replay's newline between parts.
    let separator = match part {
        Some(part) => previous_part
            .replace(part)
            .is_some_and(|previous| previous != part),
        None => {
            *previous_part = None;
            false
        }
    };
    events(ModelEvent::ReasoningDelta(if separator {
        format!("\n{delta}")
    } else {
        delta.to_string()
    }))?;
    Ok(true)
}

fn reasoning_part(
    event: &Value,
    kind: ReasoningPartKind,
    part_field: &str,
) -> Option<ReasoningPart> {
    Some(ReasoningPart {
        kind,
        output_index: usize::try_from(event.get("output_index")?.as_u64()?).ok()?,
        part_index: usize::try_from(event.get(part_field)?.as_u64()?).ok()?,
    })
}

pub(super) fn emit_text_event(
    event: &Value,
    commentary: &mut BTreeSet<String>,
    events: &ModelEventSink,
) -> Result<bool> {
    match event.get("type").and_then(Value::as_str) {
        Some("response.output_item.added") => {
            let Some(item) = event.get("item").filter(|item| {
                item.get("type").and_then(Value::as_str) == Some("message")
                    && item.get("phase").and_then(Value::as_str) == Some("commentary")
            }) else {
                return Ok(false);
            };
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                return Ok(false);
            };
            commentary.insert(id.to_string());
            Ok(true)
        }
        Some("response.output_item.done") => Ok(event
            .get("item")
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .is_some_and(|id| commentary.remove(id))),
        Some("response.output_text.delta") => {
            let Some(delta) = event.get("delta").and_then(Value::as_str) else {
                return Ok(true);
            };
            let is_commentary = event
                .get("item_id")
                .and_then(Value::as_str)
                .is_some_and(|id| commentary.contains(id));
            if is_commentary {
                events(ModelEvent::CommentaryDelta(delta.to_string()))?;
            } else {
                events(ModelEvent::TextDelta(delta.to_string()))?;
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn decode_web_action(item: &Value) -> WebSearchAction {
    let Some(action) = item.get("action") else {
        return WebSearchAction::Other;
    };
    let string = |field| {
        action
            .get(field)
            .and_then(Value::as_str)
            .map(ToString::to_string)
    };
    match action.get("type").and_then(Value::as_str) {
        Some("search") => {
            let mut queries = action
                .get("queries")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|query| !query.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            if queries.is_empty()
                && let Some(query) = string("query").filter(|query| !query.is_empty())
            {
                queries.push(query);
            }
            if queries.is_empty() {
                WebSearchAction::Other
            } else {
                WebSearchAction::Search { queries }
            }
        }
        Some("open_page") => WebSearchAction::OpenPage { url: string("url") },
        Some("find_in_page") => WebSearchAction::FindInPage {
            url: string("url"),
            pattern: string("pattern"),
        },
        _ => WebSearchAction::Other,
    }
}

pub(super) fn decode_response(response: Value) -> Result<ModelOutput> {
    let end_turn = response
        .get("end_turn")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut output = response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| Error::Provider("response omitted output".into()))?;
    for item in &mut output {
        normalize_replay_item(item);
        if item.get("type").and_then(Value::as_str) != Some("reasoning") {
            continue;
        }
        let text = |field| {
            item.get(field)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let reasoning = text("summary");
        let reasoning = if reasoning.is_empty() {
            text("content")
        } else {
            reasoning
        };
        if !reasoning.is_empty() {
            item[super::REPLAY_REASONING_FIELD] = Value::String(reasoning);
        }
    }
    ModelOutput::from_output(output, end_turn, decode_usage(response.get("usage"))?)
}

pub(super) fn decode_compact_response(response: Value) -> Result<CompactOutput> {
    let mut output = response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for item in &mut output {
        normalize_replay_item(item);
    }
    CompactOutput::from_output(output, decode_usage(response.get("usage"))?)
}

fn normalize_replay_item(item: &mut Value) {
    if item.get("type").and_then(Value::as_str) == Some("compaction_summary")
        && let Some(fields) = item.as_object_mut()
    {
        fields.insert("type".into(), Value::String("compaction".into()));
    }
    strip_replay_wire_metadata(item);
}

fn strip_replay_wire_metadata(item: &mut Value) {
    let item_type = item.get("type").and_then(Value::as_str);
    let strip_format = item_type == Some("reasoning");
    let strip_status = matches!(item_type, Some("message" | "reasoning" | "function_call"));
    let Some(fields) = item.as_object_mut() else {
        return;
    };
    if strip_format {
        fields.remove("format");
    }
    if strip_status {
        fields.remove("status");
    }
}

fn required_string<'a>(item: &'a Value, field: &str) -> Result<&'a str> {
    item.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Provider(format!("function call omitted {field}").into()))
}

fn decode_usage(usage: Option<&Value>) -> Result<TokenUsage> {
    let value = |pointer| -> Result<i64> {
        Ok(usage_i64(usage, pointer, "Responses")?.unwrap_or_default())
    };
    Ok(TokenUsage {
        input_tokens: value("/input_tokens")?,
        cached_input_tokens: value("/input_tokens_details/cached_tokens")?,
        cache_write_input_tokens: value("/input_tokens_details/cache_write_tokens")?,
        output_tokens: value("/output_tokens")?,
        reasoning_output_tokens: value("/output_tokens_details/reasoning_tokens")?,
        total_tokens: value("/total_tokens")?,
    })
}

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;

pub(super) fn response_error(event: &Value) -> String {
    event
        .pointer("/response/error/message")
        .or_else(|| event.pointer("/error/message"))
        .or_else(|| event.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("response failed")
        .to_string()
}
