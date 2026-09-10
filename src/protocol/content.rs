//! Ordered conversation content shared by tools, replay, and frontends.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::SessionFileReference;

/// Model presentation chosen when an image is admitted to durable history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageDetail {
    #[default]
    Auto,
    Low,
    High,
}

/// One validated immutable image. Dimensions describe the stored pixels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageReference {
    pub file: SessionFileReference,
    pub width: u32,
    pub height: u32,
    pub detail: ImageDetail,
}

/// One ordered observation. Files are references, not implicit model attachments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ContentPart {
    #[serde(rename = "input_text")]
    Text { text: String },
    #[serde(rename = "input_image")]
    Image { image: ImageReference },
    #[serde(rename = "file")]
    File { file: SessionFileReference },
}

/// Authoritative ordered content; text previews are explicitly lossy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolContent(pub Vec<ContentPart>);

impl ToolContent {
    /// Returns text for textual consumers without serializing image data.
    #[must_use]
    pub fn text(&self) -> String {
        self.0
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                ContentPart::Image { .. } | ContentPart::File { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Returns referenced files in observation order.
    pub fn files(&self) -> impl Iterator<Item = &SessionFileReference> {
        self.0.iter().filter_map(|part| match part {
            ContentPart::Image { image } => Some(&image.file),
            ContentPart::File { file } => Some(file),
            ContentPart::Text { .. } => None,
        })
    }
}

impl From<String> for ToolContent {
    fn from(text: String) -> Self {
        Self(vec![ContentPart::Text { text }])
    }
}

impl From<&str> for ToolContent {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

impl From<&String> for ToolContent {
    fn from(text: &String) -> Self {
        text.as_str().into()
    }
}

impl From<&ToolContent> for ToolContent {
    fn from(content: &ToolContent) -> Self {
        content.clone()
    }
}

/// A completed tool observation, including observations from failed actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResponse {
    pub content: ToolContent,
    pub is_error: bool,
}

impl From<String> for ToolResponse {
    fn from(text: String) -> Self {
        Self {
            content: text.into(),
            is_error: false,
        }
    }
}

impl From<&str> for ToolResponse {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

pub(crate) fn content_parts(item: &Value) -> Option<&Vec<Value>> {
    item.get(content_field(item)).and_then(Value::as_array)
}

pub(crate) fn content_parts_mut(item: &mut Value) -> Option<&mut Vec<Value>> {
    let field = content_field(item);
    item.get_mut(field).and_then(Value::as_array_mut)
}

fn content_field(item: &Value) -> &'static str {
    if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
        "output"
    } else {
        "content"
    }
}

/// Readable durable evidence, including references that can be reopened after compaction.
pub(crate) fn content_part_text(part: &Value) -> Option<String> {
    if let Some(text) = part
        .get("text")
        .or_else(|| part.get("content"))
        .and_then(Value::as_str)
    {
        return Some(text.to_owned());
    }
    match part.get("type").and_then(Value::as_str) {
        Some("input_image") => part.get("image").map(|image| format!("Image: {image}")),
        Some("file") => part.get("file").map(|file| format!("Stored file: {file}")),
        _ => None,
    }
}
