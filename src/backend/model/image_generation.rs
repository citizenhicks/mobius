//! Provider-neutral image requests and the two native Images API wire shapes.

use base64::Engine as _;
use image::ImageFormat;
use serde_json::{Value, json};

use super::{ImageInputLimits, image_data_url, usage_i64, validate_usage};
use crate::protocol::{ImageAspect, TokenUsage};
use crate::{Error, Result};

const MAX_PROMPT_CHARS: usize = 32_000;
const MAX_REFERENCES: usize = 16;
const MAX_REFERENCE_URL_BYTES: usize = 20 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 48 * 1024 * 1024;

/// One image supplied to an image edit.
#[derive(Debug)]
pub struct ImageGenerationReference<'a> {
    /// Image MIME type.
    pub media_type: &'a str,
    /// Original image bytes.
    pub bytes: &'a [u8],
}

/// Input for native image generation or an edit when references are present.
#[derive(Debug)]
pub struct ImageGenerationRequest<'a> {
    /// The requested image or edit.
    pub prompt: &'a str,
    /// Explicit output shape.
    pub image_aspect: ImageAspect,
    /// Existing images to guide or edit.
    pub references: &'a [ImageGenerationReference<'a>],
}

impl ImageAspect {
    const fn image_size(self) -> &'static str {
        match self {
            Self::Square => "1024x1024",
            Self::Landscape => "1536x1024",
            Self::Portrait => "1024x1536",
        }
    }
}

impl ImageGenerationRequest<'_> {
    pub(super) fn validate(&self, limits: ImageInputLimits) -> Result<()> {
        if self.prompt.trim().is_empty() || self.prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(Error::Tool(
                "image prompt must contain 1–32000 characters".into(),
            ));
        }
        if self.references.len() > MAX_REFERENCES || self.references.len() > limits.max_images {
            return Err(Error::Tool("too many reference images".into()));
        }
        let mut encoded_total = 0_usize;
        for image in self.references {
            let media_type = media_type(image.bytes)?;
            if image.media_type != media_type {
                return Err(Error::Tool(
                    "reference image media type does not match bytes".into(),
                ));
            }
            let encoded = image
                .bytes
                .len()
                .checked_add(2)
                .and_then(|size| size.checked_div(3))
                .and_then(|size| size.checked_mul(4))
                .ok_or_else(|| Error::Tool("reference image is too large".into()))?;
            encoded_total = encoded_total
                .checked_add(encoded)
                .ok_or_else(|| Error::Tool("reference images are too large".into()))?;
            if encoded_total > limits.max_encoded_bytes {
                return Err(Error::Tool("reference images exceed request budget".into()));
            }
        }
        Ok(())
    }
}

/// One complete generated image and provider-reported usage.
#[derive(Debug)]
pub struct GeneratedImage {
    /// PNG, JPEG, WebP, or GIF bytes.
    pub bytes: Vec<u8>,
    /// MIME type derived from the image bytes.
    pub media_type: String,
    /// Token usage when supplied by the provider.
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ImageApi {
    OpenAi,
    Codex,
    OpenRouter,
}

impl ImageApi {
    pub(super) const fn default_model(self) -> &'static str {
        match self {
            Self::OpenAi | Self::Codex => "gpt-image-2",
            Self::OpenRouter => "openai/gpt-image-2",
        }
    }

