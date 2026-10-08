//! Native image generation, publication, and transcript presentation.

use std::sync::{Arc, LazyLock};

use serde::Deserialize;
use serde_json::Value;

use super::tools::{ApprovalRequirement, Catalog, Tool, ToolContext, render_tool_event};
use super::{Middleware, RuntimeContext};
use crate::backend::model::{
    ImageGenerationReference, ImageGenerationRequest, ModelRouter, ToolDefinition,
};
use crate::backend::session_files::SessionFileStore;
use crate::protocol::{
    ContentPart, EventMsg, FrontendBlock, FrontendBlockFormat, FrontendBlockRole,
    FrontendBlockUpdate, FrontendContribution, ImageAspect, ToolContent, ToolResponse,
};
use crate::{BoxFuture, Error, Result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    default_enabled: bool,
    manifest_label: String,
    manifest_description: String,
    render_pending: String,
    render_ready: String,
    render_failed: String,
    #[serde(deserialize_with = "super::manifest::deserialize_settings")]
    settings: Vec<super::manifest::MiddlewareSettingManifest>,
    tool: ToolDefinition,
}

static DEFINITION: LazyLock<Definition> =
    LazyLock::new(|| crate::config::embedded(include_str!("image_generation.toml")));

super::manifest::middleware_manifest! {
/// Configuration metadata for native image generation.
    "image_generation", DEFINITION, required: false, capability: None, settings: &DEFINITION.settings
}

/// Generates a session image and publishes it as a chat artifact.
pub struct ImageGeneration {
    models: Arc<ModelRouter>,
    store: SessionFileStore,
    route: Option<Arc<str>>,
}

impl ImageGeneration {
    /// Creates the image capability with the configured model routes and session files.
    /// `route` selects an image model route; `None` uses the first configured one.
    #[must_use]
    pub fn new(models: Arc<ModelRouter>, store: SessionFileStore, route: Option<Arc<str>>) -> Self {
        Self {
            models,
            store,
            route,
        }
    }
}

