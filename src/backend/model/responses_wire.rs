//! Borrowed OpenAI Responses wire input; provider metadata never copies logical history.

use std::sync::Arc;

use serde::Serialize;
use serde::ser::{Error as _, SerializeMap as _, SerializeSeq as _};
use serde_json::Value;

use super::{ImageDataUrl, ModelInput, ToolDefinition, image_input};
use crate::protocol::{ToolLoad, content_parts};
use crate::{Error, Result};

/// Small provider envelope with borrowed history and schemas.
#[derive(Serialize)]
pub(super) struct ResponsesBody<'a> {
    #[serde(flatten)]
    pub(super) metadata: RequestMetadata<'a>,
    pub(super) input: WireInput<'a>,
    pub(super) tools: WireTools<'a>,
}

#[derive(Serialize)]
pub(super) struct RequestMetadata<'a> {
    include: [&'static str; 1],
    instructions: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) max_output_tokens: Option<u64>,
    model: &'a str,
    parallel_tool_calls: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) previous_response_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) service_tier: Option<&'a str>,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) stream: Option<bool>,
    tool_choice: &'static str,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub(super) kind: Option<&'static str>,
}

#[derive(Serialize)]
struct Reasoning<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<&'static str>,
}

impl<'a> RequestMetadata<'a> {
    pub(super) fn new(
        model: &'a str,
        request: super::ModelRequest<'a>,
        effort: Option<&'a str>,
        summary: bool,
    ) -> Self {
        Self {
            include: ["reasoning.encrypted_content"],
            instructions: request.instructions,
            max_output_tokens: None,
            model,
            parallel_tool_calls: true,
            previous_response_id: None,
            prompt_cache_key: request.prompt_cache.map(|cache| cache.key),
            reasoning: (effort.is_some() || summary).then_some(Reasoning {
                effort,
                summary: summary.then_some("auto"),
            }),
            service_tier: None,
            store: false,
            stream: Some(true),
            tool_choice: "auto",
            kind: None,
        }
    }
}

/// Provider function and hosted tools serialized without copying schemas.
#[derive(Debug, Clone, Copy)]
pub(super) struct WireTools<'a> {
    pub(super) functions: &'a [Arc<ToolDefinition>],
    pub(super) deferred: &'a [Arc<ToolDefinition>],
    pub(super) hosted: &'a [Value],
    pub(super) native_search: Option<&'a str>,
}

impl Serialize for WireTools<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        if let Some(kind) = self.native_search
            && !self.deferred.is_empty()
        {
            #[derive(Serialize)]
            struct Search<'a> {
                #[serde(rename = "type")]
                kind: &'a str,
            }
            sequence.serialize_element(&Search { kind })?;
        }
        for tool in self.functions {
            if self.native_search.is_none() || tool.name != super::TOOLS_SEARCH_NAME {
                sequence.serialize_element(&FunctionTool::new(tool, false))?;
            }
        }
        for tool in self.deferred {
            sequence.serialize_element(&FunctionTool::new(tool, true))?;
        }
        for tool in self.hosted {
            sequence.serialize_element(tool)?;
        }
        sequence.end()
    }
}

/// Validated logical input, serialized one borrowed item at a time.
#[derive(Debug, Clone, Copy)]
pub(super) struct WireInput<'a> {
    input: ModelInput<'a>,
    catalog_revision: &'a str,
    additional_tools: &'a [Arc<ToolDefinition>],
}

impl<'a> WireInput<'a> {
    pub(super) fn new(
        input: ModelInput<'a>,
        allow_images: bool,
        catalog_revision: &'a str,
        additional_tools: &'a [Arc<ToolDefinition>],
    ) -> Result<Self> {
        for item in input.iter() {
            if ToolLoad::from_input(item)?.is_some() {
                continue;
            }
            for part in content_parts(item).into_iter().flatten() {
                if part.get("type").and_then(Value::as_str) == Some("input_image") {
                    if !allow_images {
                        return Err(Error::Provider(
                            "this model provider does not support image attachments".into(),
                        ));
                    }
                    image_input(part, "Responses")?;
                }
            }
        }
        Ok(Self {
            input,
            catalog_revision,
            additional_tools,
        })
    }
}

