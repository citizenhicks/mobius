//! Native Anthropic Messages API provider.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use super::Model;
use super::ModelEventSink;
use super::ModelOutput;
use super::ModelPricing;
use super::ModelRequest;
use super::PROMPT_CACHE_BREAKPOINT_FIELD;
use super::PromptCacheMode;
use super::REPLAY_REASONING_FIELD;
use super::TOOL_ERROR_FIELD;
use super::TOOLS_SEARCH_NAME;
use super::ToolDefinition;
use super::ToolLoad;
use super::image_input;
use super::provider::HostedWebSearch;
use super::provider::ProviderAuth;
use super::provider::ProviderBuildConfig;
use super::provider::ProviderDefinition;
use super::provider::validate_base_url;
use super::transport::MAX_SSE_FRAME_BYTES;
use super::transport::frame_data;
use super::transport::push_sse_chunk;
use super::transport::status_error;
use super::transport::streaming_client;
use super::transport::take_sse_frame;
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
use crate::protocol::WebSearchAction;

mod manifest {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_backend_model_anthropic_manifest.rs"
    ));
}

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
const API_VERSION: &str = "2023-06-01";
const MAX_OUTPUT_TOKENS: u64 = 64_000;
const MAX_CONTENT_BLOCKS: usize = 1_024;
const RAW_CONTENT: &str = "_anthropic_content";
const SONNET_5_STANDARD_PRICING_START_UNIX_SECONDS: u64 = 1_788_220_800;

fn anthropic_model_pricing(model: &str) -> Option<ModelPricing> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    anthropic_model_pricing_at(model, now)
}

pub(super) fn anthropic_model_pricing_at(model: &str, unix_seconds: u64) -> Option<ModelPricing> {
    match model {
        "claude-sonnet-5" if unix_seconds < SONNET_5_STANDARD_PRICING_START_UNIX_SECONDS => {
            Some(ModelPricing::new(2_000_000, 200_000, 2_500_000, 10_000_000))
        }
        "claude-sonnet-5" => Some(ModelPricing::new(3_000_000, 300_000, 3_750_000, 15_000_000)),
        "claude-opus-4-8" => Some(ModelPricing::new(5_000_000, 500_000, 6_250_000, 25_000_000)),
        "claude-haiku-4-5" => Some(ModelPricing::new(1_000_000, 100_000, 1_250_000, 5_000_000)),
        _ => None,
    }
}

/// Anthropic's native Messages API provider.
pub struct Anthropic {
    client: Client,
    api_key: Option<String>,
    base_url: String,
    model: String,
    tool_discovery: ToolDiscoveryMode,
    reasoning_effort: Option<String>,
    web_search: bool,
}

impl Anthropic {
    /// Creates a provider for an Anthropic Messages API endpoint.
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self> {
        Self::with_client(Some(api_key.into()), base_url, model, streaming_client()?)
    }

    fn with_client(
        api_key: Option<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        client: Client,
    ) -> Result<Self> {
        if api_key.as_deref().is_some_and(|key| key.trim().is_empty()) {
            return Err(Error::Config("ANTHROPIC_API_KEY is empty".into()));
        }
        let base_url = base_url.into().trim_end_matches('/').to_string();
        validate_base_url(&base_url)?;
        let model = model.into();
        if model.trim().is_empty() {
            return Err(Error::Config("Anthropic model is empty".into()));
        }
        let tool_discovery = provider().tool_discovery(&model, Some(&base_url));
        Ok(Self {
            client,
            api_key,
            base_url,
            model,
            tool_discovery,
            reasoning_effort: None,
            web_search: false,
        })
    }

