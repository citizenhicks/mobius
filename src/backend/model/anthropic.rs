//! Native Anthropic Messages API provider.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use super::transport::ApiKeyModel;
use reqwest::Client;
use serde::ser::SerializeSeq as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Model;
use super::ModelEventSink;
use super::ModelInput;
use super::ModelOutput;
use super::ModelRequest;
use super::PROMPT_CACHE_BREAKPOINT_FIELD;
use super::PromptCacheMode;
use super::REPLAY_REASONING_FIELD;
use super::TOOL_ERROR_FIELD;
use super::TOOLS_SEARCH_NAME;
use super::ToolDefinition;
use super::image_input;
use super::provider::HostedWebSearch;
use super::provider::ProviderBuildConfig;
use super::provider::ProviderDefinition;
use super::transport::status_error;
use super::transport::streaming_client;
use super::usage_i64;
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::protocol::ModelEvent;
use crate::protocol::ModelInfo;
use crate::protocol::ModelStepAnnotation;
use crate::protocol::ModelStepContent;
use crate::protocol::ModelStepContentPhase;
use crate::protocol::TokenUsage;
use crate::protocol::ToolDiscoveryMode;
use crate::protocol::ToolLoad;
use crate::protocol::WebSearchAction;

pub(super) static MANIFEST: std::sync::LazyLock<super::provider::ProviderMetadata> =
    std::sync::LazyLock::new(|| crate::config::embedded(include_str!("anthropic_provider.toml")));
pub(super) static CATALOG: std::sync::LazyLock<super::provider::ModelCatalog> =
    std::sync::LazyLock::new(|| {
        crate::config::overridable(
            "anthropic.toml",
            include_str!("anthropic.toml"),
            super::provider::ModelCatalog::validate,
        )
    });

const MAX_CONTENT_BLOCKS: usize = 1_024;
const RAW_CONTENT: &str = "_anthropic_content";

/// Anthropic's native Messages API provider.
#[derive(Debug)]
pub struct Anthropic {
    config: ApiKeyModel,
    max_output_tokens: u64,
    tool_discovery: ToolDiscoveryMode,
    web_search: bool,
}

#[derive(Serialize)]
struct RequestBody<'a> {
    #[serde(flatten)]
    metadata: Value,
    tools: WireTools<'a>,
    messages: Vec<WireMessage<'a>>,
}

