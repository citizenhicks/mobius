//! OpenAI Responses API Adapter.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use reqwest::Client;
use serde_json::Value;

use super::GeneratedImage;
use super::ImageGenerationRequest;
use super::Model;
use super::ModelEventSink;
#[cfg(test)]
use super::ModelInput;
use super::ModelOutput;
use super::ModelRequest;
use super::PromptCacheMode;
#[cfg(test)]
use super::TOOLS_SEARCH_NAME;
use super::ToolDefinition;
use super::image_generation::{IMAGE_APIS, ImageApi};
use super::openai_auth::ApiKeyAuthorization;
use super::openai_auth::OpenAiAuthorization;
#[cfg(test)]
use super::openai_auth::ResolvedAuthorization;
use super::provider::ProviderBuildConfig;
use super::provider::ProviderDefinition;
use super::provider::validate_base_url;
use super::realtime::RealtimeTransport;
use super::responses_wire::{ResponsesBody, WireInput, WireTools};
use super::transport::SseDecoder;
use super::transport::frame_data;
use super::transport::read_limited;
use super::transport::status_error;
use super::usage_i64;
use super::{RealtimeVoiceCall, RealtimeVoiceRequest};
use crate::BoxFuture;
use crate::Error;
use crate::ProviderError;
use crate::Result;
use crate::protocol::ModelEvent;
use crate::protocol::ModelInfo;
use crate::protocol::ModelStepAnnotation;
use crate::protocol::TokenUsage;
use crate::protocol::ToolDiscoveryMode;
#[cfg(test)]
use crate::protocol::ToolLoad;
use crate::protocol::WebSearchAction;

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("openai_provider.toml")));
const MAX_IMAGE_RESPONSE_BYTES: usize = 65 * 1024 * 1024;
const MAX_STREAM_OUTPUT_ITEMS: usize = 1_024;

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

pub(super) static CATALOG: std::sync::LazyLock<super::provider::ModelCatalog> =
    std::sync::LazyLock::new(|| {
        crate::config::overridable(
            "openai.toml",
            include_str!("openai.toml"),
            super::provider::ModelCatalog::validate,
        )
    });

/// OpenAI Responses API configuration.
pub struct OpenAi {
    client: Client,
    transport: super::ModelTransportSettings,
    auth: Option<Arc<dyn OpenAiAuthorization>>,
    realtime: Option<RealtimeTransport>,
    base_url: String,
    model: String,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    reasoning_summary: bool,
    hosted_tools: Vec<Value>,
    image_input: bool,
    image_api: Option<&'static ImageApi>,
    explicit_prompt_cache: bool,
    tool_discovery: ToolDiscoveryWire,
}

impl OpenAi {
    /// Creates an OpenAI or Responses-compatible provider.
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or a required resource cannot be initialized.
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self> {
        Self::new_with_transport(
            api_key,
            base_url,
            model,
            super::ModelTransportSettings::default(),
        )
    }

    /// Creates a provider with explicit HTTP, socket and retry policy.
    /// # Errors
    /// Returns invalid provider policy or HTTP client construction errors.
    pub fn new_with_transport(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        Self::with_client(
            Some(api_key.into()),
            base_url,
            model,
            settings.streaming_client()?,
            settings,
        )
    }

    pub(super) fn with_client(
        api_key: Option<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        client: Client,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        if api_key.as_deref().is_some_and(|key| key.trim().is_empty()) {
            return Err(Error::Config("provider API key cannot be empty".into()));
        }
        let auth = api_key
            .map(|key| Arc::new(ApiKeyAuthorization::new(key)) as Arc<dyn OpenAiAuthorization>);
        Self::from_parts(auth, base_url, model, client, settings)
    }

    pub(super) fn with_authorization(
        auth: Arc<dyn OpenAiAuthorization>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        client: Client,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        Self::from_parts(Some(auth), base_url, model, client, settings)
    }