    /// Enables adaptive thinking at one supported Anthropic effort level.
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
        self.reasoning_effort = Some(effort);
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
    ) -> Result<ModelOutput> {
        let body = self.request_body(
            request.instructions,
            request.input,
            request.catalog_revision,
            request.tools,
            request.deferred_tools,
            request.allow_hosted_tools,
        )?;
        let mut response = self.post(&body).await?;
        let mut bytes = Vec::new();
        let mut stream = StreamState::default();
        let mut stream_bytes = 0;
        while let Some(chunk) = response.chunk().await? {
            push_sse_chunk(&mut bytes, &mut stream_bytes, &chunk, "Anthropic")?;
            while let Some(frame) = take_sse_frame(&mut bytes)? {
                let Some(data) = frame_data(&frame) else {
                    continue;
                };
                stream.apply(serde_json::from_str(&data)?, &events)?;
            }
            if bytes.len() > MAX_SSE_FRAME_BYTES {
                return Err(Error::Provider(
                    "Anthropic SSE frame exceeded size limit".into(),
                ));
            }
        }
        if !stream.stopped {
            return Err(Error::Provider(
                "Anthropic stream ended before message_stop".into(),
            ));
        }
        stream.finish()
    }

    fn request_body(
        &self,
        instructions: &str,
        input: &[Value],
        catalog_revision: &str,
        tools: &[ToolDefinition],
        deferred_tools: &[ToolDefinition],
        allow_hosted_tools: bool,
    ) -> Result<Value> {
        let discovery = self.tool_discovery();
        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": MAX_OUTPUT_TOKENS,
            "system": instructions,
            "messages": translate_messages(
                input,
                discovery,
                catalog_revision,
                deferred_tools,
            )?,
            "tools": wire_tools(
                tools,
                if discovery == ToolDiscoveryMode::Native { deferred_tools } else { &[] },
                self.web_search && allow_hosted_tools,
            ),
            "stream": true
        });
        self.apply_reasoning(&mut body);
        Ok(body)
    }

    fn apply_reasoning(&self, body: &mut Value) {
        if let Some(effort) = &self.reasoning_effort {
            body["thinking"] = serde_json::json!({"type": "adaptive"});
            body["output_config"] = serde_json::json!({"effort": effort});
        }
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response> {
        let mut request = self
            .client
            .post(format!("{}/messages", self.base_url))
            .header("anthropic-version", API_VERSION);
        if let Some(api_key) = &self.api_key {
            request = request.header("x-api-key", api_key);
        }
        let response = request.json(body).send().await?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(status_error(response, "Anthropic").await)
        }
    }
}

impl Model for Anthropic {
    fn info(&self) -> ModelInfo {
        ModelInfo {
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
        }
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

    fn pricing(&self) -> Option<ModelPricing> {
        anthropic_model_pricing(&self.model)
    }

    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        events: ModelEventSink,
    ) -> BoxFuture<'a, Result<ModelOutput>> {
        Box::pin(self.send_response(request, events))
    }
}

#[derive(Default)]
struct StreamState {
    blocks: BTreeMap<usize, Value>,
    partial_json: BTreeMap<usize, String>,
    web_queries: BTreeMap<String, Option<String>>,
    usage: Usage,
    stop_reason: Option<String>,
    stopped: bool,
}

impl StreamState {
    fn apply(&mut self, event: Value, events: &ModelEventSink) -> Result<()> {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => self.usage.update(event.pointer("/message/usage"))?,
            Some("content_block_start") => self.start_block(&event, events)?,
            Some("content_block_delta") => self.delta_block(&event, events)?,
            Some("content_block_stop") => self.stop_block(&event)?,
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

    fn start_block(&mut self, event: &Value, events: &ModelEventSink) -> Result<()> {
        let index = event_index(event)?;
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
            .get("content_block")
            .cloned()
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
            events(ModelEvent::WebSearchStarted { call_id: id })?;
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
            events(ModelEvent::WebSearchCompleted { call_id, action })?;
        }
        self.blocks.insert(index, block);
        Ok(())
    }