impl Anthropic {
    /// Creates a provider for an Anthropic Messages API endpoint.
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or a required resource cannot be initialized.
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self> {
        Self::with_client(Some(api_key.into()), base_url, model, streaming_client()?)
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
        )?
        .with_transport_settings(settings)
    }

    fn with_client(
        api_key: Option<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        client: Client,
    ) -> Result<Self> {
        let config = ApiKeyModel::new(api_key, base_url, model, client)?;
        let tool_discovery = provider().tool_discovery(&config.model, Some(&config.base_url));
        Ok(Self {
            config,
            max_output_tokens: MANIFEST
                .max_output_tokens
                .expect("Anthropic output default is required"),
            tool_discovery,
            web_search: false,
        })
    }

    /// Applies validated operational transport policy.
    /// # Errors
    /// Returns an error for invalid operational settings.
    pub fn with_transport_settings(
        mut self,
        settings: super::ModelTransportSettings,
    ) -> Result<Self> {
        settings.validate()?;
        self.config.transport = settings;
        Ok(self)
    }

    /// Chooses the output-token budget independently from the advertised model context.
    /// # Errors
    /// Returns an error for a zero budget or a budget above the transport safety bound.
    pub fn with_max_output_tokens(mut self, tokens: u64) -> Result<Self> {
        if tokens == 0 || tokens > 1_048_576 {
            return Err(Error::Config(
                "Anthropic output token budget must be in 1..=1048576".into(),
            ));
        }
        self.max_output_tokens = tokens;
        Ok(self)
    }

    /// Enables adaptive thinking at the configured Anthropic effort level.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Result<Self> {
        self.config.set_reasoning(effort)?;
        Ok(self)
    }

    /// Enables Anthropic-hosted web search.
    #[must_use]
    pub fn with_web_search(mut self) -> Self {
        self.web_search = true;
        self
    }

    async fn send_response(
        &self,
        request: ModelRequest<'_>,
        events: ModelEventSink,
        media: Option<super::MediaPreparation<'_>>,
    ) -> Result<ModelOutput> {
        let body = super::media::encode_request(self, request, media, true, |input| {
            Ok(serde_json::to_vec(&self.request_body(
                request.instructions,
                input,
                request.catalog_revision,
                request.tools,
                request.deferred_tools,
                request.allow_hosted_tools,
            )?)?)
        })
        .await?;
        let response = self.post(body).await?;
        let mut stream = StreamState::default();
        super::transport::read_sse(response, "Anthropic", &events, &mut stream).await?;
        if !stream.stopped {
            return Err(Error::Provider(
                "Anthropic stream ended before message_stop".into(),
            ));
        }
        stream.finish()
    }

    fn request_body<'a>(
        &self,
        instructions: &str,
        input: ModelInput<'a>,
        catalog_revision: &str,
        tools: &'a [Arc<ToolDefinition>],
        deferred_tools: &'a [Arc<ToolDefinition>],
        allow_hosted_tools: bool,
    ) -> Result<RequestBody<'a>> {
        let discovery = self.tool_discovery();
        let mut body = serde_json::json!({
            "model": self.config.model,
            "max_tokens": self.max_output_tokens,
            "system": instructions,
            "stream": true
        });
        let messages = translate_messages(input, discovery, catalog_revision, deferred_tools)?;
        self.apply_reasoning(&mut body);
        Ok(RequestBody {
            metadata: body,
            messages,
            tools: wire_tools(
                tools,
                if discovery == ToolDiscoveryMode::Native {
                    deferred_tools
                } else {
                    &[]
                },
                self.web_search && allow_hosted_tools,
            ),
        })
    }

    #[cfg(test)]
    fn request_body_value(
        &self,
        instructions: &str,
        input: ModelInput<'_>,
        catalog_revision: &str,
        tools: &[Arc<ToolDefinition>],
        deferred_tools: &[Arc<ToolDefinition>],
        allow_hosted_tools: bool,
    ) -> Result<Value> {
        Ok(serde_json::to_value(self.request_body(
            instructions,
            input,
            catalog_revision,
            tools,
            deferred_tools,
            allow_hosted_tools,
        )?)?)
    }

    fn apply_reasoning(&self, body: &mut Value) {
        if let Some(effort) = &self.config.reasoning_effort {
            body["thinking"] = serde_json::json!({"type": "adaptive"});
            body["output_config"] = serde_json::json!({"effort": effort});
        }
    }

    async fn post(&self, body: Vec<u8>) -> Result<reqwest::Response> {
        let mut request = self
            .config
            .client
            .post(format!("{}/messages", self.config.base_url));
        for (name, value) in &MANIFEST.headers {
            request = request.header(name, value);
        }
        if let Some(api_key) = &self.config.api_key {
            request = request.header("x-api-key", api_key);
        }
        let response = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(status_error(response, "Anthropic").await)
        }
    }
}

impl Model for Anthropic {
    fn transport_settings(&self) -> super::ModelTransportSettings {
        self.config.transport
    }

    fn info(&self) -> ModelInfo {
        self.config.info()
    }

    fn supports_tool_image_input(&self) -> bool {
        self.supports_image_input()
    }

    fn supports_image_input(&self) -> bool {
        true
    }

    fn prompt_cache_capability(&self) -> PromptCacheMode {
        PromptCacheMode::Explicit
    }

    fn tool_discovery(&self) -> ToolDiscoveryMode {
        self.tool_discovery
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
        super::media::serialized_size(&self.request_body(
            request.instructions,
            request.input,
            request.catalog_revision,
            request.tools,
            request.deferred_tools,
            request.allow_hosted_tools,
        )?)
    }

    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(self.send_response(request, events, None))
    }
}

#[derive(Default)]
struct StreamState {
    blocks: BTreeMap<usize, Value>,
    partial_json: BTreeMap<usize, String>,
    completed_blocks: BTreeSet<usize>,
    next_completed_block: usize,
    web_queries: BTreeMap<String, Option<String>>,
    usage: Usage,
    stop_reason: Option<String>,
    stopped: bool,
}

impl super::transport::SseHandler for StreamState {
    async fn apply_data(&mut self, data: &str, events: &ModelEventSink) -> Result<()> {
        self.apply(serde_json::from_str(data)?, events).await
    }
}

