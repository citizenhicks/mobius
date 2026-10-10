//! Shared Chat Completions wire primitives for native providers.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use serde::ser::SerializeSeq as _;
use serde_json::Value;

use super::{ImageDataUrl, MAX_TOOL_CALLS, ToolDefinition, image_input, usage_i64};
use crate::protocol::TokenUsage;
use crate::{Error, Result};

#[derive(Serialize)]
pub(super) struct RequestBody<'a, C> {
    model: &'a str,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) stream_options: Option<StreamOptions>,
    messages: Vec<WireMessage<'a, C>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<WireTools<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
}

#[derive(Serialize)]
struct WireMessage<'a, C> {
    role: &'a str,
    #[serde(flatten)]
    payload: C,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Serialize)]
pub(super) struct StreamOptions {
    pub(super) include_usage: bool,
}

pub(super) enum MessageSource<'a> {
    System(&'a str),
    History { role: &'a str, item: &'a Value },
    Tool(&'a Value),
}

impl<'a, C: Default> RequestBody<'a, C> {
    pub(super) fn new(
        model: &'a str,
        reasoning_effort: Option<&'a str>,
        request: super::ModelRequest<'a>,
        provider: &str,
        content: impl FnMut(MessageSource<'a>) -> Result<C>,
    ) -> Result<Self> {
        let has_tools = !request.tools.is_empty();
        Ok(Self {
            model,
            stream: true,
            stream_options: None,
            messages: wire_messages(request.instructions, request.input, provider, content)?,
            tools: has_tools.then_some(WireTools(request.tools)),
            tool_choice: has_tools.then_some("auto"),
            parallel_tool_calls: has_tools.then_some(true),
            reasoning_effort,
            prompt_cache_key: request.prompt_cache.map(|cache| cache.key),
        })
    }
}

#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction<'a>,
}

#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    arguments: Cow<'a, str>,
}

fn wire_messages<'a, C: Default>(
    instructions: &'a str,
    input: super::ModelInput<'a>,
    provider: &str,
    mut content: impl FnMut(MessageSource<'a>) -> Result<C>,
) -> Result<Vec<WireMessage<'a, C>>> {
    let mut messages = Vec::new();
    if !instructions.trim().is_empty() {
        messages.push(WireMessage {
            role: "system",
            payload: content(MessageSource::System(instructions))?,
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }
    for item in input.iter() {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                if messages
                    .last()
                    .is_none_or(|message| message.role != "assistant")
                {
                    messages.push(WireMessage {
                        role: "assistant",
                        payload: C::default(),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                    });
                }
                let call = WireToolCall {
                    id: required_string(item, "call_id", provider)?,
                    kind: "function",
                    function: WireFunction {
                        name: required_string(item, "name", provider)?,
                        arguments: argument_text(item.get("arguments"))?,
                    },
                };
                messages
                    .last_mut()
                    .expect("assistant message was just inserted")
                    .tool_calls
                    .push(call);
            }
            Some("function_call_output") => messages.push(WireMessage {
                role: "tool",
                tool_calls: Vec::new(),
                tool_call_id: Some(required_string(item, "call_id", provider)?),
                payload: content(MessageSource::Tool(
                    item.get("output")
                        .ok_or_else(|| Error::Provider("tool result omitted content".into()))?,
                ))?,
            }),
            Some("message") | None if item.get("role").is_some() => {
                let role = match required_string(item, "role", provider)? {
                    "developer" => "system",
                    role @ ("system" | "user" | "assistant" | "tool") => role,
                    role => {
                        return Err(Error::Provider(
                            format!("unsupported {provider} message role `{role}`").into(),
                        ));
                    }
                };
                messages.push(WireMessage {
                    role,
                    payload: content(MessageSource::History { role, item })?,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                });
            }
            Some(_) | None => {}
        }
    }
    if !messages
        .iter()
        .any(|message| matches!(message.role, "user" | "assistant" | "tool"))
    {
        return Err(Error::Provider(
            format!("{provider} request has no conversation messages").into(),
        ));
    }
    Ok(messages)
}

