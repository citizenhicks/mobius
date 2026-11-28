//! Native image generation, publication, and transcript presentation.

use crate::SessionId;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::tools::{ApprovalRequirement, Catalog, Tool, ToolContext, ToolSpec};
use super::{Middleware, RuntimeContext};
use crate::backend::model::{
    ImageAspect, ImageGenerationReference, ImageGenerationRequest, ModelRouter, ToolDefinition,
};
use crate::backend::session_files::SessionFileStore;
use crate::protocol::{
    ContentPart, EventMsg, FrontendBlock, FrontendBlockFormat, FrontendBlockRole,
    FrontendBlockUpdate, FrontendSettingValue, ToolContent, ToolResponse,
};
use crate::{BoxFuture, Error, Result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    default_enabled: bool,
    manifest_label: String,
    manifest_description: String,
    render_ready: String,
    render_failed: String,
    prompt_main: String,
    #[serde(deserialize_with = "super::manifest::deserialize_settings")]
    settings: Vec<super::manifest::MiddlewareSettingManifest>,
    generate_image: ToolSpec,
}

const MAX_REFERENCE_IMAGES: usize = 4;

fn limit_schema(definition: &mut Definition) {
    super::tools::limit_schema(
        &mut definition.generate_image.tool,
        [
            (
                "/properties/prompt",
                "maxLength",
                crate::backend::model::MAX_IMAGE_PROMPT_CHARS.into(),
            ),
            (
                "/properties/reference_file_ids",
                "maxItems",
                MAX_REFERENCE_IMAGES.into(),
            ),
        ],
    );
}

crate::embedded_config! { static DEFINITION: Definition = include_str!("image_generation.toml") => limit_schema; }

super::manifest::middleware_manifest! {
/// Configuration metadata for native image generation.
    "image_generation", DEFINITION, required: false, settings: &DEFINITION.settings
}

/// Generates a session image and publishes it as a chat artifact.
pub struct ImageGeneration {
    models: Arc<ModelRouter>,
    store: Arc<SessionFileStore>,
    route: Option<Arc<str>>,
}

impl std::fmt::Debug for ImageGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImageGeneration")
            .field("route", &self.route)
            .finish_non_exhaustive()
    }
}

impl ImageGeneration {
    /// Creates the image capability with the configured model routes and session files.
    /// `route` selects an image model route; `None` uses the first configured one.
    #[must_use]
    pub fn new(
        models: Arc<ModelRouter>,
        store: impl Into<Arc<SessionFileStore>>,
        route: Option<Arc<str>>,
    ) -> Self {
        Self {
            models,
            store: store.into(),
            route,
        }
    }

    /// Creates the image capability from its manifest settings, read through `setting`;
    /// the manifest check at save time guarantees `model` is a string when set.
    #[must_use]
    pub fn from_settings<'a>(
        models: Arc<ModelRouter>,
        store: impl Into<Arc<SessionFileStore>>,
        setting: impl FnOnce(&str) -> Option<&'a FrontendSettingValue>,
    ) -> Self {
        let route = match setting("model") {
            Some(FrontendSettingValue::String(route)) => Some(Arc::from(route.as_str())),
            _ => None,
        };
        Self::new(models, store, route)
    }
}

impl Middleware for ImageGeneration {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<super::PromptSection>> {
        Ok((self.models.image_choices().len() > 0)
            .then(|| super::PromptSection::new(DEFINITION.prompt_main.as_str())))
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        if self.models.image_choices().len() == 0 {
            return Ok(());
        }
        catalog.register(Arc::new(GenerateImage {
            models: Arc::clone(&self.models),
            store: Arc::clone(&self.store),
            session_id: runtime.session_id.share(),
            route: self.route.as_ref().map(Arc::clone),
        }))
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        let mut block = DEFINITION.generate_image.render(event)?;
        block.role = FrontendBlockRole::Artifact;
        block.format = FrontendBlockFormat::Image;
        block.update = FrontendBlockUpdate::Replace;
        match event {
            EventMsg::ToolCallBegin(_) => block.text.clear(),
            EventMsg::ToolCallEnd(result) if !result.is_error => {
                block.title.clone_from(&DEFINITION.render_ready);
                block.text.clear();
                block.files = result.output.files().cloned().collect(); // rules-allow: the owned block copies the borrowed event's files
                block.content = ToolContent(
                    result
                        .output
                        .0
                        .iter()
                        .filter(|part| matches!(part, ContentPart::Image { .. }))
                        .cloned() // rules-allow: the owned block copies the borrowed event's images
                        .collect(),
                );
            }
            EventMsg::ToolCallEnd(_) => {
                block.title.clone_from(&DEFINITION.render_failed);
            }
            _ => {}
        }
        Some(block)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerateImageArgs {
    prompt: String,
    #[serde(default)]
    image_aspect: Option<ImageAspect>,
    #[serde(default)]
    reference_file_ids: Vec<String>,
}

struct GenerateImage {
    models: Arc<ModelRouter>,
    store: Arc<SessionFileStore>,
    session_id: SessionId,
    route: Option<Arc<str>>,
}

impl Tool for GenerateImage {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &DEFINITION.generate_image.tool
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call(&self, context: ToolContext, arguments: Value) -> BoxFuture<'_, Result<ToolResponse>> {
        Box::pin(async move {
            let arguments: GenerateImageArgs = serde_json::from_value(arguments)?;
            if arguments.reference_file_ids.len() > MAX_REFERENCE_IMAGES {
                return Err(Error::Tool("too many reference images".into()));
            }
            let mut owned_references = Vec::with_capacity(arguments.reference_file_ids.len());
            for id in &arguments.reference_file_ids {
                let file = self.store.file_reference(&self.session_id, id).await?;
                let bytes = self.store.read_image(&self.session_id, &file).await?;
                owned_references.push((file.media_type, bytes));
            }
            let references = owned_references
                .iter()
                .map(|(media_type, bytes)| ImageGenerationReference { media_type, bytes })
                .collect::<Vec<_>>();
            let image = self.models.image_model(self.route.as_deref())?;
            let generated = self
                .models
                .generate_image(
                    image.route,
                    ImageGenerationRequest {
                        size: image.size,
                        model: image.model,
                        quality: image.quality,
                        prompt: &arguments.prompt,
                        image_aspect: arguments.image_aspect.unwrap_or(image.aspect),
                        references: &references,
                    },
                )
                .await?;
            let returns_images = self
                .models
                .supports_tool_image_input(context.model_route())
                .unwrap_or(false);
            if let Some(usage) = generated.usage {
                context.report_usage(image.route, usage)?;
            }
            let extension = image::ImageFormat::from_mime_type(&generated.media_type)
                .filter(image::ImageFormat::reading_enabled)
                .and_then(|format| format.extensions_str().first())
                .ok_or_else(|| {
                    Error::Tool("provider returned an unsupported image format".into())
                })?;
            let image = self
                .store
                .publish_image(
                    &self.session_id,
                    format!("generated-image.{extension}"),
                    &generated.media_type,
                    generated.bytes,
                )
                .await?;
            // Models without image tool results receive the file; frontends still get its pixels.
            let part = if returns_images {
                ContentPart::Image { image }
            } else {
                ContentPart::File { file: image.file }
            };
            Ok(ToolResponse::Completed {
                content: ToolContent(vec![part]),
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests;