impl StreamState {
    async fn apply(&mut self, event: Value, events: &ModelEventSink) -> Result<()> {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => self.usage.update(event.pointer("/message/usage"))?,
            Some("content_block_start") => self.start_block(event, events).await?,
            Some("content_block_delta") => self.delta_block(event, events).await?,
            Some("content_block_stop") => self.stop_block(&event, events).await?,
            Some("message_delta") => {
                self.usage.update(event.get("usage"))?;
                self.stop_reason = event
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
            }
            Some("message_stop") => self.stopped = true,
            Some("error") => {
                let message = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Anthropic stream error");
                return Err(Error::Provider(message.to_string().into()));
            }
            Some("ping") | None | Some(_) => {}
        }
        Ok(())
    }

    async fn start_block(&mut self, mut event: Value, events: &ModelEventSink) -> Result<()> {
        let index = event_index(&event)?;
        if self.blocks.contains_key(&index) {
            return Err(Error::Provider(
                format!("Anthropic repeated content block index {index}").into(),
            ));
        }
        if self.blocks.len() >= MAX_CONTENT_BLOCKS {
            return Err(Error::Provider(
                format!("Anthropic returned more than {MAX_CONTENT_BLOCKS} content blocks").into(),
            ));
        }
        let block = event
            .get_mut("content_block")
            .map(Value::take)
            .ok_or_else(|| Error::Provider("Anthropic content block omitted value".into()))?;
        if block.get("type").and_then(Value::as_str) == Some("server_tool_use")
            && block.get("name").and_then(Value::as_str) == Some("web_search")
        {
            let id = required_string(&block, "id")?.to_string();
            let query = block
                .pointer("/input/query")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            self.web_queries.insert(id.clone(), query);
            events(ModelEvent::WebSearchStarted { call_id: id }).await?;
        }
        if block.get("type").and_then(Value::as_str) == Some("web_search_tool_result") {
            let call_id = required_string(&block, "tool_use_id")?.to_string();
            let action = self
                .web_queries
                .get(&call_id)
                .cloned()
                .flatten()
                .filter(|query| !query.is_empty())
                .map_or(WebSearchAction::Other, |query| WebSearchAction::Search {
                    queries: vec![query],
                });
            events(ModelEvent::WebSearchCompleted { call_id, action }).await?;
        }
        self.blocks.insert(index, block);
        Ok(())
    }

    async fn delta_block(&mut self, mut event: Value, events: &ModelEventSink) -> Result<()> {
        let index = event_index(&event)?;
        if self.completed_blocks.contains(&index) {
            return Err(Error::Provider(
                format!("Anthropic delta followed completed content block index {index}").into(),
            ));
        }
        let delta = event
            .get_mut("delta")
            .ok_or_else(|| Error::Provider("Anthropic content delta omitted value".into()))?;
        let block = self
            .blocks
            .get_mut(&index)
            .ok_or_else(|| Error::Provider("Anthropic delta referenced unknown block".into()))?;
        // Omitted thinking and initial tool-input fragments legitimately carry empty strings.
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                let text = string_field(delta, "text")?;
                if !text.is_empty() {
                    append_string(block, "text", text);
                    events(ModelEvent::TextDelta(text.to_string())).await?;
                }
            }
            Some("thinking_delta") => {
                let thinking = string_field(delta, "thinking")?;
                if !thinking.is_empty() {
                    append_string(block, "thinking", thinking);
                    events(ModelEvent::ReasoningDelta(thinking.to_string())).await?;
                }
            }
            Some("signature_delta") => {
                block["signature"] =
                    Value::String(required_string(delta, "signature")?.to_string());
            }
            Some("input_json_delta") => {
                let partial = string_field(delta, "partial_json")?;
                if !partial.is_empty() {
                    self.partial_json
                        .entry(index)
                        .or_default()
                        .push_str(partial);
                }
            }
            Some("citations_delta") => {
                if let Some(citation) = delta.get_mut("citation") {
                    let citations = block
                        .as_object_mut()
                        .ok_or_else(|| {
                            Error::Provider("Anthropic content block was not an object".into())
                        })?
                        .entry("citations")
                        .or_insert_with(|| Value::Array(Vec::new()));
                    citations
                        .as_array_mut()
                        .ok_or_else(|| {
                            Error::Provider("Anthropic citations were not an array".into())
                        })?
                        .push(citation.take());
                }
            }
            None | Some(_) => {}
        }
        Ok(())
    }

    async fn stop_block(&mut self, event: &Value, events: &ModelEventSink) -> Result<()> {
        let index = event_index(event)?;
        if self.completed_blocks.contains(&index) {
            return Err(Error::Provider(
                format!("Anthropic repeated content block stop index {index}").into(),
            ));
        }
        if let Some(partial) = self.partial_json.remove(&index) {
            let input: Value = serde_json::from_str(&partial)?;
            let block = self
                .blocks
                .get_mut(&index)
                .ok_or_else(|| Error::Provider("Anthropic stop referenced unknown block".into()))?;
            block["input"] = input;
            if block.get("type").and_then(Value::as_str) == Some("server_tool_use")
                && block.get("name").and_then(Value::as_str) == Some("web_search")
            {
                let id = required_string(block, "id")?.to_string();
                let query = block
                    .pointer("/input/query")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                self.web_queries.insert(id, query);
            }
        }
        self.completed_blocks.insert(index);
        self.emit_ready_tool_calls(events).await
    }

    async fn emit_ready_tool_calls(&mut self, events: &ModelEventSink) -> Result<()> {
        while self.completed_blocks.contains(&self.next_completed_block) {
            let index = self.next_completed_block;
            self.next_completed_block = self
                .next_completed_block
                .checked_add(1)
                .ok_or_else(|| Error::Provider("Anthropic block index overflowed".into()))?;
            let Some(block) = self.blocks.get(&index) else {
                return Err(Error::Provider(
                    "Anthropic completed block was not started".into(),
                ));
            };
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let empty = serde_json::json!({});
            let input = block.get("input").unwrap_or(&empty);
            let item = serde_json::json!({
                "type": "function_call",
                "call_id": required_string(block, "id")?,
                "name": required_string(block, "name")?,
                "arguments": serde_json::to_string(&input)?
            });
            let call = super::decode_tool_call(&item)?;
            events(ModelEvent::ToolCallReady(call)).await?;
        }
        Ok(())
    }

    fn finish(self) -> Result<ModelOutput> {
        let step_content = normalized_step_content(&self.blocks)?;
        if self.stop_reason.as_deref() == Some("refusal")
            && !step_content.iter().any(|part| {
                part.phase == ModelStepContentPhase::FinalAnswer && !part.text.trim().is_empty()
            })
        {
            return Err(Error::Provider(
                "Anthropic refused the request without an answer".into(),
            ));
        }
        let content = self.blocks.into_values().collect::<Vec<_>>();
        let calls = content
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
            .map(|block| {
                let empty = serde_json::json!({});
                let arguments = block.get("input").unwrap_or(&empty);
                Ok(serde_json::json!({
                    "type": "function_call",
                    "call_id": required_string(block, "id")?,
                    "name": required_string(block, "name")?,
                    "arguments": serde_json::to_string(&arguments)?
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        let visible = content
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .map(|block| {
                serde_json::json!({
                    "type": "output_text",
                    "text": block.get("text").and_then(Value::as_str).unwrap_or_default()
                })
            })
            .collect::<Vec<_>>();
        let mut message = serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": visible
        });
        let reasoning = content
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("thinking"))
            .filter_map(|block| block.get("thinking").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !reasoning.is_empty() {
            message[REPLAY_REASONING_FIELD] = Value::String(reasoning);
        }
        message[RAW_CONTENT] = Value::Array(content);
        let mut output = vec![message];
        output.extend(calls);
        ModelOutput::from_output_with_content(
            output,
            self.stop_reason.as_deref() != Some("pause_turn"),
            self.usage.finish()?,
            step_content,
        )
    }
}

fn normalized_step_content(blocks: &BTreeMap<usize, Value>) -> Result<Vec<ModelStepContent>> {
    let mut content = Vec::new();
    for (&part_index, block) in blocks {
        let (phase, text, annotations) = match block.get("type").and_then(Value::as_str) {
            Some("thinking") => (
                ModelStepContentPhase::Reasoning,
                block.get("thinking").and_then(Value::as_str),
                Vec::new(),
            ),
            Some("text") => (
                ModelStepContentPhase::FinalAnswer,
                block.get("text").and_then(Value::as_str),
                normalize_citations(block)?,
            ),
            None | Some(_) => continue,
        };
        let Some(text) = text.filter(|text| !text.is_empty()) else {
            continue;
        };
        content.push(ModelStepContent {
            output_index: 0,
            part_index,
            phase,
            text: text.into(),
            annotations,
        });
    }
    Ok(content)
}

fn normalize_citations(block: &Value) -> Result<Vec<ModelStepAnnotation>> {
    let Some(citations) = block.get("citations") else {
        return Ok(Vec::new());
    };
    if citations.is_null() {
        return Ok(Vec::new());
    }
    let citations: Vec<AnthropicCitation> = serde::Deserialize::deserialize(citations)
        .map_err(|error| Error::Provider(format!("invalid Anthropic citation: {error}").into()))?;
    Ok(citations.into_iter().map(Into::into).collect())
}

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum AnthropicCitation {
    #[serde(rename = "char_location")]
    Character {
        cited_text: String,
        document_index: usize,
        document_title: Option<String>,
        file_id: Option<String>,
        start_char_index: usize,
        end_char_index: usize,
    },
    #[serde(rename = "page_location")]
    Page {
        cited_text: String,
        document_index: usize,
        document_title: Option<String>,
        file_id: Option<String>,
        start_page_number: usize,
        end_page_number: usize,
    },
    #[serde(rename = "content_block_location")]
    ContentBlock {
        cited_text: String,
        document_index: usize,
        document_title: Option<String>,
        file_id: Option<String>,
        start_block_index: usize,
        end_block_index: usize,
    },
    #[serde(rename = "search_result_location")]
    SearchResult {
        cited_text: String,
        search_result_index: usize,
        source: String,
        title: Option<String>,
        start_block_index: usize,
        end_block_index: usize,
    },
    #[serde(rename = "web_search_result_location")]
    WebSearchResult {
        cited_text: String,
        encrypted_index: String,
        title: Option<String>,
        url: String,
    },
}

impl From<AnthropicCitation> for ModelStepAnnotation {
    fn from(citation: AnthropicCitation) -> Self {
        match citation {
            AnthropicCitation::Character {
                cited_text,
                document_index,
                document_title,
                file_id,
                start_char_index,
                end_char_index,
            } => Self::DocumentCharacterCitation {
                cited_text,
                document_index,
                document_title,
                file_id,
                start_char_index,
                end_char_index,
            },
            AnthropicCitation::Page {
                cited_text,
                document_index,
                document_title,
                file_id,
                start_page_number,
                end_page_number,
            } => Self::DocumentPageCitation {
                cited_text,
                document_index,
                document_title,
                file_id,
                start_page_number,
                end_page_number,
            },
            AnthropicCitation::ContentBlock {
                cited_text,
                document_index,
                document_title,
                file_id,
                start_block_index,
                end_block_index,
            } => Self::DocumentContentBlockCitation {
                cited_text,
                document_index,
                document_title,
                file_id,
                start_block_index,
                end_block_index,
            },
            AnthropicCitation::SearchResult {
                cited_text,
                search_result_index,
                source,
                title,
                start_block_index,
                end_block_index,
            } => Self::SearchResultCitation {
                cited_text,
                search_result_index,
                source,
                title,
                start_block_index,
                end_block_index,
            },
            AnthropicCitation::WebSearchResult {
                cited_text,
                encrypted_index,
                title,
                url,
            } => Self::WebSearchResultCitation {
                cited_text,
                encrypted_index,
                title,
                url,
            },
        }
    }
}

#[derive(Default)]
struct Usage {
    input: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
    thinking: i64,
}

impl Usage {
    fn update(&mut self, usage: Option<&Value>) -> Result<()> {
        let Some(usage) = usage else {
            return Ok(());
        };
        update_i64(&mut self.input, usage, "/input_tokens")?;
        update_i64(&mut self.cache_read, usage, "/cache_read_input_tokens")?;
        update_i64(&mut self.cache_write, usage, "/cache_creation_input_tokens")?;
        update_i64(&mut self.output, usage, "/output_tokens")?;
        update_i64(
            &mut self.thinking,
            usage,
            "/output_tokens_details/thinking_tokens",
        )?;
        Ok(())
    }

    fn finish(self) -> Result<TokenUsage> {
        let input_tokens = self
            .input
            .checked_add(self.cache_read)
            .and_then(|tokens| tokens.checked_add(self.cache_write))
            .ok_or_else(|| Error::Provider("Anthropic token usage overflowed".into()))?;
        let total_tokens = input_tokens
            .checked_add(self.output)
            .ok_or_else(|| Error::Provider("Anthropic token usage overflowed".into()))?;
        Ok(TokenUsage {
            input_tokens,
            cached_input_tokens: self.cache_read,
            cache_write_input_tokens: self.cache_write,
            output_tokens: self.output,
            reasoning_output_tokens: self.thinking,
            total_tokens,
        })
    }
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: Vec<Cow<'a, Value>>,
}

fn translate_messages<'a>(
    input: ModelInput<'a>,
    discovery: ToolDiscoveryMode,
    catalog_revision: &str,
    deferred_tools: &[Arc<ToolDefinition>],
) -> Result<Vec<WireMessage<'a>>> {
    let mut messages = Vec::new();
    let mut preserved_tools = BTreeSet::new();
    let mut search_calls = BTreeSet::new();
    let deferred_tool_names = deferred_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<BTreeSet<_>>();
    for (index, item) in input.iter().enumerate() {
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .or_else(|| item.get("role").is_some().then_some("message"));
        match kind {
            Some("message") => {
                let role = item
                    .get("role")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Provider("history message omitted role".into()))?;
                if let Some(content) = item.get(RAW_CONTENT).and_then(Value::as_array) {
                    preserved_tools.extend(
                        content
                            .iter()
                            .filter(|block| {
                                block.get("type").and_then(Value::as_str) == Some("tool_use")
                            })
                            .filter_map(|block| block.get("id").and_then(Value::as_str))
                            .map(ToString::to_string),
                    );
                    push_message(&mut messages, role, content.iter().map(Cow::Borrowed));
                } else {
                    let blocks = item
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(content_block)
                        .filter_map(Result::transpose)
                        .collect::<Result<Vec<_>>>()?;
                    push_message(&mut messages, role, blocks.into_iter().map(Cow::Owned));
                }
            }
            Some("function_call") => {
                let call_id = required_string(item, "call_id")?;
                let name = required_string(item, "name")?;
                remember_search_call(&mut search_calls, call_id, name);
                if !preserved_tools.contains(call_id) {
                    push_message(
                        &mut messages,
                        "assistant",
                        [Cow::Owned(serde_json::json!({
                            "type": "tool_use",
                            "id": call_id,
                            "name": name,
                            "input": serde_json::from_str::<Value>(required_string(item, "arguments")?)?
                        }))],
                    );
                }
            }
            Some("function_call_output") => push_message(
                &mut messages,
                "user",
                [Cow::Owned(tool_result_block(
                    item,
                    input.get(index + 1),
                    discovery,
                    catalog_revision,
                    &search_calls,
                    &deferred_tool_names,
                )?)],
            ),
            Some("tool_load") => replay_standalone_tool_load(
                &mut messages,
                ToolLoad::from_input(item)?,
                follows_search_result(input.get(index.saturating_sub(1)), &search_calls),
                index,
                discovery,
                catalog_revision,
                &deferred_tool_names,
            ),
            None | Some(_) => {}
        }
    }
    if messages.is_empty() {
        return Err(Error::Provider("Anthropic request has no messages".into()));
    }
    mark_latest_cache_endpoint(&mut messages);
    Ok(messages)
}

