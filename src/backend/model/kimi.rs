//! Native Kimi Chat Completions provider.

use std::collections::BTreeMap;
use std::sync::Arc;

use reqwest::Client;
use serde_json::Value;

use super::MAX_TOOL_CALLS;
use super::Model;
use super::ModelEventSink;
use super::ModelOutput;
use super::ModelRequest;
use super::PromptCacheCapability;
use super::REPLAY_REASONING_FIELD;
use super::ToolDefinition;
use super::image_data_url;
use super::image_input;
use super::provider::{ProviderAuth, ProviderBuildConfig, ProviderDefinition, validate_base_url};
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
use crate::protocol::TokenUsage;

mod manifest {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_backend_model_kimi_manifest.rs"
    ));
}

const DEFAULT_BASE_URL: &str = "https://api.moonshot.ai/v1";

/// Kimi's native Chat Completions provider.
pub struct Kimi {
    client: Client,
    api_key: Option<String>,
    base_url: String,
    model: String,
    reasoning_effort: Option<String>,
}

impl Kimi {
    /// Creates a provider for a Moonshot Kimi endpoint.
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
            return Err(Error::Config("MOONSHOT_API_KEY is empty".into()));
        }
        let base_url = base_url.into().trim_end_matches('/').to_string();
        validate_base_url(&base_url)?;
        let model = model.into();
        if model.trim().is_empty() {
            return Err(Error::Config("Kimi model is empty".into()));
        }
        Ok(Self {
            client,
            api_key,
            base_url,
            model,
            reasoning_effort: None,
        })
    }

    /// Selects an effort advertised for this Kimi model.
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

    async fn send_response(
        &self,
        request: ModelRequest<'_>,
        events: ModelEventSink,
    ) -> Result<ModelOutput> {
        let body = self.request_body(&request)?;
        let mut response = self.post(&body).await?;
        let mut bytes = Vec::new();
        let mut stream = StreamState::default();
        let mut stream_bytes = 0;
        while let Some(chunk) = response.chunk().await? {
            push_sse_chunk(&mut bytes, &mut stream_bytes, &chunk, "Kimi")?;
            while let Some(frame) = take_sse_frame(&mut bytes)? {
                if let Some(data) = frame_data(&frame) {
                    stream.apply_data(&data, &events)?;
                }
            }
            if bytes.len() > MAX_SSE_FRAME_BYTES {
                return Err(Error::Provider("Kimi SSE frame exceeded size limit".into()));
            }
        }
        stream.finish()
    }

    fn request_body(&self, request: &ModelRequest<'_>) -> Result<Value> {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": wire_messages(request.instructions, request.input)?,
            "stream": true,
            "stream_options": {"include_usage": true}
        });
        if let Some(prompt_cache) = request.prompt_cache {
            body["prompt_cache_key"] = Value::String(prompt_cache.key.into());
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(wire_tools(request.tools));
            body["tool_choice"] = Value::String("auto".into());
            body["parallel_tool_calls"] = Value::Bool(true);
        }
        self.apply_reasoning(&mut body);
        Ok(body)
    }

    fn apply_reasoning(&self, body: &mut Value) {
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = Value::String(effort.clone());
        }
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response> {
        let mut request = self
            .client
            .post(format!("{}/chat/completions", self.base_url));
        if let Some(api_key) = &self.api_key {
            request = request.bearer_auth(api_key);
        }
        let response = request.json(body).send().await?;
        if !response.status().is_success() {
            return Err(status_error(response, "Kimi").await);
        }
        Ok(response)
    }
}

