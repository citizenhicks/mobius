//! Prepare transport-owned image copies while preserving durable observation history.

use std::fmt::Write as _;
use std::sync::Arc;

use base64::Engine as _;
use serde_json::Value;

use super::ModelInput;
use crate::backend::session_files::SessionFileStore;
use crate::protocol::{ImageReference, content_parts, content_parts_mut};
use crate::{Error, Result};

/// Request admission policy, independent of stored-file and decoder limits.
#[derive(Debug, Clone, Copy)]
pub struct ImageInputLimits {
    /// The max images.
    pub max_images: usize,
    /// Encoded reference budget for image generation; conversation requests use the route byte limit.
    pub max_encoded_bytes: usize,
}

impl Default for ImageInputLimits {
    fn default() -> Self {
        Self {
            max_images: 64,
            max_encoded_bytes: 48 * 1024 * 1024,
        }
    }
}

/// Borrowed storage context; transports choose their logical input before reading pixels.
#[derive(Clone, Copy)]
pub struct MediaPreparation<'a> {
    pub(crate) files: Option<&'a SessionFileStore>,
    pub(crate) limits: ImageInputLimits,
}

impl std::fmt::Debug for MediaPreparation<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MediaPreparation")
            .field("files", &self.files.is_some())
            .field("limits", &self.limits)
            .finish()
    }
}

impl MediaPreparation<'_> {
    pub(super) async fn prepare<M: super::Model + ?Sized>(
        self,
        session_id: Option<&crate::SessionId>,
        input: ModelInput<'_>,
        model: &M,
        replay: bool,
    ) -> Result<Option<Vec<Arc<Value>>>> {
        let (historical_end, omittable_end) = if replay {
            let Some(newest_batch) = newest_observation_start(input) else {
                return Ok(None);
            };
            // A newer model output makes the last image batch historical too.
            let historical_end = input
                .suffix(newest_batch)
                .iter()
                .rposition(observation_boundary)
                .map_or(newest_batch, |offset| newest_batch + offset + 1);
            (historical_end, newest_batch)
        } else if input.iter().any(has_image) {
            (0, 0)
        } else {
            return Ok(None);
        };
        let session_id = session_id.ok_or_else(|| {
            Error::Provider("image observations require a session identity".into())
        })?;
        hydrate(
            self.files,
            session_id,
            input,
            model,
            historical_end,
            omittable_end,
            self.limits.max_images,
        )
        .await
        .map(Some)
    }
}

/// Removes historical images only from the prepared copy, measuring the complete wire body.
pub(super) fn bound_request(
    prepared: &mut [Arc<Value>],
    original: ModelInput<'_>,
    limits: ImageInputLimits,
    max_request_bytes: usize,
    replay: bool,
    mut size: impl FnMut(&[Arc<Value>]) -> Result<usize>,
) -> Result<()> {
    let fresh = if replay {
        newest_observation_start(original).unwrap_or(original.len())
    } else {
        0
    };
    let mut candidates = Vec::new();
    let mut images = 0;
    for (item_index, item) in prepared.iter().enumerate() {
        for (part_index, part) in content_parts(item).into_iter().flatten().enumerate() {
            if part.get("type").and_then(Value::as_str) == Some("input_image") {
                images += 1;
                let bytes = part.get("data").and_then(Value::as_str).map_or(0, str::len);
                if item_index < fresh {
                    candidates.push((item_index, part_index, bytes));
                }
            }
        }
    }
    let mut request_bytes = size(prepared)?;
    for (item_index, part_index, bytes) in candidates {
        if images <= limits.max_images && request_bytes <= max_request_bytes {
            return Ok(());
        }
        let file = original
            .get(item_index)
            .and_then(content_parts)
            .and_then(|parts| parts.get(part_index))
            .ok_or_else(|| Error::Provider("missing original observation".into()))?
            .pointer("/image/file")
            .unwrap_or(&Value::Null);
        // Replay demotion edits only the outgoing item; durable history keeps its original image.
        let part = prepared
            .get_mut(item_index)
            .and_then(|item| content_parts_mut(Arc::make_mut(item)))
            .and_then(|parts| parts.get_mut(part_index))
            .ok_or_else(|| Error::Provider("missing prepared observation".into()))?;
        replace_image(part, historical_image_omitted(file));
        images -= 1;
        request_bytes = request_bytes.saturating_sub(bytes);
        if images <= limits.max_images && request_bytes <= max_request_bytes {
            request_bytes = size(prepared)?;
        }
    }
    if images > limits.max_images || request_bytes > max_request_bytes {
        return Err(super::admission::over_budget());
    }
    Ok(())
}

fn historical_image_omitted(file: &Value) -> Value {
    serde_json::json!({"type":"input_text", "text":format!("[Historical image omitted to fit the request. Reopen the original session file: {file}]")})
}