/// Advances the explicit cache endpoint to the newest cacheable block, so every request
/// writes its whole prefix and the previous request's endpoint stays readable.
fn mark_latest_cache_endpoint(messages: &mut [WireMessage<'_>]) {
    if let Some(block) = messages
        .last_mut()
        .map(|message| &mut message.content)
        .and_then(|content| {
            content
                .iter_mut()
                .rev()
                .find(|block| accepts_cache_control(block))
        })
    {
        block.to_mut()["cache_control"] = serde_json::json!({"type": "ephemeral"});
    }
}

/// Anthropic rejects cache markers on thinking blocks and empty text blocks.
fn accepts_cache_control(block: &Value) -> bool {
    !matches!(
        block.get("type").and_then(Value::as_str),
        Some("thinking" | "redacted_thinking")
    ) && block.get("text").and_then(Value::as_str) != Some("")
}

fn remember_search_call(search_calls: &mut BTreeSet<String>, call_id: &str, name: &str) {
    if name == TOOLS_SEARCH_NAME {
        search_calls.insert(call_id.to_string());
    }
}

fn content_block(part: &Value) -> Result<Option<Value>> {
    let mut block = match part.get("type").and_then(Value::as_str) {
        Some("input_text" | "output_text") => {
            serde_json::json!({"type":"text", "text": required_string(part, "text")?})
        }
        Some("input_image") => {
            let Some((media_type, data)) = image_input(part, "Anthropic")? else {
                return Ok(None);
            };
            serde_json::json!({"type":"image", "source":{"type":"base64", "media_type":media_type,"data":data}})
        }
        Some("file") => {
            serde_json::json!({"type":"text", "text":format!("Stored file: {}", part["file"])})
        }
        _ => return Ok(None),
    };
    if part
        .get(PROMPT_CACHE_BREAKPOINT_FIELD)
        .and_then(Value::as_bool)
        == Some(true)
    {
        block["cache_control"] = serde_json::json!({"type":"ephemeral"});
    }
    Ok(Some(block))
}

fn tool_content_blocks(output: &Value) -> Result<Vec<Value>> {
    output
        .as_array()
        .ok_or_else(|| Error::Provider("tool result content must be an array".into()))?
        .iter()
        .map(|part| {
            content_block(part)?
                .ok_or_else(|| Error::Provider("unsupported tool result content".into()))
        })
        .collect()
}

fn tool_result_block(
    item: &Value,
    next: Option<&Value>,
    discovery: ToolDiscoveryMode,
    catalog_revision: &str,
    search_calls: &BTreeSet<String>,
    deferred_tool_names: &BTreeSet<&str>,
) -> Result<Value> {
    let call_id = required_string(item, "call_id")?;
    let load = next.map(ToolLoad::from_input).transpose()?.flatten();
    let references = load
        .filter(|_| discovery == ToolDiscoveryMode::Native && search_calls.contains(call_id))
        .map_or_else(Vec::new, |load| {
            tool_references(load, catalog_revision, deferred_tool_names)
        });
    let content = if references.is_empty() {
        Value::Array(tool_content_blocks(item.get("output").ok_or_else(
            || Error::Provider("tool result omitted content".into()),
        )?)?)
    } else {
        Value::Array(references)
    };
    Ok(serde_json::json!({
        "type": "tool_result",
        "tool_use_id": call_id,
        "content": content,
        "is_error": item.get(TOOL_ERROR_FIELD).and_then(Value::as_bool).unwrap_or(false)
    }))
}

fn follows_search_result(previous: Option<&Value>, search_calls: &BTreeSet<String>) -> bool {
    previous
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .and_then(|item| item.get("call_id").and_then(Value::as_str))
        .is_some_and(|call_id| search_calls.contains(call_id))
}

fn replay_standalone_tool_load(
    messages: &mut Vec<WireMessage<'_>>,
    load: Option<ToolLoad>,
    follows_search_result: bool,
    index: usize,
    discovery: ToolDiscoveryMode,
    catalog_revision: &str,
    deferred_tool_names: &BTreeSet<&str>,
) {
    let Some(load) = load else {
        return;
    };
    if discovery != ToolDiscoveryMode::Native || follows_search_result {
        return;
    }
    let references = tool_references(load, catalog_revision, deferred_tool_names);
    if references.is_empty() {
        return;
    }
    let call_id = format!("mobius-tool-load-{index}");
    push_message(
        messages,
        "assistant",
        [Cow::Owned(serde_json::json!({
            "type": "tool_use",
            "id": call_id,
            "name": TOOLS_SEARCH_NAME,
            "input": {"query": "restore loaded session tools"}
        }))],
    );
    push_message(
        messages,
        "user",
        [Cow::Owned(serde_json::json!({
            "type": "tool_result",
            "tool_use_id": call_id,
            "content": references,
            "is_error": false
        }))],
    );
}

fn tool_references(
    load: ToolLoad,
    catalog_revision: &str,
    deferred_tool_names: &BTreeSet<&str>,
) -> Vec<Value> {
    if load.catalog_revision != catalog_revision {
        return Vec::new();
    }
    load.tools
        .into_iter()
        .filter(|name| deferred_tool_names.contains(name.as_str()))
        .map(|name| {
            serde_json::json!({
                "type": "tool_reference",
                "tool_name": name
            })
        })
        .collect()
}

fn push_message<'a>(
    messages: &mut Vec<WireMessage<'a>>,
    role: &'a str,
    blocks: impl IntoIterator<Item = Cow<'a, Value>>,
) {
    let mut blocks = blocks.into_iter().peekable();
    if blocks.peek().is_none() {
        return;
    }
    if let Some(last) = messages.last_mut()
        && last.role == role
    {
        last.content.extend(blocks);
    } else {
        messages.push(WireMessage {
            role,
            content: blocks.collect(),
        });
    }
}

