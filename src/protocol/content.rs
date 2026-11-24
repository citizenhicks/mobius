//! Ordered conversation content shared by tools, replay, and frontends.

use std::{fmt, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::SessionFileReference;

/// Model presentation chosen when an image is admitted to durable history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageDetail {
    #[default]
    /// Selects the auto case.
    Auto,
    /// Selects the low case.
    Low,
    /// Selects the high case.
    High,
}

/// One validated immutable image. Dimensions describe the stored pixels.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageReference {
    /// The file.
    pub file: SessionFileReference,
    /// The width.
    pub width: u32,
    /// The height.
    pub height: u32,
    /// The detail.
    pub detail: ImageDetail,
    /// Model-only pixels; the original file remains the download and editing source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendition: Option<ImageRendition>,
}

/// Prepared model pixels stored under the same session authorization as the original.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRendition {
    /// Prepared immutable file.
    pub file: SessionFileReference,
    /// Prepared pixel width.
    pub width: u32,
    /// Prepared pixel height.
    pub height: u32,
}

/// One ordered observation. Files are references, not implicit model attachments.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ContentPart {
    #[serde(rename = "input_text")]
    /// A text segment.
    Text {
        /// The text content.
        text: String,
    },
    #[serde(rename = "input_image")]
    /// An image attachment.
    Image {
        /// The image reference.
        image: ImageReference,
    },
    #[serde(rename = "file")]
    /// A file attachment.
    File {
        /// The file reference.
        file: SessionFileReference,
    },
}

/// Authoritative ordered content; text previews are explicitly lossy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// The outcome of starting a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolResponse {
    /// The call finished, including tool-reported failures.
    Completed {
        /// Observations for the model.
        content: ToolContent,
        /// Whether the operation failed.
        is_error: bool,
    },
    /// The tool accepted the operation. Its result will arrive through the message queue.
    Deferred(DeferredToolCall),
}

/// Providers can reuse a call ID on a later model step.
pub(crate) fn scoped_call_id<'a>(step: &'a str, call: &'a str) -> impl fmt::Display + 'a {
    fmt::from_fn(move |f| write!(f, "{step}/{call}"))
}

/// Model-visible reference of a deferred invocation. Existing question records store it as a string.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct DeferredToolCall(pub(crate) Arc<str>);

impl DeferredToolCall {
    pub(crate) fn new(step: Option<&str>, call: &Arc<str>) -> Self {
        Self(match step {
            Some(step) => scoped_call_id(step, call).to_string().into(),
            None => Arc::clone(call),
        })
    }

    /// The same reference is used by the pending receipt, the UI action and the later result.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
impl ToolResponse {
    pub(crate) fn completed_content(&self) -> &ToolContent {
        match self {
            Self::Completed { content, .. } => content,
            Self::Deferred(_) => panic!("expected a completed tool response"),
        }
    }
}

impl From<String> for ToolResponse {
    fn from(text: String) -> Self {
        Self::Completed {
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
