//! Resolve immutable observations without changing selection or retention policy.

use base64::Engine as _;
use serde_json::Value;

use crate::backend::session_files::SessionFileStore;
use crate::protocol::{ImageReference, content_parts, content_parts_mut};
use crate::{Error, Result};

/// Request admission policy, independent of stored-file and decoder limits.
#[derive(Debug, Clone, Copy)]
pub struct ImageInputLimits {
    pub max_images: usize,
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

pub(super) async fn hydrate(
    files: Option<&SessionFileStore>,
    session_id: &str,
    input: &[Value],
    model: &dyn super::Model,
    limits: ImageInputLimits,
) -> Result<Vec<Value>> {
    let mut prepared = input.to_vec();
    let mut images = 0usize;
    let mut encoded = 0usize;
    for item in &mut prepared {
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
            images = images.saturating_add(1);
            let image: ImageReference =
                serde_json::from_value(part.get("image").cloned().ok_or_else(|| {
                    Error::Provider("image observations require a durable reference".into())
                })?)?;
            let size = usize::try_from(image.file.size)
                .ok()
                .and_then(|bytes| bytes.checked_add(2))
                .and_then(|bytes| bytes.checked_div(3))
                .and_then(|bytes| bytes.checked_mul(4));
            encoded = encoded
                .checked_add(
                    size.ok_or_else(|| Error::Provider("invalid image observation size".into()))?,
                )
                .ok_or_else(|| Error::Provider("image request size overflow".into()))?;
            if images > limits.max_images || encoded > limits.max_encoded_bytes {
                return Err(Error::Provider("image observations exceed the configured request budget; compact context or select a suitable model".into()));
            }
            let files = files.ok_or_else(|| {
                Error::Config(
                    "model router requires session file storage for image observations".into(),
                )
            })?;
            let bytes = files.read_file(session_id, &image.file).await?;
            let breakpoint = part.get(super::PROMPT_CACHE_BREAKPOINT_FIELD).cloned();
            *part = serde_json::json!({
                "type": "input_image", "media_type": image.file.media_type,
                "data": base64::engine::general_purpose::STANDARD.encode(bytes), "detail": image.detail,
            });
            if let Some(breakpoint) = breakpoint {
                part[super::PROMPT_CACHE_BREAKPOINT_FIELD] = breakpoint;
            }
        }
    }
    Ok(prepared)
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
                        prepared.get("data") == part.get("data")
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
    async fn large_native_observations_keep_exact_prefix_and_durable_compaction_references() {
        use image::ImageEncoder as _;
        let state = tempfile::tempdir().expect("state");
        let store = SessionFileStore::new(state.path());
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
        let prepared = hydrate(
            Some(&store),
            "parent",
            &input,
            &Vision,
            ImageInputLimits::default(),
        )
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
                .starts_with("data:image/png;base64,")
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
        let appended = hydrate(
            Some(&store),
            "parent",
            &input,
            &Vision,
            ImageInputLimits::default(),
        )
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
            hydrate(
                Some(&store),
                "parent",
                &input,
                &Vision,
                ImageInputLimits {
                    max_images: 1,
                    ..ImageInputLimits::default()
                }
            )
            .await
            .is_err()
        );
        assert!(
            hydrate(
                Some(&store),
                "other",
                &input,
                &Vision,
                ImageInputLimits::default()
            )
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
            hydrate(
                Some(&store),
                "child",
                &input,
                &Vision,
                ImageInputLimits::default()
            )
            .await
            .expect("fork retains blobs"),
            appended
        );
    }
}