    fn from_parts(
        auth: Option<Arc<dyn OpenAiAuthorization>>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        client: Client,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        settings.validate()?;
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let model = model.into();
        validate_base_url(&base_url)?;
        if model.trim().is_empty() {
            return Err(Error::Config("OPENAI_MODEL is empty".into()));
        }
        let realtime = auth
            .as_ref()
            .map(|auth| RealtimeTransport::new_openai(&base_url, Arc::clone(auth), settings))
            .transpose()?;
        let image_api = Some(&IMAGE_APIS["openai"]);
        Ok(Self {
            client,
            transport: settings,
            auth,
            realtime,
            base_url,
            model,
            reasoning_effort: None,
            service_tier: None,
            reasoning_summary: false,
            hosted_tools: Vec::new(),
            image_input: true,
            image_api,
            explicit_prompt_cache: false,
            tool_discovery: ToolDiscoveryWire::Rebuild,
        })
    }

    /// Sets validated socket, voice and retry policy for this model.
    /// # Errors
    /// Returns an error for invalid operational settings.
    pub fn with_transport_settings(
        mut self,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        settings.validate()?;
        if let Some(realtime) = self.realtime.take() {
            self.realtime = Some(realtime.with_settings(settings)?);
        }
        self.transport = settings;
        Ok(self)
    }

    pub(super) fn with_codex_realtime_voice(mut self, base_url: &str) -> Result<Self> {
        let auth = self
            .auth
            .as_ref()
            .ok_or_else(|| Error::Config("Codex voice requires authorization".into()))?;
        self.realtime = Some(RealtimeTransport::new_codex(
            base_url,
            Arc::clone(auth),
            self.transport,
        )?);
        Ok(self)
    }

    pub(super) fn without_realtime_voice(mut self) -> Self {
        self.realtime = None;
        self
    }

    pub(super) fn with_service_tier(mut self, service_tier: Option<String>) -> Self {
        self.service_tier = service_tier;
        self
    }

    pub(super) fn apply_service_tier(&self, body: &mut Value) {
        if let Some(tier) = &self.service_tier {
            body["service_tier"] = Value::String(tier.clone());
        }
    }

    pub(super) fn with_image_api(mut self, api: Option<&'static ImageApi>) -> Self {
        self.image_api = api;
        self
    }