struct WireTools<'a> {
    direct: &'a [Arc<ToolDefinition>],
    deferred: &'a [Arc<ToolDefinition>],
    web_search: bool,
}

impl Serialize for WireTools<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Function<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            defer_loading: Option<bool>,
            description: &'a str,
            #[serde(serialize_with = "serialize_tool_schema")]
            input_schema: &'a Value,
            name: &'a str,
        }
        let mut sequence = serializer.serialize_seq(None)?;
        for (tool, deferred) in self
            .direct
            .iter()
            .map(|tool| (tool, false))
            .chain(self.deferred.iter().map(|tool| (tool, true)))
        {
            sequence.serialize_element(&Function {
                defer_loading: deferred.then_some(true),
                description: &tool.description,
                input_schema: &tool.parameters,
                name: &tool.name,
            })?;
        }
        if self.web_search {
            #[derive(Serialize)]
            struct Search {
                name: &'static str,
                #[serde(rename = "type")]
                kind: &'static str,
            }
            sequence.serialize_element(&Search {
                name: "web_search",
                kind: "web_search_20260318",
            })?;
        }
        sequence.end()
    }
}

fn serialize_tool_schema<S: serde::Serializer>(
    schema: &Value,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    match schema.as_object() {
        // Anthropic rejects root combinators; tool handlers still validate arguments.
        Some(fields) => serializer.collect_map(
            fields
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "oneOf" | "allOf" | "anyOf")),
        ),
        None => schema.serialize(serializer),
    }
}