pub(super) struct WireTools<'a>(pub(super) &'a [Arc<ToolDefinition>]);

impl Serialize for WireTools<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Function<'a> {
            description: &'a str,
            name: &'a str,
            parameters: &'a Value,
        }
        #[derive(Serialize)]
        struct Tool<'a> {
            function: Function<'a>,
            #[serde(rename = "type")]
            kind: &'static str,
        }
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for tool in self.0 {
            sequence.serialize_element(&Tool {
                function: Function {
                    description: &tool.description,
                    name: &tool.name,
                    parameters: &tool.parameters,
                },
                kind: "function",
            })?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum WireContent<'a> {
    Text(Cow<'a, str>),
    Parts(Vec<WireContentPart<'a>>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum WireContentPart<'a> {
    Text { text: Cow<'a, str> },
    ImageUrl { image_url: ImageUrl<'a> },
}

#[derive(Serialize)]
pub(super) struct ImageUrl<'a> {
    url: ImageDataUrl<'a>,
}

impl<'a> WireContent<'a> {
    pub(super) fn tool_result(output: &'a Value, provider: &str) -> Result<Self> {
        let Some(parts) = output.as_array().filter(|parts| {
            parts
                .iter()
                .any(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
        }) else {
            return Ok(Self::Text(tool_text(output)?));
        };
        let mut content = Vec::with_capacity(parts.len());
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("input_text") => content.push(WireContentPart::Text {
                    text: part
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| Error::Provider("invalid tool text".into()))?
                        .into(),
                }),
                Some("file") => content.push(WireContentPart::Text {
                    text: Cow::Owned(format!("Stored file: {}", part["file"])),
                }),
                Some("input_image") => {
                    if let Some((media_type, data)) = image_input(part, provider)? {
                        content.push(WireContentPart::ImageUrl {
                            image_url: ImageUrl {
                                url: ImageDataUrl(media_type, data),
                            },
                        });
                    }
                }
                _ => {
                    return Err(Error::Provider(
                        "selected provider cannot represent this tool observation".into(),
                    ));
                }
            }
        }
        Ok(Self::Parts(content))
    }

    pub(super) fn new(content: Option<&'a Value>, provider: &str) -> Result<Self> {
        let Some(Value::Array(parts)) = content else {
            return Ok(Self::Text(content_text(content)));
        };
        if !parts
            .iter()
            .any(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
        {
            return Ok(Self::Text(content_text(content)));
        }
        let mut output = Vec::with_capacity(parts.len());
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("input_text" | "output_text" | "text") => output.push(WireContentPart::Text {
                    text: part
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                }),
                Some("input_image") => {
                    if let Some((media_type, data)) = image_input(part, provider)? {
                        output.push(WireContentPart::ImageUrl {
                            image_url: ImageUrl {
                                url: ImageDataUrl(media_type, data),
                            },
                        });
                    }
                }
                None | Some(_) => {}
            }
        }
        Ok(Self::Parts(output))
    }
}

pub(super) fn content_text(content: Option<&Value>) -> Cow<'_, str> {
    match content {
        Some(Value::String(text)) => Cow::Borrowed(text),
        Some(Value::Array(parts)) => {
            let mut text = parts
                .iter()
                .filter(|part| {
                    matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("input_text" | "output_text" | "text")
                    )
                })
                .filter_map(|part| part.get("text").and_then(Value::as_str));
            match (text.next(), text.next()) {
                (None, _) => Cow::Borrowed(""),
                (Some(first), None) => Cow::Borrowed(first),
                (Some(first), Some(second)) => {
                    Cow::Owned([first, second].into_iter().chain(text).collect())
                }
            }
        }
        Some(value) => Cow::Owned(value.to_string()),
        None => Cow::Borrowed(""),
    }
}

#[derive(Default)]
pub(super) struct ToolCalls(BTreeMap<usize, PendingTool>);