    /// Selects a Responses reasoning effort.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Result<Self> {
        let effort = effort.into();
        if effort.trim().is_empty() {
            return Err(Error::Config("reasoning effort cannot be empty".into()));
        }
        self.reasoning_effort = Some(effort);
        Ok(self)
    }

    pub(super) fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
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
        media: Option<super::MediaPreparation<'_>>,
    ) -> Result<ModelOutput> {
        let session_id = request.session_id;
        let deferred_tools = request.deferred_tools;
        let body = super::media::encode_request(self, request, media, true, |input| {
            Ok(serde_json::to_vec(
                &self.response_body(ModelRequest { input, ..request })?,
            )?)
        })
        .await?;
        let template = self
            .client
            .post(format!("{}/responses", self.base_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        let mut response = self
            .send_authorized_with(true, Some(session_id), || {
                // A refreshed authorization retries the same immutable bytes, sharing reqwest's body buffer.
                template.try_clone().ok_or_else(|| {
                    Error::Provider("prepared Responses body cannot be retried".into())
                })
            })
            .await?;
        if !response.status().is_success() {
            return Err(status_error(response, "Responses").await);
        }

        let mut sse = SseDecoder::default();
        let mut commentary = BTreeSet::new();
        let mut reasoning_part = None;
        let mut web_searches = BTreeSet::new();
        let mut output = BTreeMap::new();
        let mut next_output_index = 0;
        while let Some(chunk) = response.chunk().await? {
            sse.push(&chunk, "Responses")?;
            while let Some(frame) = sse.next_frame()? {
                let Some(data) = frame_data(frame) else {
                    continue;
                };
                if data == "[DONE]" {
                    continue;
                }
                let mut event: Value = serde_json::from_str(&data)?;
                let pending = validate_stream_output(&event, &output)?;
                emit_ready_tool_calls(
                    &output,
                    pending.map(|index| (index, &event["item"])),
                    &mut next_output_index,
                    &events,
                )
                .await?;
                let handled = emit_web_event(&event, &mut web_searches, &events).await?
                    || emit_reasoning_event(&event, &mut reasoning_part, &events).await?
                    || emit_text_event(&event, &mut commentary, &events).await?;
                collect_stream_output(&mut event, &mut output, pending);
                if handled {
                    continue;
                }
                if let Some(response) =
                    self.finish_stream_event(event, &mut output, deferred_tools)?
                {
                    emit_citation_web_search(&response, &web_searches, &events).await?;
                    return Ok(response);
                }
            }
        }
        Err(Error::Provider(ProviderError::stream_interrupted(None)))
    }

    async fn send_image(&self, request: ImageGenerationRequest<'_>) -> Result<GeneratedImage> {
        request.validate(super::ImageInputLimits::default())?;
        let api = self.image_api.ok_or_else(|| {
            Error::Provider("image generation is unavailable for this provider".into())
        })?;
        let response = if api.uses_multipart(&request) {
            self.send_authorized_with(false, None, || {
                Ok(self
                    .client
                    .post(format!("{}/{}", self.base_url, api.edit_path))
                    .multipart(api.edit_form(&request)?))
            })
            .await?
        } else {
            let (endpoint, body) = api.wire(&request)?;
            self.send_authorized(endpoint, &body, false, None).await?
        };
        if !response.status().is_success() {
            return Err(status_error(response, "Images").await);
        }
        api.decode(&read_limited(response, MAX_IMAGE_RESPONSE_BYTES, "Images").await?)
    }

    fn finish_stream_event(
        &self,
        mut event: Value,
        output: &mut BTreeMap<u64, Value>,
        deferred_tools: &[Arc<ToolDefinition>],
    ) -> Result<Option<ModelOutput>> {
        match event.get("type").and_then(Value::as_str) {
            Some("response.completed") => {
                let response = event
                    .get_mut("response")
                    .map(Value::take)
                    .map(|response| attach_stream_output(response, std::mem::take(output)))
                    .ok_or_else(|| Error::Provider("completion omitted response".into()))?;
                self.decode_response(response, deferred_tools).map(Some)
            }
            Some("error" | "response.failed" | "response.incomplete") => {
                Err(Error::Provider(response_provider_error(&event, None)))
            }
            _ => Ok(None),
        }
    }

    fn response_body<'a>(&'a self, request: ModelRequest<'a>) -> Result<ResponsesBody<'a>> {
        let additional_tools = match self.tool_discovery {
            ToolDiscoveryWire::AdditionalTools => request.deferred_tools,
            ToolDiscoveryWire::Rebuild | ToolDiscoveryWire::OpenRouter => &[],
        };
        let mut body = serde_json::json!({
            "model": self.model,
            "instructions": request.instructions,
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
        self.apply_service_tier(&mut body);
        Ok(ResponsesBody {
            metadata: body,
            tools: self.wire_request_tools(&request),
            input: WireInput::new(
                request.input,
                self.image_input,
                self.explicit_prompt_cache,
                request.catalog_revision,
                additional_tools,
            )?,
        })
    }

    #[cfg(test)]
    fn response_body_value(&self, request: ModelRequest<'_>) -> Result<Value> {
        Ok(serde_json::to_value(self.response_body(request)?)?)
    }

    fn wire_request_tools<'a>(&'a self, request: &ModelRequest<'a>) -> WireTools<'a> {
        let openrouter = self.tool_discovery == ToolDiscoveryWire::OpenRouter;
        WireTools {
            functions: request.tools,
            deferred: if openrouter {
                request.deferred_tools
            } else {
                &[]
            },
            hosted: if request.allow_hosted_tools {
                &self.hosted_tools
            } else {
                &[]
            },
            openrouter,
        }
    }

    fn decode_response(
        &self,
        response: Value,
        deferred_tools: &[Arc<ToolDefinition>],
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

    async fn send_authorized(
        &self,
        endpoint: &str,
        body: &(impl serde::Serialize + Sync),
        streaming: bool,
        session_id: Option<&str>,
    ) -> Result<reqwest::Response> {
        self.send_authorized_with(streaming, session_id, || {
            Ok(self
                .client
                .post(format!("{}/{endpoint}", self.base_url))
                .json(body))
        })
        .await
    }

    async fn send_authorized_with(
        &self,
        streaming: bool,
        session_id: Option<&str>,
        mut body: impl FnMut() -> Result<reqwest::RequestBuilder>,
    ) -> Result<reqwest::Response> {
        for attempt in 0..2 {
            let mut request = body()?;
            let Some(auth) = &self.auth else {
                return Ok(request.send().await?);
            };
            let authorization = auth.authorize_http(streaming, session_id).await?;
            request = request.bearer_auth(&authorization.token);
            for (name, value) in authorization.headers {
                request = request.header(name, value.as_ref());
            }
            let response = request.send().await?;
            if response.status() != reqwest::StatusCode::UNAUTHORIZED || attempt == 1 {
                return Ok(response);
            }
            if !auth.recover_unauthorized(&authorization.token).await? {
                return Ok(response);
            }
        }
        unreachable!("authorized request retry is bounded")
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
        self.image_input
    }

    fn supports_image_generation(&self) -> bool {
        self.image_api.is_some()
    }

    fn generate_image<'a>(
        &'a self,
        request: ImageGenerationRequest<'a>,
    ) -> BoxFuture<'a, Result<GeneratedImage>> {
        Box::pin(self.send_image(request))
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

    fn respond_prepared<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
        media: super::MediaPreparation<'a>,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(self.send_response(request, events, Some(media)))
    }

    fn request_size(&self, request: ModelRequest<'_>) -> Result<usize> {
        super::media::serialized_size(&self.response_body(request)?)
    }

    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(self.send_response(request, events, None))
    }
}

#[cfg(test)]
pub(super) fn wire_input_with_cache(
    input: ModelInput<'_>,
    allow_images: bool,
    explicit_prompt_cache: bool,
    catalog_revision: &str,
    additional_tools: &[Arc<ToolDefinition>],
) -> Result<Vec<Value>> {
    let input = WireInput::new(
        input,
        allow_images,
        explicit_prompt_cache,
        catalog_revision,
        additional_tools,
    )?;
    match serde_json::to_value(input)? {
        Value::Array(items) => Ok(items),
        _ => Err(Error::Provider(
            "Responses wire input must be an array".into(),
        )),
    }
}

pub(super) fn validate_stream_output(
    event: &Value,
    output: &BTreeMap<u64, Value>,
) -> Result<Option<u64>> {
    if event.get("type").and_then(Value::as_str) != Some("response.output_item.done") {
        return Ok(None);
    }
    event
        .get("item")
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
    if output.contains_key(&index) {
        return Err(Error::Provider(
            format!("response repeated output item index {index}").into(),
        ));
    }
    Ok(Some(index))
}

pub(super) fn collect_stream_output(
    event: &mut Value,
    output: &mut BTreeMap<u64, Value>,
    index: Option<u64>,
) {
    if let Some(index) = index {
        output.insert(index, event["item"].take());
    }
}

pub(super) async fn emit_ready_tool_calls(
    output: &BTreeMap<u64, Value>,
    pending: Option<(u64, &Value)>,
    next_output_index: &mut u64,
    events: &ModelEventSink,
) -> Result<()> {
    // A newly completed item can release buffered calls before its borrowed event handlers run.
    while let Some(item) = output.get(next_output_index).or_else(|| {
        pending
            .filter(|(index, _)| *index == *next_output_index)
            .map(|(_, item)| item)
    }) {
        if item.get("type").and_then(Value::as_str) == Some("function_call") {
            let call = super::decode_tool_call(item)?;
            events(ModelEvent::ToolCallReady(call)).await?;
        }
        *next_output_index = next_output_index
            .checked_add(1)
            .ok_or_else(|| Error::Provider("Responses output index overflowed".into()))?;
    }
    Ok(())
}

pub(super) fn attach_stream_output(mut response: Value, output: BTreeMap<u64, Value>) -> Value {
    // Completed output owns its final annotations; item snapshots only fill omitted output.
    let needs_stream_output = response.is_object()
        && response
            .get("output")
            .is_none_or(|value| matches!(value, Value::Array(items) if items.is_empty()));
    if needs_stream_output && !output.is_empty() {
        response["output"] = Value::Array(output.into_values().collect());
    }
    response
}

pub(super) fn wire_tools<'a>(
    tools: &'a [Arc<ToolDefinition>],
    hosted_tools: &'a [Value],
    allow_hosted_tools: bool,
) -> WireTools<'a> {
    WireTools {
        functions: tools,
        deferred: &[],
        hosted: if allow_hosted_tools {
            hosted_tools
        } else {
            &[]
        },
        openrouter: false,
    }
}