impl Model for Kimi {
    fn info(&self) -> ModelInfo {
        ModelInfo {
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }

    fn supports_image_input(&self) -> bool {
        true
    }

    fn prompt_cache_capability(&self) -> PromptCacheCapability {
        PromptCacheCapability::Implicit
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
    text: String,
    reasoning: String,
    tools: BTreeMap<usize, PendingTool>,
    usage: TokenUsage,
    done: bool,
}

#[derive(Default)]
struct PendingTool {
    id: String,
    name: String,
    arguments: String,
}

impl StreamState {
    fn apply_data(&mut self, data: &str, events: &ModelEventSink) -> Result<()> {
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let chunk: Value = serde_json::from_str(data)?;
        if let Some(message) = chunk.pointer("/error/message").and_then(Value::as_str) {
            return Err(Error::Provider(
                format!("Kimi stream error: {message}").into(),
            ));
        }
        if let Some(usage) = chunk.get("usage") {
            self.usage = decode_usage(Some(usage))?;
        }
        let Some(choice) = chunk.pointer("/choices/0") else {
            return Ok(());
        };
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
            self.reasoning.push_str(reasoning);
            if !reasoning.is_empty() {
                events(ModelEvent::ReasoningDelta(reasoning.to_string()))?;
            }
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.text.push_str(text);
            if !text.is_empty() {
                events(ModelEvent::TextDelta(text.to_string()))?;
            }
        }
        for (position, call) in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|index| usize::try_from(index).ok())
                .unwrap_or(position);
            if self.tools.len() >= MAX_TOOL_CALLS && !self.tools.contains_key(&index) {
                return Err(Error::Provider(
                    format!("Kimi returned more than {MAX_TOOL_CALLS} tool calls").into(),
                ));
            }
            let pending = self.tools.entry(index).or_default();
            set_fragment(
                &mut pending.id,
                call.get("id").and_then(Value::as_str),
                "ID",
            )?;
            set_fragment(
                &mut pending.name,
                call.pointer("/function/name").and_then(Value::as_str),
                "name",
            )?;
            if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                pending.arguments.push_str(arguments);
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<ModelOutput> {
        if !self.done {
            return Err(Error::Provider(
                "Kimi stream ended before the [DONE] event".into(),
            ));
        }
        let has_tools = !self.tools.is_empty();
        let calls = self
            .tools
            .into_values()
            .map(|call| {
                let arguments = if call.arguments.is_empty() {
                    "{}".to_string()
                } else {
                    call.arguments
                };
                Ok(serde_json::json!({
                    "type": "function_call",
                    "call_id": required(call.id, "tool-call ID")?,
                    "name": required(call.name, "tool name")?,
                    "arguments": arguments
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        if self.text.is_empty() && self.reasoning.is_empty() && calls.is_empty() {
            return Err(Error::Provider("Kimi returned no output".into()));
        }
        let mut output = Vec::new();
        if !self.text.is_empty() || !self.reasoning.is_empty() {
            output.push(serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": self.text
                }],
                (REPLAY_REASONING_FIELD): self.reasoning
            }));
        }
        output.extend(calls);
        ModelOutput::from_output(output, !has_tools, self.usage)
    }
}

fn wire_messages(instructions: &str, input: &[Value]) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    if !instructions.trim().is_empty() {
        messages.push(serde_json::json!({"role": "system", "content": instructions}));
    }
    for item in input {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => push_tool_call(&mut messages, item)?,
            Some("function_call_output") => messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": required_string(item, "call_id")?,
                "content": value_text(item.get("output"))
            })),
            Some("message") | None if item.get("role").is_some() => {
                push_history_message(&mut messages, item)?
            }
            Some(_) | None => {}
        }
    }
    if !messages.iter().any(|message| {
        matches!(
            message.get("role").and_then(Value::as_str),
            Some("user" | "assistant" | "tool")
        )
    }) {
        return Err(Error::Provider(
            "Kimi request has no conversation messages".into(),
        ));
    }
    Ok(messages)
}

fn push_history_message(messages: &mut Vec<Value>, item: &Value) -> Result<()> {
    let role = match required_string(item, "role")? {
        "developer" => "system",
        role @ ("system" | "user" | "assistant" | "tool") => role,
        role => {
            return Err(Error::Provider(
                format!("unsupported Kimi message role `{role}`").into(),
            ));
        }
    };
    let mut message = serde_json::json!({
        "role": role,
        "content": wire_content(item.get("content"))?
    });
    if role == "assistant"
        && let Some(reasoning) = item.get(REPLAY_REASONING_FIELD).and_then(Value::as_str)
        && !reasoning.is_empty()
    {
        message["reasoning_content"] = Value::String(reasoning.to_string());
    }
    messages.push(message);
    Ok(())
}

fn push_tool_call(messages: &mut Vec<Value>, item: &Value) -> Result<()> {
    let call = serde_json::json!({
        "id": required_string(item, "call_id")?,
        "type": "function",
        "function": {
            "name": required_string(item, "name")?,
            "arguments": argument_text(item.get("arguments"))?
        }
    });
    if messages
        .last()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        != Some("assistant")
    {
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [call]
        }));
        return Ok(());
    }
    let calls = messages
        .last_mut()
        .ok_or_else(|| Error::Provider("Kimi assistant message disappeared".into()))?
        .as_object_mut()
        .ok_or_else(|| Error::Provider("Kimi assistant message was not an object".into()))?
        .entry("tool_calls")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| Error::Provider("Kimi tool_calls was not an array".into()))?;
    calls.push(call);
    Ok(())
}

