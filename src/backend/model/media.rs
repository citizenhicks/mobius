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

impl MediaPreparation<'_> {
    pub(super) async fn prepare<M: super::Model + ?Sized>(
        self,
        session_id: &str,
        input: ModelInput<'_>,
        model: &M,
        replay: bool,
    ) -> Result<Option<Vec<Arc<Value>>>> {
        if !input.iter().any(|item| {
            content_parts(item)
                .into_iter()
                .flatten()
                .any(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
        }) {
            return Ok(None);
        }
        let historical_end = if replay {
            input
                .iter()
                .rposition(observation_boundary)
                .map_or(0, |index| index + 1)
        } else {
            0
        };
        hydrate(self.files, session_id, input, model, historical_end)
            .await
            .map(Some)
    }
}

/// Serializes the admitted transport body, retaining the exact bytes that passed admission.
/// Providers own encoding; continuation transports pass only their selected logical suffix.
pub(super) async fn encode_request<M: super::Model + ?Sized>(
    model: &M,
    request: super::ModelRequest<'_>,
    media: Option<MediaPreparation<'_>>,
    replay: bool,
    mut encode: impl FnMut(ModelInput<'_>) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut prepared = match media {
        Some(media) => {
            media
                .prepare(request.session_id, request.input, model, replay)
                .await?
        }
        None => None,
    };
    let max_bytes = model.transport_settings().max_request_bytes;
    let (Some(input), Some(media)) = (prepared.as_mut(), media) else {
        let body = encode(request.input)?;
        check_request_bytes(body.len(), max_bytes)?;
        return Ok(body);
    };
    let mut body = Vec::new();
    bound_request(
        input,
        request.input,
        media.limits,
        max_bytes,
        replay,
        |input| {
            // A demoted replay cannot reuse the old bytes; release them before re-encoding.
            drop(std::mem::take(&mut body));
            body = encode(input.into())?;
            Ok(body.len())
        },
    )?;
    Ok(body)
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
        newest_observation_start(original)
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
        let original_part = &content_parts(&original[item_index])
            .ok_or_else(|| Error::Provider("missing original observation".into()))?[part_index];
        let file = &original_part["image"]["file"];
        // Replay demotion edits only the outgoing item; durable history keeps its original image.
        let part = &mut content_parts_mut(Arc::make_mut(&mut prepared[item_index]))
            .ok_or_else(|| Error::Provider("missing prepared observation".into()))?[part_index];
        replace_image(
            part,
            serde_json::json!({"type":"input_text", "text":format!("[Historical image omitted to fit the request. Reopen the original session file: {file}]")}),
        );
        images -= 1;
        request_bytes = request_bytes.saturating_sub(bytes);
        if images <= limits.max_images && request_bytes <= max_request_bytes {
            request_bytes = size(prepared)?;
        }
    }
    if images > limits.max_images || request_bytes > max_request_bytes {
        return Err(Error::Provider("the newest observation batch and retained text exceed the model request budget; use fewer or smaller images, or reduce the text before retrying".into()));
    }
    Ok(())
}

fn newest_observation_start(input: ModelInput<'_>) -> usize {
    let Some(last_image) = input.iter().rposition(|item| {
        content_parts(item)
            .into_iter()
            .flatten()
            .any(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
    }) else {
        return input.len();
    };
    input
        .prefix(last_image)
        .iter()
        .rposition(observation_boundary)
        .map_or(0, |index| index + 1)
}

fn observation_boundary(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("assistant")
        || matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call" | "reasoning")
        )
}

pub(super) use crate::serialized_len as serialized_size;

pub(super) fn check_request_bytes(bytes: usize, max_bytes: usize) -> Result<()> {
    if bytes > max_bytes {
        return Err(Error::Provider("the newest observation batch and retained text exceed the model request budget; use fewer or smaller images, or reduce the text before retrying".into()));
    }
    Ok(())
}