pub(super) fn generic_provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        "responses",
        &MANIFEST,
        MANIFEST.api_key_auth(),
        None,
        build_generic,
    )
    .with_image_input()
    .with_image_generation()
    .with_realtime_voices(&super::realtime::VOICES, &super::realtime::OPENAI_MODELS)
    .with_credentialless_endpoints()
}

fn build_generic(config: ProviderBuildConfig) -> Result<std::sync::Arc<dyn Model>> {
    let tool_discovery = config.tool_discovery.unwrap_or_else(|| {
        generic_provider().tool_discovery(&config.model, config.base_url.as_deref())
    });
    let base_url = config
        .base_url
        .ok_or_else(|| Error::Config("Responses provider requires a base URL".into()))?;
    let api_key = config.credential.into_optional_api_key("responses")?;
    let native_voice = config.capability == Some(crate::protocol::ModelCapability::RealtimeVoice)
        || generic_provider().supports_at(
            crate::protocol::ModelCapability::RealtimeVoice,
            Some(&base_url),
        );
    let provider = OpenAi::with_client(
        api_key,
        base_url,
        config.model,
        config.http,
        config.transport,
    )?
    .with_tool_discovery(tool_discovery)
    .with_service_tier(config.service_tier);
    let provider = if native_voice {
        provider
    } else {
        provider.without_realtime_voice()
    };
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

async fn emit_citation_web_search(
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
    })
    .await?;
    events(ModelEvent::WebSearchCompleted {
        call_id,
        action: WebSearchAction::Other,
    })
    .await
}