fn wire_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters
                }
            })
        })
        .collect()
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("input_text" | "output_text" | "text")
                )
            })
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn wire_content(content: Option<&Value>) -> Result<Value> {
    let Some(Value::Array(parts)) = content else {
        return Ok(Value::String(content_text(content)));
    };
    if !parts
        .iter()
        .any(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
    {
        return Ok(Value::String(content_text(content)));
    }
    let mut output = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => output.push(serde_json::json!({
                "type": "text",
                "text": part.get("text").and_then(Value::as_str).unwrap_or_default()
            })),
            Some("input_image") => {
                let Some((media_type, data)) = image_input(part, "Kimi")? else {
                    continue;
                };
                output.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": {"url": image_data_url(media_type, data)}
                }));
            }
            None | Some(_) => {}
        }
    }
    Ok(Value::Array(output))
}

fn argument_text(arguments: Option<&Value>) -> Result<String> {
    match arguments {
        Some(Value::String(arguments)) => {
            serde_json::from_str::<Value>(arguments)?;
            Ok(arguments.clone())
        }
        Some(arguments) => Ok(serde_json::to_string(arguments)?),
        None => Err(Error::Provider("function call omitted arguments".into())),
    }
}

fn value_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Provider(format!("Kimi value omitted {field}").into()))
}

fn required(value: String, field: &str) -> Result<String> {
    (!value.is_empty())
        .then_some(value)
        .ok_or_else(|| Error::Provider(format!("Kimi response omitted {field}").into()))
}

fn set_fragment(target: &mut String, fragment: Option<&str>, field: &str) -> Result<()> {
    let Some(fragment) = fragment.filter(|fragment| !fragment.is_empty()) else {
        return Ok(());
    };
    if target.is_empty() {
        target.push_str(fragment);
    } else if target != fragment {
        return Err(Error::Provider(
            format!("Kimi changed a streamed tool-call {field}").into(),
        ));
    }
    Ok(())
}

fn decode_usage(usage: Option<&Value>) -> Result<TokenUsage> {
    let value =
        |pointer| -> Result<i64> { Ok(usage_i64(usage, pointer, "Kimi")?.unwrap_or_default()) };
    let cached_input_tokens =
        value("/cached_tokens")?.max(value("/prompt_tokens_details/cached_tokens")?);
    Ok(TokenUsage {
        input_tokens: value("/prompt_tokens")?,
        cached_input_tokens,
        cache_write_input_tokens: 0,
        output_tokens: value("/completion_tokens")?,
        reasoning_output_tokens: value("/completion_tokens_details/reasoning_tokens")?,
        total_tokens: value("/total_tokens")?,
    })
}

pub(super) const fn provider() -> ProviderDefinition {
    ProviderDefinition::new(
        "kimi",
        manifest::PROVIDER_LABEL,
        "kimi",
        manifest::PROVIDER_DESCRIPTION,
        ProviderAuth::ApiKey("MOONSHOT_API_KEY"),
        manifest::MODELS,
        manifest::DEFAULT_MODEL,
        manifest::SEARCH,
        build_provider,
    )
    .with_image_input()
    .with_tool_discovery(
        manifest::TOOL_DISCOVERY,
        manifest::CUSTOM_ENDPOINT_TOOL_DISCOVERY,
    )
    .with_base_url(DEFAULT_BASE_URL)
    .with_credentialless_endpoints()
}

fn build_provider(config: ProviderBuildConfig) -> Result<Arc<dyn Model>> {
    let base_url = config
        .base_url
        .ok_or_else(|| Error::Config("Kimi requires a base URL".into()))?;
    let api_key = config.credential.into_optional_api_key("kimi")?;
    let provider = Kimi::with_client(api_key, base_url, config.model, config.http)?;
    let provider = match config.reasoning_effort {
        Some(effort) => provider.with_reasoning_effort(effort)?,
        None => provider,
    };
    Ok(Arc::new(provider))
}

#[cfg(test)]
#[path = "kimi_tests.rs"]
mod tests;