fn has_image(item: &Value) -> bool {
    content_parts(item)
        .into_iter()
        .flatten()
        .any(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
}

/// Locates the batch whose newest image must keep its pixels when the request is bounded.
fn newest_observation_start(input: ModelInput<'_>) -> Option<usize> {
    let mut items = input.iter();
    // A fresh image often ends the transcript. Otherwise scan forward for the common text-only case.
    let last_image = if has_image(items.next_back()?) {
        items.len()
    } else {
        let first = items.position(has_image)?;
        items
            .rposition(has_image)
            .map_or(first, |last| first + 1 + last)
    };
    Some(
        input
            .prefix(last_image)
            .iter()
            .rposition(observation_boundary)
            .map_or(0, |index| index + 1),
    )
}

fn observation_boundary(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("assistant")
        || matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call" | "reasoning")
        )
}

pub(super) async fn hydrate<M: super::Model + ?Sized>(
    files: Option<&SessionFileStore>,
    session_id: &crate::SessionId,
    input: ModelInput<'_>,
    model: &M,
    historical_end: usize,
    omittable_end: usize,
    max_images: usize,
) -> Result<Vec<Arc<Value>>> {
    let mut prepared = input
        .iter()
        .enumerate()
        .map(|(index, item)| {
            input
                .shared_item(index)
                .map_or_else(|| Arc::new(item.clone()), Arc::clone) // rules-allow: hydration rewrites items, so borrowed values become owned and durable ones share their handle
        })
        .collect::<Vec<_>>();
    // Newest first, so the images past the cap are the ones `bound_request` would drop,
    // found before their pixels are read. Unreadable images never spend the cap.
    let mut read = 0;
    for (item_index, item) in prepared.iter_mut().enumerate().rev() {
        let tool_result = item.get("type").and_then(Value::as_str) == Some("function_call_output");
        if !content_parts(item).is_some_and(|parts| {
            parts
                .iter()
                .any(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
        }) {
            continue;
        }
        // Hydrated pixels belong only to the outgoing request, never the shared durable item.
        let Some(parts) = content_parts_mut(Arc::make_mut(item)) else {
            continue;
        };
        for part in parts.iter_mut().rev() {
            if part.get("type").and_then(Value::as_str) != Some("input_image") {
                continue;
            }
            let image: ImageReference =
                serde::Deserialize::deserialize(part.get("image").ok_or_else(|| {
                    Error::Provider("image observations require a durable reference".into())
                })?)?;
            if !model.supports_image_input() || (tool_result && !model.supports_tool_image_input())
            {
                replace_image(
                    part,
                    serde_json::json!({"type":"input_text", "text":format!("[Image omitted because the selected model cannot read images here. Session file: {}]", image.file.id)}),
                );
                continue;
            }
            if read >= max_images && item_index < omittable_end {
                let omitted =
                    historical_image_omitted(part.pointer("/image/file").unwrap_or(&Value::Null));
                replace_image(part, omitted);
                continue;
            }
            let files = files.ok_or_else(|| {
                Error::Internal(
                    "model router requires session file storage for image observations".into(),
                )
            })?;
            let replacement = async {
                let rendition = files.prepare_image(session_id, &image).await?;
                let file = &rendition.file;
                let bytes = files.read_file(session_id, file).await?;
                Ok(serde_json::json!({
                    "type": "input_image", "media_type": file.media_type,
                    "data": base64::engine::general_purpose::STANDARD.encode(bytes), "detail": image.detail,
                }))
            }
            .await;
            let replacement = match replacement {
                Ok(part) => {
                    read += 1;
                    part
                }
                Err(Error::Io(error))
                    if item_index < historical_end
                        && error.kind() == std::io::ErrorKind::NotFound =>
                {
                    if !files.file_is_missing(session_id, &image.file.id).await? {
                        return Err(Error::Io(error));
                    }
                    serde_json::json!({"type":"input_text", "text":format!("[Historical image is unavailable because the stored file was deleted or is missing. Original session file: {}]", image.file.id)})
                }
                Err(error) => return Err(error),
            };
            replace_image(part, replacement);
        }
    }
    Ok(prepared)
}

fn replace_image(part: &mut Value, replacement: Value) {
    let breakpoint = part
        .as_object_mut()
        .and_then(|part| part.remove(super::PROMPT_CACHE_BREAKPOINT_FIELD));
    *part = replacement;
    if let Some(breakpoint) = breakpoint {
        part[super::PROMPT_CACHE_BREAKPOINT_FIELD] = breakpoint;
    }
}

/// Extracts readable text from typed tool results for text-only consumers.
pub(crate) fn output_text(value: &Value) -> Result<String> {
    let parts = value
        .as_array()
        .ok_or_else(|| Error::Provider("tool result content must be an array".into()))?;
    let mut text = String::new();
    for (index, part) in parts.iter().enumerate() {
        if index != 0 {
            text.push('\n');
        }
        match part.get("type").and_then(Value::as_str) {
            Some("input_text") => text.push_str(
                part.get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Provider("invalid tool text".into()))?,
            ),
            Some("file") => {
                write!(text, "{}", super::StoredFile(&part["file"]))
                    .map_err(|error| Error::Provider(error.to_string().into()))?;
            }
            _ => {
                return Err(Error::Provider(
                    "selected provider cannot represent this tool observation".into(),
                ));
            }
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests;