impl Middleware for ImageGeneration {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        if self.models.image_choices().len() == 0 {
            return Ok(());
        }
        catalog.register(Arc::new(GenerateImage {
            models: Arc::clone(&self.models),
            store: self.store.clone(),
            session_id: runtime.session_id.clone(),
            route: self.route.as_ref().map(Arc::clone),
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
            |name| name == DEFINITION.tool.name,
            |_, _| DEFINITION.render_pending.as_str().into(),
        )?;
        block.role = FrontendBlockRole::Artifact;
        block.format = FrontendBlockFormat::Image;
        block.update = FrontendBlockUpdate::Replace;
        match event {
            EventMsg::ToolCallBegin(call) => {
                block.text.clear();
                block.image_aspect = Some(
                    call.arguments
                        .get("image_aspect")
                        .and_then(|value| serde::Deserialize::deserialize(value).ok())
                        .unwrap_or_default(),
                );
            }
            EventMsg::ToolCallEnd(result) if !result.is_error => {
                if let Some(file) = result.output.files().next().cloned() {
                    block.title = DEFINITION.render_ready.clone();
                    block.text.clear();
                    block.content = ToolContent::default();
                    block.files = vec![file];
                }
            }
            EventMsg::ToolCallEnd(_) => block.title = DEFINITION.render_failed.clone(),
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
    image_aspect: ImageAspect,
    #[serde(default)]
    reference_file_ids: Vec<String>,
}

struct GenerateImage {
    models: Arc<ModelRouter>,
    store: SessionFileStore,
    session_id: String,
    route: Option<Arc<str>>,
}

impl Tool for GenerateImage {
    fn definition(&self) -> ToolDefinition {
        DEFINITION.tool.clone()
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
                        model: image.model,
                        quality: image.quality,
                        prompt: &arguments.prompt,
                        image_aspect: arguments.image_aspect,
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
            request: ImageGenerationRequest<'a>,
        ) -> BoxFuture<'a, Result<GeneratedImage>> {
            assert_eq!(request.model, "gpt-image-2.5-sunburst");
            assert_eq!(request.image_aspect, ImageAspect::Landscape);
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
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(3, 2))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("test image");
        let png = bytes.into_inner();
        let store = SessionFileStore::new(state.path(), None);
        let mut router = ModelRouter::new("image", Arc::new(ImageModel(png.clone(), true)));
        router
            .register_image(
                Arc::new(ImageModel(png.clone(), true)),
                crate::protocol::ModelChoice {
                    route: "openai::gpt-image-2.5-sunburst".into(),
                    group: "OpenAI".into(),
                    model: "gpt-image-2.5-sunburst".into(),
                    reasoning_effort: None,
                    variant_label: None,
                    context_window: None,
                    supports_image_input: false,
                    supports_image_generation: true,
                    supports_realtime_voice: false,
                    tool_discovery: crate::protocol::ToolDiscoveryMode::Rebuild,
                },
                Default::default(),
            )
            .expect("image route");
        router
            .register_image(
                Arc::new(ImageModel(png.clone(), true)),
                crate::protocol::ModelChoice {
                    route: "openai::gpt-image-2.5-flare::high".into(),
                    group: "OpenAI".into(),
                    model: "gpt-image-2.5-flare".into(),
                    reasoning_effort: Some("high".into()),
                    variant_label: None,
                    context_window: None,
                    supports_image_input: false,
                    supports_image_generation: true,
                    supports_realtime_voice: false,
                    tool_discovery: crate::protocol::ToolDiscoveryMode::Rebuild,
                },
                Default::default(),
            )
            .expect("image tier route");
        let flare = router
            .image_model(Some("openai::gpt-image-2.5-flare::high"))
            .expect("quality route");
        assert_eq!(
            (flare.model, flare.quality),
            ("gpt-image-2.5-flare", Some("high"))
        );
        let middleware = ImageGeneration::new(Arc::new(router), store.clone(), None);
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
            None,
        );
        let mut unsupported_runtime = runtime.clone();
        unsupported_runtime.model_route = "unsupported".into();
        unsupported
            .register(&mut Catalog::default(), &unsupported_runtime)
            .expect("a router without image models registers no tool");
        let source = store
            .publish_artifact("session", "source.png".into(), "image/png".into(), &png)
            .await
            .expect("source image");
        let tool = GenerateImage {
            models: Arc::clone(&middleware.models),
            store: store.clone(),
            session_id: "session".into(),
            route: None,
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
                serde_json::json!({"prompt": "a red landscape", "image_aspect": "landscape", "reference_file_ids": [source.id]}),
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
        let artifacts = store
            .list_files(
                "session",
                &[crate::backend::session_files::SessionFileOrigin::Artifact],
            )
            .await
            .expect("artifacts");
        assert_eq!(artifacts.len(), 2);
        assert!(artifacts.iter().any(|(_, stored)| stored == &file));
        assert_eq!(
            store
                .list_files(
                    "session",
                    &[
                        crate::backend::session_files::SessionFileOrigin::Upload,
                        crate::backend::session_files::SessionFileOrigin::Artifact
                    ]
                )
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
                    arguments: serde_json::json!({"prompt": "a red landscape", "image_aspect": "landscape"}),
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
        assert_eq!(begin.image_aspect, Some(ImageAspect::Landscape));
        assert_eq!(
            serde_json::to_value(&begin).expect("pending wire")["image_aspect"],
            "landscape"
        );
        assert_eq!(end.state, FrontendBlockState::Complete);
        assert_eq!(end.files, vec![file]);

        let default: GenerateImageArgs =
            serde_json::from_value(serde_json::json!({"prompt": "x"})).expect("default shape");
        assert_eq!(default.image_aspect, ImageAspect::Square);
        assert!(
            serde_json::from_value::<GenerateImageArgs>(
                serde_json::json!({"prompt": "x", "image_aspect": "auto"})
            )
            .is_err()
        );

        let unsupported_tool = GenerateImage {
            models: Arc::clone(&unsupported.models),
            store,
            session_id: "session".into(),
            route: None,
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
            Err(Error::Config(_))
        ));
    }
}