    fn delta_block(&mut self, event: &Value, events: &ModelEventSink) -> Result<()> {
        let index = event_index(event)?;
        let delta = event
            .get("delta")
            .ok_or_else(|| Error::Provider("Anthropic content delta omitted value".into()))?;
        let block = self
            .blocks
            .get_mut(&index)
            .ok_or_else(|| Error::Provider("Anthropic delta referenced unknown block".into()))?;
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                let text = required_string(delta, "text")?;
                append_string(block, "text", text);
                events(ModelEvent::TextDelta(text.to_string()))?;
            }
            Some("thinking_delta") => {
                let thinking = required_string(delta, "thinking")?;
                append_string(block, "thinking", thinking);
                events(ModelEvent::ReasoningDelta(thinking.to_string()))?;
            }
            Some("signature_delta") => {
                block["signature"] =
                    Value::String(required_string(delta, "signature")?.to_string());
            }
            Some("input_json_delta") => self
                .partial_json
                .entry(index)
                .or_default()
                .push_str(required_string(delta, "partial_json")?),
            Some("citations_delta") => {
                if let Some(citation) = delta.get("citation") {
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
                        .push(citation.clone());
                }
            }
            None | Some(_) => {}
        }
        Ok(())
    }

    fn stop_block(&mut self, event: &Value) -> Result<()> {
        let index = event_index(event)?;
        let Some(partial) = self.partial_json.remove(&index) else {
            return Ok(());
        };
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
        Ok(())
    }

    fn finish(self) -> Result<ModelOutput> {
        let step_content = normalized_step_content(&self.blocks)?;
        let content = self.blocks.into_values().collect::<Vec<_>>();
        let calls = content
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
            .map(|block| {
                let arguments = block
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
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
    let citations: Vec<AnthropicCitation> = serde_json::from_value(citations.clone())
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

fn translate_messages(
    input: &[Value],
    discovery: ToolDiscoveryMode,
    catalog_revision: &str,
    deferred_tools: &[ToolDefinition],
) -> Result<Vec<Value>> {
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
                    push_message(&mut messages, role, content.clone());
                } else {
                    let mut blocks = Vec::new();
                    for part in item
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        match part.get("type").and_then(Value::as_str) {
                            Some("input_text" | "output_text") => {
                                let mut block = serde_json::json!({
                                    "type": "text",
                                    "text": part.get("text").and_then(Value::as_str).unwrap_or_default()
                                });
                                if part
                                    .get(PROMPT_CACHE_BREAKPOINT_FIELD)
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false)
                                {
                                    block["cache_control"] =
                                        serde_json::json!({"type": "ephemeral"});
                                }
                                blocks.push(block);
                            }
                            Some("input_image") => {
                                let Some((media_type, data)) = image_input(part, "Anthropic")?
                                else {
                                    continue;
                                };
                                blocks.push(serde_json::json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": media_type,
                                        "data": data
                                    }
                                }));
                            }
                            None | Some(_) => {}
                        }
                    }
                    push_message(&mut messages, role, blocks);
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
                        vec![serde_json::json!({
                            "type": "tool_use",
                            "id": call_id,
                            "name": name,
                            "input": serde_json::from_str::<Value>(required_string(item, "arguments")?)?
                        })],
                    );
                }
            }
            Some("function_call_output") => push_message(
                &mut messages,
                "user",
                vec![tool_result_block(
                    item,
                    input.get(index + 1),
                    discovery,
                    catalog_revision,
                    &search_calls,
                    &deferred_tool_names,
                )?],
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
    Ok(messages)
}

fn remember_search_call(search_calls: &mut BTreeSet<String>, call_id: &str, name: &str) {
    if name == TOOLS_SEARCH_NAME {
        search_calls.insert(call_id.to_string());
    }
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
        Value::String(string_field(item, "output")?.to_string())
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
    messages: &mut Vec<Value>,
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
        vec![serde_json::json!({
            "type": "tool_use",
            "id": call_id,
            "name": TOOLS_SEARCH_NAME,
            "input": {"query": "restore loaded session tools"}
        })],
    );
    push_message(
        messages,
        "user",
        vec![serde_json::json!({
            "type": "tool_result",
            "tool_use_id": call_id,
            "content": references,
            "is_error": false
        })],
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

fn push_message(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.extend(blocks);
        return;
    }
    messages.push(serde_json::json!({"role": role, "content": blocks}));
}

fn wire_tools(
    tools: &[ToolDefinition],
    deferred_tools: &[ToolDefinition],
    web_search: bool,
) -> Vec<Value> {
    let mut output = tools
        .iter()
        .map(|tool| (tool, false))
        .chain(deferred_tools.iter().map(|tool| (tool, true)))
        .map(|(tool, deferred)| {
            let mut wire = serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters
            });
            if deferred {
                wire["defer_loading"] = Value::Bool(true);
            }
            wire
        })
        .collect::<Vec<_>>();
    if web_search {
        output.push(serde_json::json!({
            "type": "web_search_20260318",
            "name": "web_search"
        }));
    }
    output
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

pub(super) const fn provider() -> ProviderDefinition {
    ProviderDefinition::new(
        "anthropic",
        manifest::PROVIDER_LABEL,
        "claude",
        manifest::PROVIDER_DESCRIPTION,
        ProviderAuth::ApiKey("ANTHROPIC_API_KEY"),
        manifest::MODELS,
        manifest::DEFAULT_MODEL,
        manifest::SEARCH,
        build_provider,
    )
    .with_image_input()
    .with_base_url(DEFAULT_BASE_URL)
    .with_tool_discovery(
        manifest::TOOL_DISCOVERY,
        manifest::CUSTOM_ENDPOINT_TOOL_DISCOVERY,
    )
    .with_credentialless_endpoints()
}

fn build_provider(config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
    let base_url = config
        .base_url
        .ok_or_else(|| Error::Config("Anthropic requires a base URL".into()))?;
    let api_key = config.credential.into_optional_api_key("anthropic")?;
    let provider = Anthropic::with_client(api_key, base_url, config.model, config.http)?;
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
