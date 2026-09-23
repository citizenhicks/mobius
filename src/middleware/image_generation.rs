//! Native image generation, publication, and transcript presentation.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::manifest::MiddlewareManifest;
use super::tools::{ApprovalRequirement, Catalog, Tool, ToolContext, render_tool_event};
use super::{Middleware, RuntimeContext};
use crate::backend::model::{
    ImageGenerationReference, ImageGenerationRequest, ModelRouter, ToolDefinition,
};
use crate::backend::session_files::SessionFileStore;
use crate::protocol::{
    ContentPart, EventMsg, FrontendBlock, FrontendBlockFormat, FrontendBlockRole,
    FrontendBlockUpdate, FrontendContribution, ModelCapability, ToolContent, ToolResponse,
};
use crate::{BoxFuture, Error, Result};

/// Configuration metadata for native image generation.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "image_generation",
    label: "Image generation",
    description: "Create images with a supported OpenAI or OpenRouter model",
    required: false,
    default_enabled: false,
    required_model_capability: Some(ModelCapability::ImageGeneration),
    settings: &[],
};

/// Generates a session image and publishes it as a chat artifact.
pub struct ImageGeneration {
    models: Arc<ModelRouter>,
    store: SessionFileStore,
}

impl ImageGeneration {
    /// Creates the image capability with the configured model routes and session files.
    #[must_use]
    pub fn new(models: Arc<ModelRouter>, store: SessionFileStore) -> Self {
        Self { models, store }
    }
}

impl Middleware for ImageGeneration {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        catalog.register(Arc::new(GenerateImage {
            models: Arc::clone(&self.models),
            store: self.store.clone(),
            session_id: runtime.session_id.clone(),
        }))
    }

    fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: MANIFEST.id.into(),
            ..FrontendContribution::default()
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        let mut block = render_tool_event(
            event,
            |name| name == "generate_image",
            |_, _| "Generating image".into(),
        )?;
        block.role = FrontendBlockRole::Artifact;
        block.format = FrontendBlockFormat::Image;
        block.update = FrontendBlockUpdate::Replace;
        match event {
            EventMsg::ToolCallBegin(_) => block.text.clear(),
            EventMsg::ToolCallEnd(result) if !result.is_error => {
                if let Some(file) = result.output.files().next().cloned() {
                    block.title = "Image ready".into();
                    block.text.clear();
                    block.content = ToolContent::default();
                    block.files = vec![file];
                }
            }
            EventMsg::ToolCallEnd(_) => block.title = "Image generation failed".into(),
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
    reference_file_ids: Vec<String>,
}

struct GenerateImage {
    models: Arc<ModelRouter>,
    store: SessionFileStore,
    session_id: String,
}

impl Tool for GenerateImage {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "generate_image".into(),
            description: "Generate one image from a prompt, optionally using images already attached to this chat as references. The finished image is sent to the user.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "minLength": 1, "maxLength": 32000, "description": "Describe the image to create or how to edit the reference images."},
                    "reference_file_ids": {"type": "array", "maxItems": 4, "items": {"type": "string"}, "description": "Optional authorized image file IDs from this chat."}
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call<'a>(
        &'a self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolResponse>> {
        Box::pin(async move {
            let arguments: GenerateImageArgs = serde_json::from_value(arguments)?;
            if arguments.prompt.trim().is_empty() || arguments.prompt.chars().count() > 32_000 {
                return Err(Error::Tool(
                    "image prompt must contain 1–32000 characters".into(),
                ));
            }
            if arguments.reference_file_ids.len() > 4 {
                return Err(Error::Tool("too many reference images".into()));
            }
            let route = context.model_route();
            if !self.models.supports_image_generation(route)? {
                return Err(Error::Tool(
                    "the active model route does not support image generation".into(),
                ));
            }
            let mut owned_references = Vec::with_capacity(arguments.reference_file_ids.len());
            for id in &arguments.reference_file_ids {
                let file = self.store.file_reference(&self.session_id, id).await?;
                owned_references.push((
                    file.media_type.clone(),
                    self.store.read_image(&self.session_id, &file).await?,
                ));
            }
            let references = owned_references
                .iter()
                .map(|(media_type, bytes)| ImageGenerationReference { media_type, bytes })
                .collect::<Vec<_>>();
            let generated = self
                .models
                .generate_image(
                    route,
                    ImageGenerationRequest {
                        prompt: &arguments.prompt,
                        references: &references,
                    },
                )
                .await?;
            if let Some(usage) = generated.usage {
                context.report_usage(usage)?;
            }
            let extension = match generated.media_type.as_str() {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/webp" => "webp",
                "image/gif" => "gif",
                _ => {
                    return Err(Error::Tool(
                        "provider returned an unsupported image format".into(),
                    ));
                }
            };
            let file = self
                .store
                .publish_image(
                    &self.session_id,
                    format!("generated-image.{extension}"),
                    &generated.media_type,
                    generated.bytes,
                )
                .await?;
            Ok(ToolResponse {
                content: ToolContent(vec![ContentPart::File { file }]),
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRole;
    use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
    use crate::backend::model::{GeneratedImage, Model, ModelEventSink, ModelOutput, ModelRequest};
    use crate::backend::sandbox::local::LocalSandbox;
    use crate::backend::sandbox::{
        ApprovalPolicy, NetworkAccess, Sandbox, SandboxMode, SandboxPermissions,
    };
    use crate::middleware::tools::ToolExposure;
    use crate::protocol::{
        FrontendBlockState, SessionContext, TokenUsage, ToolCallBeginEvent, ToolCallEndEvent,
    };

    struct ImageModel(Vec<u8>, bool);

    impl Model for ImageModel {
        fn supports_image_generation(&self) -> bool {
            self.1
        }

        fn generate_image<'a>(
            &'a self,
            _request: ImageGenerationRequest<'a>,
        ) -> BoxFuture<'a, Result<GeneratedImage>> {
            let bytes = self.0.clone();
            Box::pin(async move {
                Ok(GeneratedImage {
                    bytes,
                    media_type: "image/png".into(),
                    usage: Some(TokenUsage {
                        output_tokens: 1,
                        total_tokens: 1,
                        ..TokenUsage::default()
                    }),
                })
            })
        }

        fn respond<'a>(
            &'a self,
            _request: ModelRequest<'a>,
            _events: ModelEventSink,
        ) -> BoxFuture<'a, Result<ModelOutput>> {
            Box::pin(async { unreachable!("image tool never starts a conversation response") })
        }
    }