pub(super) async fn hydrate<M: super::Model + ?Sized>(
    files: Option<&SessionFileStore>,
    session_id: &str,
    input: ModelInput<'_>,
    model: &M,
    historical_end: usize,
) -> Result<Vec<Arc<Value>>> {
    let mut prepared = input
        .iter()
        .enumerate()
        .map(|(index, item)| {
            input
                .shared_item(index)
                .map_or_else(|| Arc::new(item.clone()), Arc::clone)
        })
        .collect::<Vec<_>>();
    for (item_index, item) in prepared.iter_mut().enumerate() {
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
        for part in parts {
            if part.get("type").and_then(Value::as_str) != Some("input_image") {
                continue;
            }
            if !model.supports_image_input() || (tool_result && !model.supports_tool_image_input())
            {
                return Err(Error::Provider(
                    "selected model does not support this image observation".into(),
                ));
            }
            let image: ImageReference =
                serde::Deserialize::deserialize(part.get("image").ok_or_else(|| {
                    Error::Provider("image observations require a durable reference".into())
                })?)?;
            let files = files.ok_or_else(|| {
                Error::Config(
                    "model router requires session file storage for image observations".into(),
                )
            })?;
            let replacement = async {
                let rendition = files.prepare_image(session_id, &image).await?;
                let file = rendition
                    .as_ref()
                    .map_or(&image.file, |rendition| &rendition.file);
                let bytes = files.read_file(session_id, file).await?;
                Ok(serde_json::json!({
                    "type": "input_image", "media_type": file.media_type,
                    "data": base64::engine::general_purpose::STANDARD.encode(bytes), "detail": image.detail,
                }))
            }
            .await;
            let replacement = match replacement {
                Ok(part) => part,
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
            Some("file") => write!(text, "Stored file: {}", part["file"])
                .expect("writing to a String cannot fail"),
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
mod tests {
    use super::*;
    use crate::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest};
    use crate::protocol::{ContentPart, ImageDetail, ToolContent};

    #[test]
    fn tool_text_keeps_empty_parts_and_file_references() {
        let parts = serde_json::json!([
            {"type": "input_text", "text": ""},
            {"type": "input_text", "text": "hello"},
            {"type": "file", "file": {"id": "reference"}},
            {"type": "input_text", "text": ""}
        ]);
        assert_eq!(
            output_text(&parts).expect("tool text"),
            "\nhello\nStored file: {\"id\":\"reference\"}\n"
        );
        assert!(output_text(&serde_json::json!([{"type": "input_image"}])).is_err());
    }

    #[tokio::test]
    async fn text_requests_keep_borrowed_history_without_materialization() {
        let input = [super::super::user_message("plain text")];
        let media = MediaPreparation {
            files: None,
            limits: ImageInputLimits::default(),
        };
        assert!(
            media
                .prepare("text", (&input).into(), &Vision, true)
                .await
                .expect("borrowed text request")
                .is_none()
        );
    }

    #[tokio::test]
    async fn admitted_bytes_are_encoded_once_and_returned_without_copying() {
        let input = [super::super::user_message("plain text")];
        for media in [
            None,
            Some(MediaPreparation {
                files: None,
                limits: ImageInputLimits::default(),
            }),
        ] {
            let request = ModelRequest {
                session_id: "single-encoding",
                cancellation: None,
                prompt_cache: None,
                instructions: "instructions",
                input: (&input).into(),
                catalog_revision: "catalog",
                tools: &[],
                deferred_tools: &[],
                allow_hosted_tools: false,
                allow_continuation: false,
            };
            let mut count = 0;
            let mut allocation = 0;
            let body = encode_request(&Vision, request, media, true, |input| {
                count += 1;
                let bytes = serde_json::to_vec(&input)?;
                allocation = bytes.as_ptr() as usize;
                Ok(bytes)
            })
            .await
            .expect("admitted request");
            assert_eq!(count, 1);
            assert_eq!(body.as_ptr() as usize, allocation);
            assert_eq!(
                serde_json::from_slice::<Value>(&body).expect("body"),
                serde_json::json!(input)
            );
        }
    }

    #[test]
    fn replay_drops_only_old_images_after_measuring_full_body_and_keeps_fresh_batch() {
        let original = vec![
            serde_json::json!({"role":"user", "content":[{"type":"input_text","text":"keep text"},{"type":"input_image","image":{"file":{"id":"old"}}}]}),
            serde_json::json!({"type":"function_call","call_id":"a"}),
            serde_json::json!({"type":"function_call","call_id":"b"}),
            serde_json::json!({"type":"function_call_output","call_id":"a","output":[{"type":"input_image","image":{"file":{"id":"fresh-a"}}}]}),
            serde_json::json!({"type":"function_call_output","call_id":"b","output":[{"type":"input_text","text":"result text"},{"type":"input_image","image":{"file":{"id":"fresh-b"}}}]}),
        ];
        let mut prepared = original.to_vec();
        for part in prepared.iter_mut().filter_map(content_parts_mut).flatten() {
            if part.get("image").is_some() {
                *part = serde_json::json!({"type":"input_image", "data":"x".repeat(1000)});
            }
        }
        let mut prepared = prepared.into_iter().map(Arc::new).collect::<Vec<_>>();
        let measure = |input: &[Arc<Value>]| {
            serialized_size(&serde_json::json!({"input":input,"tools":"schema".repeat(40)}))
        };
        let initial = measure(&prepared).expect("size");
        let unchanged = prepared.to_vec();
        bound_request(
            &mut prepared,
            (&original).into(),
            ImageInputLimits::default(),
            initial,
            true,
            measure,
        )
        .expect("exact limit");
        assert_eq!(prepared, unchanged);
        bound_request(
            &mut prepared,
            (&original).into(),
            ImageInputLimits::default(),
            initial - 500,
            true,
            measure,
        )
        .expect("strip oldest");
        assert_eq!(prepared[0]["content"][0]["text"], "keep text");
        assert!(
            prepared[0]["content"][1]["text"]
                .as_str()
                .expect("placeholder")
                .contains("old")
        );
        assert_eq!(prepared[1..], unchanged[1..]);
        assert!(
            bound_request(
                &mut prepared,
                (&original).into(),
                ImageInputLimits::default(),
                1000,
                true,
                measure
            )
            .is_err()
        );
        assert_eq!(prepared[1..], unchanged[1..]);
        let mut suffix = unchanged;
        assert!(
            bound_request(
                &mut suffix,
                (&original).into(),
                ImageInputLimits::default(),
                initial - 1,
                false,
                measure
            )
            .is_err()
        );
        assert_eq!(suffix[0]["content"][1]["type"], "input_image");
    }

    #[test]
    fn replay_remeasures_only_when_removed_bytes_could_fit() {
        let old = (0..10).map(|index| serde_json::json!({"type":"input_image", "image":{"file":{"id":format!("old-{index}")}}})).collect::<Vec<_>>();
        let original = vec![
            serde_json::json!({"role":"user","content":old}),
            serde_json::json!({"role":"assistant","content":"observed"}),
            serde_json::json!({"role":"user","content":[{"type":"input_image","image":{"file":{"id":"fresh"}}}]}),
        ];
        let mut prepared = original.to_vec();
        for part in prepared.iter_mut().filter_map(content_parts_mut).flatten() {
            *part = serde_json::json!({"type":"input_image", "data":"x".repeat(10_000)});
        }
        let mut prepared = prepared.into_iter().map(Arc::new).collect::<Vec<_>>();
        let mut measurements = 0;
        bound_request(
            &mut prepared,
            (&original).into(),
            ImageInputLimits {
                max_encoded_bytes: 1,
                ..ImageInputLimits::default()
            },
            20_000,
            true,
            |input| {
                measurements += 1;
                serialized_size(&serde_json::json!({"input":input}))
            },
        )
        .expect("bounded replay");
        assert_eq!(measurements, 2);
        assert_eq!(
            prepared[2]["content"][0]["data"]
                .as_str()
                .expect("fresh pixels")
                .len(),
            10_000
        );
    }

    #[tokio::test]
    async fn sixteen_large_fitted_photo_pngs_use_smaller_jpegs_without_losing_originals() {
        let state = tempfile::tempdir().expect("state");
        let store = SessionFileStore::new(state.path(), None);
        let (width, height) = (1835, 1376);
        let mut random = 1_u32;
        let photo = image::RgbImage::from_fn(width, height, |x, y| {
            let base = [
                x * 192 / width,
                y * 192 / height,
                (x + y) * 192 / (width + height),
            ];
            image::Rgb(std::array::from_fn(|channel| {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                u8::try_from(base[channel] + (random & 31)).expect("bounded channel")
            }))
        });
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(photo)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("PNG");
        let bytes = png.into_inner();
        assert!(bytes.len().div_ceil(3) * 4 * 16 > 24 * 1024 * 1024);
        let observation = store
            .ingest_image(
                "photos",
                "photo.png".into(),
                bytes.to_vec(),
                ImageDetail::High,
            )
            .await
            .expect("photo");
        let rendition = observation.rendition.as_ref().expect("same-size rendition");
        assert_eq!((rendition.width, rendition.height), (width, height));
        assert_eq!(rendition.file.media_type, "image/jpeg");
        assert_eq!(
            store
                .read_file("photos", &observation.file)
                .await
                .expect("original PNG"),
            bytes
        );
        let parts = (0..16)
            .map(|_| serde_json::json!({"type":"input_image", "image":observation}))
            .collect::<Vec<_>>();
        let original = [
            serde_json::json!({"type":"function_call_output", "call_id":"photos", "output":parts}),
        ];
        let mut prepared = hydrate(Some(&store), "photos", (&original).into(), &Vision, 0)
            .await
            .expect("prepare batch");
        let size = |input: &[Arc<Value>]| {
            serialized_size(
                &serde_json::json!({"input":super::super::responses::wire_input_with_cache((input).into(), true, true, "catalog", &[])?}),
            )
        };
        bound_request(
            &mut prepared,
            (&original).into(),
            ImageInputLimits::default(),
            24 * 1024 * 1024,
            true,
            size,
        )
        .expect("all sixteen photos fit");
        assert_eq!(
            prepared[0]["output"]
                .as_array()
                .expect("complete batch")
                .len(),
            16
        );
        assert!(
            prepared[0]["output"]
                .as_array()
                .expect("images")
                .iter()
                .all(|part| part["type"] == "input_image")
        );
        eprintln!(
            "16-photo request: {} bytes (original PNG base64: {} bytes)",
            size(&prepared).expect("request bytes"),
            bytes.len().div_ceil(3) * 4 * 16
        );
    }

    struct Vision;
    impl Model for Vision {
        fn supports_image_input(&self) -> bool {
            true
        }
        fn supports_tool_image_input(&self) -> bool {
            true
        }
        fn respond<'a>(
            &'a self,
            _: ModelRequest<'a>,
            _: ModelEventSink,
        ) -> crate::BoxFuture<'a, Result<ModelOutput>> {
            Box::pin(async { Err(Error::Provider("unused transport".into())) })
        }
    }

    #[tokio::test]
    async fn replay_replaces_deleted_history_but_keeps_fresh_and_invalid_images_strict() {
        let state = tempfile::tempdir().expect("state");
        let store = SessionFileStore::new(state.path(), None);
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("PNG");
        let bytes = bytes.into_inner();
        let old = store
            .ingest_image(
                "history",
                "old.png".into(),
                bytes.to_vec(),
                ImageDetail::High,
            )
            .await
            .expect("old image");
        store
            .delete_session("history")
            .await
            .expect("purge old image");
        let fresh = store
            .ingest_image("history", "fresh.png".into(), bytes, ImageDetail::High)
            .await
            .expect("fresh image");
        let input = vec![
            serde_json::json!({"role":"user","content":[{"type":"input_image","image":old}]}),
            serde_json::json!({"type":"function_call","call_id":"capture"}),
            serde_json::json!({"type":"function_call_output","call_id":"capture", super::super::TOOL_ERROR_FIELD:true, "output":[{"type":"input_text","text":"failed capture"},{"type":"input_image","image":old, super::super::PROMPT_CACHE_BREAKPOINT_FIELD:true}]}),
            serde_json::json!({"role":"assistant","content":"observed"}),
            serde_json::json!({"role":"user","content":[{"type":"input_text","text":"new image"},{"type":"input_image","image":fresh}]}),
        ];
        let media = MediaPreparation {
            files: Some(&store),
            limits: ImageInputLimits::default(),
        };
        let prepared = media
            .prepare("history", (&input).into(), &Vision, true)
            .await
            .expect("cold replay after purge")
            .expect("image materialization");
        for part in [&prepared[0]["content"][0], &prepared[2]["output"][1]] {
            assert_eq!(part["type"], "input_text");
            assert!(
                part["text"]
                    .as_str()
                    .expect("unavailable")
                    .contains("unavailable")
            );
        }
        assert!(super::super::has_prompt_cache_breakpoint(
            (&prepared).into()
        ));
        assert_eq!(prepared[2]["output"][0]["text"], "failed capture");
        assert_eq!(prepared[4]["content"][1]["type"], "input_image");
        assert_eq!(input[0]["content"][0]["type"], "input_image");
        assert_eq!(input[2]["output"][1]["type"], "input_image");
        assert!(
            media
                .prepare("history", (&input).into(), &Vision, false)
                .await
                .is_err()
        );
        assert!(
            media
                .prepare("other", (&input).into(), &Vision, true)
                .await
                .is_err()
        );

        let mut continued = input[..4].to_vec();
        continued.push(serde_json::json!({"role":"user","content":"continue without images"}));
        media
            .prepare("history", (&continued).into(), &Vision, true)
            .await
            .expect("purged prior batch does not poison later text turns");
        continued.push(
            serde_json::json!({"role":"user","content":[{"type":"input_image","image":old}]}),
        );
        assert!(
            media
                .prepare("history", (&continued).into(), &Vision, true)
                .await
                .is_err()
        );

        let mut invalid = input.to_vec();
        invalid[0]["content"][0]["image"] = serde_json::to_value(fresh).expect("image reference");
        invalid[0]["content"][0]["image"]["file"]["size"] = serde_json::json!(1);
        assert!(matches!(
            media
                .prepare("history", (&invalid).into(), &Vision, true)
                .await,
            Err(Error::Tool(_))
        ));
    }

    #[tokio::test]
    async fn large_native_observations_keep_exact_prefix_and_durable_file_references() {
        use image::ImageEncoder as _;
        let state = tempfile::tempdir().expect("state");
        let store = SessionFileStore::new(state.path(), None);
        let mut random = 1_u32;
        let pixels = (0..1800 * 1800 * 3)
            .map(|_| {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                random as u8
            })
            .collect::<Vec<_>>();
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&pixels, 1800, 1800, image::ExtendedColorType::Rgb8)
            .expect("PNG");
        assert!(bytes.len() > 8 * 1024 * 1024);
        let observation = store
            .ingest_image("parent", "screen.png".into(), bytes, ImageDetail::High)
            .await
            .expect("image");
        let content = ToolContent(vec![
            ContentPart::Text {
                text: "before".into(),
            },
            ContentPart::Image {
                image: observation.clone(),
            },
            ContentPart::Text {
                text: "after".into(),
            },
            ContentPart::Image {
                image: observation.clone(),
            },
        ]);
        let mut input = vec![super::super::tool_output("call", &content, true)];
        super::super::reset_prompt_cache_breakpoint(&mut input);
        input.push(super::super::user_message("retained text beside the image"));
        let mut input = input.into_iter().map(Arc::new).collect::<Vec<_>>();
        let prepared = hydrate(Some(&store), "parent", (&input).into(), &Vision, 0)
            .await
            .expect("large images admitted");
        assert!(Arc::ptr_eq(&prepared[1], &input[1]));
        assert!(!Arc::ptr_eq(&prepared[0], &input[0]));
        assert!(input[0]["output"][1].get("image").is_some());
        assert_eq!(prepared[0]["output"][0]["text"], "before");
        assert_eq!(prepared[0]["output"][2]["text"], "after");
        assert_eq!(prepared[0]["output"][1]["detail"], "high");
        assert!(super::super::has_prompt_cache_breakpoint(
            (&prepared).into()
        ));
        let wired = super::super::responses::wire_input_with_cache(
            (&prepared).into(),
            true,
            true,
            "catalog",
            &[],
        )
        .expect("native result");
        assert!(
            wired[0]["output"][1]["image_url"]
                .as_str()
                .expect("image url")
                .starts_with("data:image/jpeg;base64,")
        );
        assert!(
            wired[0]["output"][3]
                .get("prompt_cache_breakpoint")
                .is_some()
        );
        input.push(Arc::new(super::super::tool_output(
            "next",
            &ToolContent(vec![ContentPart::Image {
                image: observation.clone(),
            }]),
            false,
        )));
        let appended = hydrate(Some(&store), "parent", (&input).into(), &Vision, 0)
            .await
            .expect("append");
        assert_eq!(prepared, appended[..2]);
        assert!(
            hydrate(Some(&store), "other", (&input).into(), &Vision, 0)
                .await
                .is_err()
        );
        crate::backend::session_files::grant_context(Some(&store), "parent", "child", &input)
            .await
            .expect("fork grants");
        let published = store
            .publish_reference("parent", &observation.file)
            .await
            .expect("publish existing observation");
        assert_eq!(
            store
                .read_file("parent", &published)
                .await
                .expect("published bytes"),
            store
                .read_file("parent", &observation.file)
                .await
                .expect("original bytes")
        );
        store.delete_session("parent").await.expect("delete parent");
        assert_eq!(
            hydrate(Some(&store), "child", (&input).into(), &Vision, 0)
                .await
                .expect("fork retains blobs"),
            appended
        );
    }
}