#[derive(Default)]
struct PendingTool {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCalls {
    pub(super) fn append(&mut self, delta: &Value, provider: &str) -> Result<()> {
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
            if self.0.len() >= MAX_TOOL_CALLS && !self.0.contains_key(&index) {
                return Err(Error::Provider(
                    format!("{provider} returned more than {MAX_TOOL_CALLS} tool calls").into(),
                ));
            }
            let pending = self.0.entry(index).or_default();
            set_fragment(
                &mut pending.id,
                call.get("id").and_then(Value::as_str),
                "ID",
                provider,
            )?;
            set_fragment(
                &mut pending.name,
                call.pointer("/function/name").and_then(Value::as_str),
                "name",
                provider,
            )?;
            if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                pending.arguments.push_str(arguments);
            }
        }
        Ok(())
    }

    pub(super) fn finish(self, provider: &str) -> Result<Vec<Value>> {
        self.0
            .into_values()
            .map(|call| {
                let arguments = if call.arguments.is_empty() {
                    "{}".into()
                } else {
                    call.arguments
                };
                Ok(serde_json::json!({
                    "type": "function_call",
                    "call_id": required(call.id, "tool-call ID", provider)?,
                    "name": required(call.name, "tool name", provider)?,
                    "arguments": arguments
                }))
            })
            .collect()
    }
}

pub(super) fn argument_text(arguments: Option<&Value>) -> Result<Cow<'_, str>> {
    match arguments {
        Some(Value::String(arguments)) => {
            serde_json::from_str::<serde::de::IgnoredAny>(arguments)?;
            Ok(Cow::Borrowed(arguments))
        }
        Some(arguments) => Ok(Cow::Owned(serde_json::to_string(arguments)?)),
        None => Err(Error::Provider("function call omitted arguments".into())),
    }
}

pub(super) fn required_string<'a>(
    value: &'a Value,
    field: &str,
    provider: &str,
) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Provider(format!("{provider} value omitted {field}").into()))
}

fn required(value: String, field: &str, provider: &str) -> Result<String> {
    (!value.is_empty())
        .then_some(value)
        .ok_or_else(|| Error::Provider(format!("{provider} response omitted {field}").into()))
}

fn set_fragment(
    target: &mut String,
    fragment: Option<&str>,
    field: &str,
    provider: &str,
) -> Result<()> {
    let Some(fragment) = fragment.filter(|fragment| !fragment.is_empty()) else {
        return Ok(());
    };
    if target.is_empty() {
        target.push_str(fragment);
    } else if target != fragment {
        return Err(Error::Provider(
            format!("{provider} changed a streamed tool-call {field}").into(),
        ));
    }
    Ok(())
}

pub(super) fn decode_usage(usage: Option<&Value>, provider: &str) -> Result<TokenUsage> {
    let value =
        |pointer| -> Result<i64> { Ok(usage_i64(usage, pointer, provider)?.unwrap_or_default()) };
    let cached_input_tokens =
        value("/cached_tokens")?.max(value("/prompt_tokens_details/cached_tokens")?);
    Ok(TokenUsage {
        input_tokens: value("/prompt_tokens")?,
        cached_input_tokens,
        cache_write_input_tokens: value("/prompt_tokens_details/cache_write_tokens")?,
        output_tokens: value("/completion_tokens")?,
        reasoning_output_tokens: value("/completion_tokens_details/reasoning_tokens")?,
        total_tokens: value("/total_tokens")?,
    })
}

pub(super) async fn post(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    body: Vec<u8>,
    provider: &str,
) -> Result<reqwest::Response> {
    let mut request = client.post(format!("{base_url}/chat/completions"));
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(super::transport::status_error(response, provider).await);
    }
    Ok(response)
}

pub(super) fn tool_text(output: &Value) -> Result<Cow<'_, str>> {
    if let Some([part]) = output.as_array().map(Vec::as_slice)
        && part.get("type").and_then(Value::as_str) == Some("input_text")
    {
        return Ok(Cow::Borrowed(
            part.get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Provider("invalid tool text".into()))?,
        ));
    }
    Ok(Cow::Owned(super::media::output_text(output)?))
}