    #[tokio::test]
    async fn generates_one_artifact_and_replaces_its_pending_transcript_block() {
        let state = tempfile::tempdir().expect("state");
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("test image");
        let png = bytes.into_inner();
        let store = SessionFileStore::new(state.path());
        let middleware = ImageGeneration::new(
            Arc::new(ModelRouter::new(
                "image",
                Arc::new(ImageModel(png.clone(), true)),
            )),
            store.clone(),
        );
        let runtime = RuntimeContext {
            sender: crate::agent::test_sender(),
            checkpoints: Arc::new(
                SqliteCheckpoint::new(state.path().join("checkpoints.sqlite3"))
                    .expect("checkpoint store"),
            ),
            session_id: "session".into(),
            model_route: "image".into(),
            model: "image".into(),
            approval_policy: ApprovalPolicy::Ask,
            session_context: SessionContext::default(),
            metadata: Default::default(),
            role: AgentRole::Main,
            frontend: Arc::new(|_| Ok(())),
        };
        let mut catalog = Catalog::default();
        middleware
            .register(&mut catalog, &runtime)
            .expect("supported route registers tool");
        let unsupported = ImageGeneration::new(
            Arc::new(ModelRouter::new(
                "unsupported",
                Arc::new(ImageModel(png.clone(), false)),
            )),
            store.clone(),
        );
        let mut unsupported_runtime = runtime.clone();
        unsupported_runtime.model_route = "unsupported".into();
        unsupported
            .register(&mut Catalog::default(), &unsupported_runtime)
            .expect("registration does not depend on credential availability");
        let source = store
            .publish_artifact("session", "source.png".into(), "image/png".into(), &png)
            .await
            .expect("source image");
        let tool = GenerateImage {
            models: Arc::clone(&middleware.models),
            store: store.clone(),
            session_id: "session".into(),
        };
        assert_eq!(tool.exposure(), ToolExposure::Deferred);
        assert_eq!(tool.approval(), ApprovalRequirement::Always);
        let sandbox = Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(state.path()).expect("sandbox")),
            ApprovalPolicy::Ask,
        ));
        let permissions = SandboxPermissions::restore(
            "session",
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Allowed,
            ["image-call".into()],
        );
        let context = ToolContext::new(
            Arc::clone(&sandbox),
            permissions.for_call("image-call"),
            "turn",
        )
        .with_model_route("image");
        let response = tool
            .call(
                context,
                serde_json::json!({"prompt": "a red square", "reference_file_ids": [source.id]}),
            )
            .await
            .expect("generate image");
        assert!(matches!(
            response.content.0.first(),
            Some(ContentPart::File { .. })
        ));
        let file = response
            .content
            .files()
            .next()
            .expect("published file")
            .clone();
        assert_eq!(store.read_file("session", &file).await.expect("image"), png);
        let artifacts = store.list_artifacts("session").await.expect("artifacts");
        assert_eq!(artifacts.len(), 2);
        assert!(artifacts.contains(&file));
        assert_eq!(
            store
                .list_files("session")
                .await
                .expect("stored files")
                .len(),
            2
        );

        let begin = middleware
            .render(
                &EventMsg::ToolCallBegin(ToolCallBeginEvent {
                    turn_id: "turn".into(),
                    call_id: "image-call".into(),
                    name: "generate_image".into(),
                    arguments: serde_json::json!({"prompt": "a red square"}),
                }),
                "session",
            )
            .expect("pending block");
        let end = middleware
            .render(
                &EventMsg::ToolCallEnd(ToolCallEndEvent {
                    turn_id: "turn".into(),
                    call_id: "image-call".into(),
                    name: "generate_image".into(),
                    output: response.content,
                    is_error: false,
                }),
                "session",
            )
            .expect("completed block");
        assert_eq!(begin.id, end.id);
        assert_eq!(begin.state, FrontendBlockState::Pending);
        assert_eq!(begin.format, FrontendBlockFormat::Image);
        assert_eq!(end.state, FrontendBlockState::Complete);
        assert_eq!(end.files, vec![file]);

        let unsupported_tool = GenerateImage {
            models: Arc::clone(&unsupported.models),
            store,
            session_id: "session".into(),
        };
        let unsupported_context =
            ToolContext::new(sandbox, permissions.for_call("image-call"), "turn")
                .with_model_route("unsupported");
        assert!(matches!(
            unsupported_tool
                .call(
                    unsupported_context,
                    serde_json::json!({"prompt": "a red square"})
                )
                .await,
            Err(Error::Tool(_))
        ));
    }
}