pub(super) async fn emit_web_event(
    event: &Value,
    seen: &mut BTreeSet<String>,
    events: &ModelEventSink,
) -> Result<bool> {
    let Some(item) = web_search_item(event) else {
        return Ok(false);
    };
    let call_id = required_string(item, "id")?;
    if !seen.contains(call_id) {
        seen.insert(call_id.to_string());
        events(ModelEvent::WebSearchStarted {
            call_id: call_id.to_string(),
        })
        .await?;
    }
    if event.get("type").and_then(Value::as_str) == Some("response.output_item.done") {
        events(ModelEvent::WebSearchCompleted {
            call_id: call_id.to_string(),
            action: decode_web_action(item),
        })
        .await?;
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

pub(super) async fn emit_reasoning_event(
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
    }))
    .await?;
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

pub(super) async fn emit_text_event(
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
                events(ModelEvent::CommentaryDelta(delta.to_string())).await?;
            } else {
                events(ModelEvent::TextDelta(delta.to_string())).await?;
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

pub(super) fn decode_response(mut response: Value) -> Result<ModelOutput> {
    let end_turn = response
        .get("end_turn")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut output = response
        .get_mut("output")
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .ok_or_else(|| Error::Provider("response omitted output".into()))?;
    for item in &mut output {
        strip_replay_wire_metadata(item);
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

pub(super) fn response_provider_error(event: &Value, retry_after: Option<String>) -> ProviderError {
    let message = response_error(event);
    let error = match event
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .filter(|status| (400..=599).contains(status))
    {
        Some(status) => ProviderError::http(message, status, retry_after),
        None => ProviderError::new(message),
    };
    error.with_code(super::transport::response_error_code(event))
}
