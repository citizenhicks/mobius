//! Provider-neutral image requests and the two native Images API wire shapes.

use std::collections::BTreeMap;

use base64::Engine as _;
use image::ImageFormat;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{ImageInputLimits, image_data_url, usage_i64, validate_usage};
use crate::protocol::TokenUsage;
use crate::{Error, Result};

pub(crate) const MAX_IMAGE_PROMPT_CHARS: usize = 32_000;
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
#[derive(Debug, Clone, Copy)]
pub struct ImageGenerationRequest<'a> {
    /// Image model selected by the caller; provider naming is applied by the transport.
    pub model: &'a str,
    /// The requested image or edit.
    pub prompt: &'a str,
    /// Provider resolution tier when explicitly configured.
    pub size: Option<&'a str>,
    /// Explicit output shape.
    pub image_aspect: ImageAspect,
    /// Quality level advertised by the selected model, or `None` when it has none.
    pub quality: Option<&'a str>,
    /// Existing images to guide or edit.
    pub references: &'a [ImageGenerationReference<'a>],
}

/// Requested shape of a generated image.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageAspect {
    /// Equal width and height.
    #[default]
    Square,
    /// Wider than tall.
    Landscape,
    /// Taller than wide.
    Portrait,
}

/// Defaults attached to one image model by its provider setup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct ImageDefaults {
    /// Optional resolution tier, interpreted by the provider adapter.
    pub size: Option<String>,
    /// Shape used when the image request omits its aspect.
    pub aspect: ImageAspect,
    /// Default quality if the selected route has no explicit variant.
    pub quality: Option<String>,
}

#[derive(Deserialize)]
struct Geometry {
    resolutions: BTreeMap<String, BTreeMap<ImageAspect, String>>,
    defaults: ImageGeometry,
}
#[derive(Deserialize)]
struct ImageGeometry {
    size: String,
    aspect: ImageAspect,
}
crate::embedded_config! {
    static GEOMETRY: Geometry = include_str!("image_defaults.toml");
}

impl ImageDefaults {
    /// Maximum byte length accepted by image adapters for a quality identifier.
    pub const MAX_QUALITY_BYTES: usize = 64;

    fn valid_quality(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= Self::MAX_QUALITY_BYTES
            && value.trim() == value
            && !value.chars().any(char::is_control)
    }

    /// Resolution tiers understood by the image adapters.
    pub fn sizes() -> impl Iterator<Item = &'static str> {
        GEOMETRY.resolutions.keys().map(String::as_str)
    }

    /// Checks the geometry and quality before sending the provider request.
    /// # Errors
    /// Returns an error when a tier or quality cannot be represented.
    pub fn validate(&self) -> Result<()> {
        if self
            .size
            .as_ref()
            .is_some_and(|v| !GEOMETRY.resolutions.contains_key(v))
        {
            return Err(Error::Invalid("unsupported image size".into()));
        }
        if self
            .quality
            .as_deref()
            .is_some_and(|v| !Self::valid_quality(v))
        {
            return Err(Error::Invalid("invalid image quality".into()));
        }
        Ok(())
    }
}
impl Default for ImageDefaults {
    fn default() -> Self {
        Self {
            size: None,
            quality: None,
            aspect: GEOMETRY.defaults.aspect,
        }
    }
}

impl ImageAspect {
    fn image_size(self, size: Option<&str>) -> Result<&'static str> {
        GEOMETRY
            .resolutions
            .get(size.unwrap_or(&GEOMETRY.defaults.size))
            .and_then(|r| r.get(&self))
            .map(String::as_str)
            .ok_or_else(|| Error::Invalid("unsupported image geometry".into()))
    }
    const fn aspect_ratio(self) -> &'static str {
        match self {
            Self::Square => "1:1",
            Self::Landscape => "3:2",
            Self::Portrait => "2:3",
        }
    }
}

