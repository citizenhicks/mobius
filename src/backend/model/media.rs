//! Prepare transport-owned image copies while preserving durable observation history.

use base64::Engine as _;
use serde_json::Value;

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
        input: &[Value],
        model: &M,
        replay: bool,
    ) -> Result<Vec<Value>> {
        let historical_end = if replay {
            input
                .iter()
                .rposition(observation_boundary)
                .map_or(0, |index| index + 1)
        } else {
            0
        };
        hydrate(self.files, session_id, input, model, historical_end).await
    }
}

/// Removes historical images only from the prepared copy, measuring the complete wire body.
pub(super) fn bound_request(
    prepared: &mut [Value],
    original: &[Value],
    limits: ImageInputLimits,
    max_request_bytes: usize,
    replay: bool,
    mut size: impl FnMut(&[Value]) -> Result<usize>,
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
        let part = &mut content_parts_mut(&mut prepared[item_index])
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

fn newest_observation_start(input: &[Value]) -> usize {
    let Some(last_image) = input.iter().rposition(|item| {
        content_parts(item)
            .into_iter()
            .flatten()
            .any(|part| part.get("type").and_then(Value::as_str) == Some("input_image"))
    }) else {
        return input.len();
    };
    input[..last_image]
        .iter()
        .rposition(observation_boundary)
        .map_or(0, |index| index + 1)
}

fn observation_boundary(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("assistant")
        || matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call" | "reasoning" | "compaction")
        )
}

pub(super) fn serialized_size(body: &Value) -> Result<usize> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("request size overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, body)?;
    Ok(counter.0)
}