fn wire_tools<'a>(
    tools: &'a [Arc<ToolDefinition>],
    deferred_tools: &'a [Arc<ToolDefinition>],
    web_search: bool,
) -> WireTools<'a> {
    WireTools {
        direct: tools,
        deferred: deferred_tools,
        web_search,
    }
}

fn event_index(event: &Value) -> Result<usize> {
    event
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| Error::Provider("Anthropic event omitted block index".into()))
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    let value = string_field(value, field)?;
    if value.is_empty() {
        return Err(Error::Provider(
            format!("Anthropic value omitted {field}").into(),
        ));
    }
    Ok(value)
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Provider(format!("Anthropic value omitted {field}").into()))
}

fn append_string(value: &mut Value, field: &str, addition: &str) {
    if let Some(Value::String(current)) = value.get_mut(field) {
        current.push_str(addition);
    } else {
        value[field] = Value::String(addition.to_string());
    }
}

fn update_i64(target: &mut i64, value: &Value, path: &str) -> Result<()> {
    if let Some(value) = usage_i64(Some(value), path, "Anthropic")? {
        *target = value;
    }
    Ok(())
}

pub(super) fn provider() -> ProviderDefinition {
    ProviderDefinition::from_metadata(
        "anthropic",
        &MANIFEST,
        MANIFEST.api_key_auth(),
        Some(&CATALOG),
        build_provider,
    )
    .with_image_input()
    .with_credentialless_endpoints()
    .with_replay_reasoning_field(RAW_CONTENT)
}

fn build_provider(config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
    let tool_discovery = config
        .tool_discovery
        .unwrap_or_else(|| provider().tool_discovery(&config.model, config.base_url.as_deref()));
    let base_url = config
        .base_url
        .ok_or_else(|| Error::Config("Anthropic requires a base URL".into()))?;
    let api_key = config.credential.into_optional_api_key("anthropic")?;
    let mut provider = Anthropic::with_client(api_key, base_url, config.model, config.http)?
        .with_transport_settings(config.transport)?;
    provider.tool_discovery = tool_discovery;
    let provider = match config.reasoning_effort {
        Some(effort) => provider.with_reasoning_effort(effort)?,
        None => provider,
    };
    let provider = match config.web_search {
        HostedWebSearch::Off => provider,
        HostedWebSearch::Cached => {
            return Err(Error::Config(
                "Anthropic does not support cached web search".into(),
            ));
        }
        HostedWebSearch::Live => provider.with_web_search(),
    };
    Ok(Arc::new(provider))
}

#[cfg(test)]
#[path = "anthropic_tests.rs"]
mod tests;