impl ImageGenerationRequest<'_> {
    pub(super) fn edit_form(&self) -> Result<reqwest::multipart::Form> {
        let mut form = reqwest::multipart::Form::new()
            .text("model", self.model.to_owned())
            .text("prompt", self.prompt.to_owned())
            .text("n", "1")
            .text("size", self.image_aspect.image_size(self.size)?);
        if let Some(quality) = self.quality {
            form = form.text("quality", quality.to_owned());
        }
        for (index, image) in self.references.iter().enumerate() {
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

    pub(super) fn validate(&self, limits: ImageInputLimits) -> Result<()> {
        if self.model.trim().is_empty() || self.model.len() > 256 {
            return Err(Error::Tool("image model must contain 1–256 bytes".into()));
        }
        if self.prompt.trim().is_empty() || self.prompt.chars().count() > MAX_IMAGE_PROMPT_CHARS {
            return Err(Error::Tool(
                "image prompt must contain 1–32000 characters".into(),
            ));
        }
        if self
            .quality
            .is_some_and(|quality| !ImageDefaults::valid_quality(quality))
        {
            return Err(Error::Tool("image quality must contain 1–64 bytes".into()));
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
            let encoded = base64::encoded_len(image.bytes.len(), true)
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

impl GeneratedImage {
    /// Decodes the response with the same bounds used for session image attachments.
    /// # Errors
    /// Rejects invalid data and images whose MIME type disagrees with the decoded bytes.
    pub fn validate(&self) -> Result<(u32, u32)> {
        let (mime, width, height, _) = crate::backend::session_files::validate_image(&self.bytes)?;
        if mime != self.media_type {
            return Err(Error::Provider("generated image MIME type mismatch".into()));
        }
        Ok((width, height))
    }
}

crate::embedded_config! {
    pub(super) static IMAGE_APIS: BTreeMap<String, ImageApi> = include_str!("image_generation.toml");
}

pub(super) fn image_api(name: &str) -> Result<&'static ImageApi> {
    IMAGE_APIS
        .get(name)
        .ok_or_else(|| Error::Invalid(format!("image API `{name}` is not configured")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ImageApi {
    pub(super) generation_path: String,
    pub(super) edit_path: String,
    reference_format: ImageReferenceFormat,
    size_format: ImageSizeFormat,
    input_tokens: String,
    output_tokens: String,
}

/// How an API receives the output shape: pixel `size` or a normalized `aspect_ratio`.
#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum ImageSizeFormat {
    Pixels,
    AspectRatio,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ImageReferenceFormat {
    Multipart,
    ImageUrls,
    InputReferences,
}

impl super::provider::ProviderDefinition {
    /// Resolution sent when a user leaves the image size at its default.
    /// Ratio-based APIs can choose the output resolution upstream.
    #[must_use]
    pub fn default_image_resolution(&self) -> Option<&'static str> {
        let api = image_api(self.image_endpoint?).ok()?;
        matches!(api.size_format, ImageSizeFormat::Pixels)
            .then_some(GEOMETRY.defaults.size.as_str())
    }
}

impl ImageApi {
    pub(super) fn uses_multipart(&self, request: &ImageGenerationRequest<'_>) -> bool {
        !request.references.is_empty() && self.reference_format == ImageReferenceFormat::Multipart
    }

    pub(super) fn wire(&self, request: &ImageGenerationRequest<'_>) -> Result<(&str, Value)> {
        if self.uses_multipart(request) {
            return Err(Error::Provider(
                "image edits require multipart upload".into(),
            ));
        }
        let model = request.model;
        let encoded = request
            .references
            .iter()
            .map(|image| {
                let encoded_len = base64::encoded_len(image.bytes.len(), true)
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
        let mut body = json!({"model": model, "prompt": request.prompt, "n": 1});
        match self.size_format {
            ImageSizeFormat::Pixels => {
                super::set_field(
                    &mut body,
                    "size",
                    json!(request.image_aspect.image_size(request.size)?),
                );
            }
            ImageSizeFormat::AspectRatio => {
                if let Some(size) = request.size {
                    super::set_field(&mut body, "resolution", json!(size));
                }
                super::set_field(
                    &mut body,
                    "aspect_ratio",
                    json!(request.image_aspect.aspect_ratio()),
                );
            }
        }
        if let Some(quality) = request.quality {
            super::set_field(&mut body, "quality", json!(quality));
        }
        if encoded.is_empty() {
            return Ok((&self.generation_path, body));
        }
        match self.reference_format {
            ImageReferenceFormat::ImageUrls => super::set_field(
                &mut body,
                "images",
                encoded
                    .into_iter()
                    .map(|image_url| json!({"image_url": image_url}))
                    .collect(),
            ),
            ImageReferenceFormat::InputReferences => super::set_field(
                &mut body,
                "input_references",
                encoded
                    .into_iter()
                    .map(|url| json!({"type": "image_url", "image_url": {"url": url}}))
                    .collect(),
            ),
            ImageReferenceFormat::Multipart => unreachable!("multipart requests are handled above"),
        }
        Ok((&self.edit_path, body))
    }

    pub(super) fn decode(&self, response: &[u8]) -> Result<GeneratedImage> {
        let response: Value = serde_json::from_slice(response)?;
        let data = response
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Provider("image response has no data".into()))?;
        let [image] = data.as_slice() else {
            return Err(Error::Provider(
                "image response must contain one image".into(),
            ));
        };
        let encoded = image
            .get("b64_json")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Provider("image response has no image bytes".into()))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| {
                Error::Provider(format!("image response contains invalid base64: {error}").into())
            })?;
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return Err(Error::Provider(
                "image response exceeded image size limit".into(),
            ));
        }
        let media_type = media_type(&bytes).map_err(|error| {
            Error::Provider(format!("image response has unsupported image format: {error}").into())
        })?;
        if image
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

fn parse_usage(value: Option<&Value>, api: &ImageApi) -> Result<Option<TokenUsage>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let input_tokens = usage_i64(Some(value), &api.input_tokens, "Images")?.unwrap_or_default();
    let output_tokens = usage_i64(Some(value), &api.output_tokens, "Images")?.unwrap_or_default();
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
    fn saved_quality_and_image_requests_accept_the_same_byte_limit() {
        for (length, valid) in [(64, true), (65, false)] {
            let quality = "x".repeat(length);
            let defaults = ImageDefaults {
                quality: Some(quality),
                ..Default::default()
            };
            let selected = ImageGenerationRequest {
                quality: defaults.quality.as_deref(),
                model: "image",
                prompt: "paint",
                size: None,
                image_aspect: ImageAspect::Square,
                references: &[],
            };
            assert_eq!(defaults.validate().is_ok(), valid);
            assert_eq!(
                selected.validate(ImageInputLimits::default()).is_ok(),
                valid
            );
        }
    }

    #[test]
    fn native_image_wire_and_response_are_bounded_and_provider_neutral() {
        let references = [ImageGenerationReference {
            media_type: "image/png",
            bytes: PNG,
        }];
        let request = ImageGenerationRequest {
            size: None,
            model: "gpt-image-2.5-sunburst",
            prompt: "paint this blue",
            image_aspect: ImageAspect::Landscape,
            quality: None,
            references: &references,
        };
        request
            .validate(ImageInputLimits::default())
            .expect("input");
        let (endpoint, openai) = IMAGE_APIS["codex"].wire(&request).expect("Codex JSON edit");
        assert_eq!(endpoint, "images/edits");
        assert_eq!(openai["model"], "gpt-image-2.5-sunburst");
        assert_eq!(openai["size"], "1536x1024");
        let large = ImageGenerationRequest {
            size: Some("2K"),
            quality: Some("high"),
            ..request
        };
        let (_, pixels) = IMAGE_APIS["codex"].wire(&large).expect("pixel tier");
        let (_, ratio) = IMAGE_APIS["openrouter"]
            .wire(&large)
            .expect("resolution tier");
        assert_eq!(pixels["size"], "3072x2048");
        assert_eq!(pixels["quality"], "high");
        assert_eq!(ratio["resolution"], "2K");
        assert_eq!(ratio["aspect_ratio"], "3:2");
        assert_eq!(ratio["quality"], "high");
        assert!(
            openai["images"][0]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert!(IMAGE_APIS["openai"].wire(&request).is_err());
        let (endpoint, openrouter) = IMAGE_APIS["openrouter"]
            .wire(&ImageGenerationRequest {
                size: None,
                model: "openai/gpt-image-2.5-sunburst",
                ..request
            })
            .expect("OpenRouter JSON edit");
        assert_eq!(endpoint, "images");
        assert_eq!(openrouter["model"], "openai/gpt-image-2.5-sunburst");
        assert_eq!(openrouter["input_references"][0]["type"], "image_url");
        assert_eq!(openrouter["aspect_ratio"], "3:2");
        assert!(openrouter.get("size").is_none());
        assert!(openrouter.get("quality").is_none());

        let (endpoint, portrait) = IMAGE_APIS["openai"]
            .wire(&ImageGenerationRequest {
                size: None,
                model: "gpt-image-2.5-flare",
                prompt: "a tall painting",
                image_aspect: ImageAspect::Portrait,
                quality: None,
                references: &[],
            })
            .expect("OpenAI generation");
        assert_eq!(endpoint, "images/generations");
        assert_eq!(portrait["model"], "gpt-image-2.5-flare");
        assert_eq!(portrait["size"], "1024x1536");

        let image = base64::engine::general_purpose::STANDARD.encode(PNG);
        let decoded = IMAGE_APIS["openrouter"]
            .decode(json!({"data": [{"b64_json": image, "media_type": "image/png"}], "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5, "cost": 0.04}}).to_string().as_bytes())
            .expect("response");
        assert_eq!(decoded.bytes, PNG);
        assert_eq!(decoded.media_type, "image/png");
        assert_eq!(decoded.usage.unwrap().total_tokens, 5);
        assert!(
            ImageGenerationRequest {
                size: None,
                model: "gpt-image-2.5-sunburst",
                prompt: "edit",
                image_aspect: ImageAspect::Portrait,
                quality: None,
                references: &[ImageGenerationReference {
                    media_type: "image/gif",
                    bytes: PNG,
                }],
            }
            .edit_form()
            .is_err()
        );
    }
}