pub(super) async fn hydrate<M: super::Model + ?Sized>(
    files: Option<&SessionFileStore>,
    session_id: &str,
    input: &[Value],
    model: &M,
    historical_end: usize,
) -> Result<Vec<Value>> {
    let mut prepared = input.to_vec();
    for (item_index, item) in prepared.iter_mut().enumerate() {
        let tool_result = item.get("type").and_then(Value::as_str) == Some("function_call_output");
        let Some(parts) = content_parts_mut(item) else {
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

pub(super) fn restore_references(
    output: &mut [Value],
    original: &[Value],
    prepared: &[Value],
) -> Result<()> {
    let images = original
        .iter()
        .zip(prepared)
        .flat_map(|(original, prepared)| {
            content_parts(original)
                .into_iter()
                .flatten()
                .zip(content_parts(prepared).into_iter().flatten())
        })
        .filter(|(original, _)| original.get("image").is_some())
        .collect::<Vec<_>>();
    for item in output {
        let Some(parts) = content_parts_mut(item) else {
            continue;
        };
        for part in parts {
            if part.get("type").and_then(Value::as_str) != Some("input_image") {
                continue;
            }
            let retained = images
                .iter()
                .find(|(original, prepared)| *original == part || *prepared == part)
                .or_else(|| {
                    images.iter().find(|(_, prepared)| {
                        prepared.get("data").is_some()
                            && prepared.get("data") == part.get("data")
                            && prepared.get("media_type") == part.get("media_type")
                            && prepared.get("detail") == part.get("detail")
                    })
                })
                .ok_or_else(|| {
                    Error::Provider("compaction returned an unrecognized image observation".into())
                })?;
            *part = retained.0.clone();
        }
    }
    Ok(())
}

/// Extracts readable text from typed tool results for text-only consumers.
pub(crate) fn output_text(value: &Value) -> Result<String> {
    let parts = value
        .as_array()
        .ok_or_else(|| Error::Provider("tool result content must be an array".into()))?;
    parts
        .iter()
        .map(|part| match part.get("type").and_then(Value::as_str) {
            Some("input_text") => part
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| Error::Provider("invalid tool text".into())),
            Some("file") => Ok(format!("Stored file: {}", part["file"])),
            _ => Err(Error::Provider(
                "selected provider cannot represent this tool observation".into(),
            )),
        })
        .collect::<Result<Vec<_>>>()
        .map(|text| text.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest};
    use crate::protocol::{ContentPart, ImageDetail, ToolContent};

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
        let measure = |input: &[Value]| {
            serialized_size(&serde_json::json!({"input":input,"tools":"schema".repeat(40)}))
        };
        let initial = measure(&prepared).expect("size");
        let unchanged = prepared.to_vec();
        bound_request(
            &mut prepared,
            &original,
            ImageInputLimits::default(),
            initial,
            true,
            measure,
        )
        .expect("exact limit");
        assert_eq!(prepared, unchanged);
        bound_request(
            &mut prepared,
            &original,
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
                &original,
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
                &original,
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
        let mut measurements = 0;
        bound_request(
            &mut prepared,
            &original,
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

    #[test]
    fn compaction_does_not_match_unknown_references_against_image_placeholders() {
        let original = [
            serde_json::json!({"role":"user","content":[{"type":"input_image","image":{"file":{"id":"old"}}}]}),
        ];
        let prepared = [
            serde_json::json!({"role":"user","content":[{"type":"input_text","text":"historical image omitted"}]}),
        ];
        let mut output = [
            serde_json::json!({"role":"user","content":[{"type":"input_image","image":{"file":{"id":"unknown"}}}]}),
        ];
        assert!(restore_references(&mut output, &original, &prepared).is_err());
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
        let mut prepared = hydrate(Some(&store), "photos", &original, &Vision, 0)
            .await
            .expect("prepare batch");
        let size = |input: &[Value]| {
            serialized_size(
                &serde_json::json!({"input":super::super::openai::wire_input_with_cache(input, true, true, "catalog", &[])?}),
            )
        };
        bound_request(
            &mut prepared,
            &original,
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
            .prepare("history", &input, &Vision, true)
            .await
            .expect("cold replay after purge");
        for part in [&prepared[0]["content"][0], &prepared[2]["output"][1]] {
            assert_eq!(part["type"], "input_text");
            assert!(
                part["text"]
                    .as_str()
                    .expect("unavailable")
                    .contains("unavailable")
            );
        }
        assert!(super::super::has_prompt_cache_breakpoint(&prepared));
        assert_eq!(prepared[2]["output"][0]["text"], "failed capture");
        assert_eq!(prepared[4]["content"][1]["type"], "input_image");
        assert_eq!(input[0]["content"][0]["type"], "input_image");
        assert_eq!(input[2]["output"][1]["type"], "input_image");
        assert!(
            media
                .prepare("history", &input, &Vision, false)
                .await
                .is_err()
        );
        assert!(media.prepare("other", &input, &Vision, true).await.is_err());

        let mut continued = input[..4].to_vec();
        continued.push(serde_json::json!({"role":"user","content":"continue without images"}));
        media
            .prepare("history", &continued, &Vision, true)
            .await
            .expect("purged prior batch does not poison later text turns");
        continued.push(
            serde_json::json!({"role":"user","content":[{"type":"input_image","image":old}]}),
        );
        assert!(
            media
                .prepare("history", &continued, &Vision, true)
                .await
                .is_err()
        );

        let mut invalid = input.to_vec();
        invalid[0]["content"][0]["image"] = serde_json::to_value(fresh).expect("image reference");
        invalid[0]["content"][0]["image"]["file"]["size"] = serde_json::json!(1);
        assert!(matches!(
            media.prepare("history", &invalid, &Vision, true).await,
            Err(Error::Tool(_))
        ));
    }

    #[tokio::test]
    async fn large_native_observations_keep_exact_prefix_and_durable_compaction_references() {
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
        let mut input = vec![super::super::tool_output("call", content.clone(), true)];
        super::super::reset_prompt_cache_breakpoint(&mut input);
        let prepared = hydrate(Some(&store), "parent", &input, &Vision, 0)
            .await
            .expect("large images admitted");
        assert_eq!(prepared[0]["output"][0]["text"], "before");
        assert_eq!(prepared[0]["output"][2]["text"], "after");
        assert_eq!(prepared[0]["output"][1]["detail"], "high");
        assert!(super::super::has_prompt_cache_breakpoint(&prepared));
        let wired =
            super::super::openai::wire_input_with_cache(&prepared, true, true, "catalog", &[])
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
        input.push(super::super::tool_output(
            "next",
            ToolContent(vec![ContentPart::Image {
                image: observation.clone(),
            }]),
            false,
        ));
        let appended = hydrate(Some(&store), "parent", &input, &Vision, 0)
            .await
            .expect("append");
        assert_eq!(prepared, appended[..1]);
        let mut compacted = prepared.clone();
        restore_references(&mut compacted, &input[..1], &prepared).expect("restore");
        assert_eq!(compacted, input[..1]);
        let mut unknown = prepared.clone();
        unknown[0]["output"][1]["data"] = Value::String("unknown".into());
        assert!(restore_references(&mut unknown, &input[..1], &prepared).is_err());
        assert!(
            hydrate(Some(&store), "other", &input, &Vision, 0)
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
            hydrate(Some(&store), "child", &input, &Vision, 0)
                .await
                .expect("fork retains blobs"),
            appended
        );
    }
}