impl Serialize for WireInput<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        // A later search can load a tool again; the model gets each definition once.
        let mut sent = std::collections::BTreeSet::new();
        for item in self.input.iter() {
            if let Some(load) = ToolLoad::from_input(item).map_err(S::Error::custom)? {
                if load.catalog_revision == self.catalog_revision {
                    let tools = load
                        .tools
                        .iter()
                        .filter_map(|name| {
                            self.additional_tools.iter().find(|tool| tool.name == *name)
                        })
                        .filter(|tool| sent.insert(tool.name.as_str()))
                        .map(|tool| FunctionTool::new(tool, false))
                        .collect::<Vec<_>>();
                    if !tools.is_empty() {
                        #[derive(Serialize)]
                        struct AdditionalTools<'a> {
                            #[serde(rename = "type")]
                            kind: &'a str,
                            role: &'static str,
                            tools: &'a [FunctionTool<'a>],
                        }
                        sequence.serialize_element(&AdditionalTools {
                            kind: "additional_tools",
                            role: "developer",
                            tools: &tools,
                        })?;
                    }
                }
                continue;
            }
            sequence.serialize_element(&WireItem { item })?;
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct FunctionTool<'a> {
    // Match the existing sorted JSON projection used by socket settings fingerprints.
    #[serde(skip_serializing_if = "Option::is_none")]
    defer_loading: Option<bool>,
    description: &'a str,
    name: &'a str,
    parameters: &'a Value,
    strict: bool,
    #[serde(rename = "type")]
    kind: &'a str,
}

impl<'a> FunctionTool<'a> {
    fn new(tool: &'a ToolDefinition, deferred: bool) -> Self {
        Self {
            defer_loading: deferred.then_some(true),
            description: &tool.description,
            name: &tool.name,
            parameters: &tool.parameters,
            strict: false,
            kind: "function",
        }
    }
}

struct WireItem<'a> {
    item: &'a Value,
}

impl Serialize for WireItem<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let Some(fields) = self.item.as_object() else {
            return self.item.serialize(serializer);
        };
        let kind = self.item.get("type").and_then(Value::as_str);
        let content_field = if kind == Some("function_call_output") {
            "output"
        } else {
            "content"
        };
        let mut map = serializer.serialize_map(None)?;
        for (key, value) in fields {
            if key.starts_with('_')
                || (key == "format" && kind == Some("reasoning"))
                || (key == "status"
                    && matches!(kind, Some("message" | "reasoning" | "function_call")))
            {
                continue;
            }
            if key == content_field
                && let Some(parts) = value.as_array()
            {
                map.serialize_entry(key, &WireContent { parts })?;
                continue;
            }
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

struct WireContent<'a> {
    parts: &'a [Value],
}

impl Serialize for WireContent<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.parts.len()))?;
        for part in self.parts {
            sequence.serialize_element(&WirePart { part })?;
        }
        sequence.end()
    }
}

struct WirePart<'a> {
    part: &'a Value,
}

impl Serialize for WirePart<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let Some(fields) = self.part.as_object() else {
            return self.part.serialize(serializer);
        };
        let mut map = serializer.serialize_map(None)?;
        match self.part.get("type").and_then(Value::as_str) {
            Some("file") => {
                map.serialize_entry("type", "input_text")?;
                map.serialize_entry("text", &super::StoredFile(&self.part["file"]).to_string())?;
            }
            Some("input_image") => {
                let Some((media_type, data)) =
                    image_input(self.part, "Responses").map_err(S::Error::custom)?
                else {
                    return Err(S::Error::custom(
                        "Responses image input changed after validation",
                    ));
                };
                map.serialize_entry("type", "input_image")?;
                map.serialize_entry("image_url", &ImageDataUrl(media_type, data))?;
                if let Some(detail) = self.part.get("detail") {
                    map.serialize_entry("detail", detail)?;
                }
            }
            _ => {
                for (key, value) in fields {
                    if !key.starts_with('_') {
                        map.serialize_entry(key, value)?;
                    }
                }
            }
        }
        map.end()
    }
}