    pub(super) fn wire(
        self,
        request: &ImageGenerationRequest<'_>,
    ) -> Result<(&'static str, Value)> {
        if self == Self::OpenAi && !request.references.is_empty() {
            return Err(Error::Provider(
                "public OpenAI image edits require multipart upload".into(),
            ));
        }
        let model = self.default_model();
        let size = request.image_aspect.image_size();
        let encoded = request
            .references
            .iter()
            .map(|image| {
                let encoded_len = image
                    .bytes
                    .len()
                    .checked_add(2)
                    .and_then(|size| size.checked_div(3))
                    .and_then(|size| size.checked_mul(4))
                    .ok_or_else(|| Error::Tool("reference image is too large".into()))?;
                if encoded_len.saturating_add(image.media_type.len() + "data:;base64,".len())
                    > MAX_REFERENCE_URL_BYTES
                {
                    return Err(Error::Tool("reference image is too large".into()));
                }
                let data = base64::engine::general_purpose::STANDARD.encode(image.bytes);
                Ok(image_data_url(image.media_type, &data))
            })
            .collect::<Result<Vec<_>>>()?;
        match self {
            Self::OpenAi | Self::Codex if encoded.is_empty() => Ok((
                "images/generations",
                json!({"model": model, "prompt": request.prompt, "n": 1, "size": size}),
            )),
            Self::Codex => Ok((
                "images/edits",
                json!({
                    "model": model,
                    "prompt": request.prompt,
                    "n": 1,
                    "size": size,
                    "images": encoded.into_iter().map(|image_url| json!({"image_url": image_url})).collect::<Vec<_>>(),
                }),
            )),
            Self::OpenAi => Err(Error::Provider(
                "public OpenAI image edits require multipart upload".into(),
            )),
            Self::OpenRouter => {
                let mut body =
                    json!({"model": model, "prompt": request.prompt, "n": 1, "size": size});
                if !encoded.is_empty() {
                    body["input_references"] = Value::Array(
                        encoded
                            .into_iter()
                            .map(|url| json!({"type": "image_url", "image_url": {"url": url}}))
                            .collect(),
                    );
                }
                Ok(("images", body))
            }
        }
    }

    pub(super) fn openai_edit_form(
        request: &ImageGenerationRequest<'_>,
    ) -> Result<reqwest::multipart::Form> {
        let mut form = reqwest::multipart::Form::new()
            .text("model", Self::OpenAi.default_model())
            .text("prompt", request.prompt.to_owned())
            .text("n", "1")
            .text("size", request.image_aspect.image_size());
        for (index, image) in request.references.iter().enumerate() {
            let extension = match image.media_type {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/webp" => "webp",
                _ => return Err(Error::Tool("unsupported reference image type".into())),
            };
            let part = reqwest::multipart::Part::bytes(image.bytes.to_vec())
                .file_name(format!("reference-{index}.{extension}"))
                .mime_str(image.media_type)?;
            form = form.part("image[]", part);
        }
        Ok(form)
    }

    pub(super) fn decode(self, response: &[u8]) -> Result<GeneratedImage> {
        let response: Value = serde_json::from_slice(response)?;
        let data = response
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Provider("image response has no data".into()))?;
        if data.len() != 1 {
            return Err(Error::Provider(
                "image response must contain one image".into(),
            ));
        }
        let encoded = data[0]
            .get("b64_json")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Provider("image response has no image bytes".into()))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| Error::Provider("image response contains invalid base64".into()))?;
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return Err(Error::Provider(
                "image response exceeded image size limit".into(),
            ));
        }
        let media_type = media_type(&bytes)
            .map_err(|_| Error::Provider("image response has unsupported image format".into()))?;
        if data[0]
            .get("media_type")
            .is_some_and(|declared| declared.as_str() != Some(media_type))
        {
            return Err(Error::Provider(
                "image response media type does not match bytes".into(),
            ));
        }
        let usage = parse_usage(response.get("usage"), self)?;
        Ok(GeneratedImage {
            bytes,
            media_type: media_type.into(),
            usage,
        })
    }
}

fn parse_usage(value: Option<&Value>, api: ImageApi) -> Result<Option<TokenUsage>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let (input_field, output_field) = match api {
        ImageApi::OpenAi | ImageApi::Codex => ("/input_tokens", "/output_tokens"),
        ImageApi::OpenRouter => ("/prompt_tokens", "/completion_tokens"),
    };
    let input_tokens = usage_i64(Some(value), input_field, "Images")?.unwrap_or_default();
    let output_tokens = usage_i64(Some(value), output_field, "Images")?.unwrap_or_default();
    let total_tokens = match usage_i64(Some(value), "/total_tokens", "Images")? {
        Some(total) => total,
        None => input_tokens
            .checked_add(output_tokens)
            .ok_or_else(|| Error::Provider("image response token usage overflow".into()))?,
    };
    let usage = TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        ..TokenUsage::default()
    };
    validate_usage(&usage)?;
    Ok(Some(usage))
}

fn media_type(bytes: &[u8]) -> Result<&'static str> {
    match image::guess_format(bytes) {
        Ok(ImageFormat::Png) => Ok("image/png"),
        Ok(ImageFormat::Jpeg) => Ok("image/jpeg"),
        Ok(ImageFormat::WebP) => Ok("image/webp"),
        Ok(ImageFormat::Gif) => Ok("image/gif"),
        _ => Err(Error::Tool("image must be PNG, JPEG, WebP, or GIF".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1,
        162, 75, 152, 19, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    #[test]
    fn native_image_wire_and_response_are_bounded_and_provider_neutral() {
        let references = [ImageGenerationReference {
            media_type: "image/png",
            bytes: PNG,
        }];
        let request = ImageGenerationRequest {
            prompt: "paint this blue",
            image_aspect: ImageAspect::Landscape,
            references: &references,
        };
        request
            .validate(ImageInputLimits::default())
            .expect("input");
        let (endpoint, openai) = ImageApi::Codex.wire(&request).expect("Codex JSON edit");
        assert_eq!(endpoint, "images/edits");
        assert_eq!(openai["model"], "gpt-image-2");
        assert_eq!(openai["size"], "1536x1024");
        assert!(
            openai["images"][0]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert!(ImageApi::OpenAi.wire(&request).is_err());
        let (endpoint, openrouter) = ImageApi::OpenRouter
            .wire(&request)
            .expect("OpenRouter JSON edit");
        assert_eq!(endpoint, "images");
        assert_eq!(openrouter["input_references"][0]["type"], "image_url");
        assert_eq!(openrouter["size"], "1536x1024");

        let (endpoint, portrait) = ImageApi::OpenAi
            .wire(&ImageGenerationRequest {
                prompt: "a tall painting",
                image_aspect: ImageAspect::Portrait,
                references: &[],
            })
            .expect("OpenAI generation");
        assert_eq!(endpoint, "images/generations");
        assert_eq!(portrait["size"], "1024x1536");

        let image = base64::engine::general_purpose::STANDARD.encode(PNG);
        let decoded = ImageApi::OpenRouter
            .decode(json!({"data": [{"b64_json": image, "media_type": "image/png"}], "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5, "cost": 0.04}}).to_string().as_bytes())
            .expect("response");
        assert_eq!(decoded.bytes, PNG);
        assert_eq!(decoded.media_type, "image/png");
        assert_eq!(decoded.usage.unwrap().total_tokens, 5);
        assert!(
            ImageApi::openai_edit_form(&ImageGenerationRequest {
                prompt: "edit",
                image_aspect: ImageAspect::Portrait,
                references: &[ImageGenerationReference {
                    media_type: "image/gif",
                    bytes: PNG,
                }],
            })
            .is_err()
        );
    }

    #[test]
    fn multipart_edits_do_not_inherit_data_url_size_limit() {
        let mut bytes = vec![0; MAX_REFERENCE_URL_BYTES / 4 * 3];
        bytes[..PNG.len()].copy_from_slice(PNG);
        let references = [ImageGenerationReference {
            media_type: "image/png",
            bytes: &bytes,
        }];
        let request = ImageGenerationRequest {
            prompt: "edit",
            image_aspect: ImageAspect::Square,
            references: &references,
        };
        request
            .validate(ImageInputLimits::default())
            .expect("aggregate request budget");
        assert!(ImageApi::OpenAi.wire(&request).is_err());
        assert!(ImageApi::Codex.wire(&request).is_err());
        assert!(ImageApi::openai_edit_form(&request).is_ok());
    }
}
