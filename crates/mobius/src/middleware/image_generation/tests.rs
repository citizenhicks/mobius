use super::*;
use crate::backend::model::{GeneratedImage, Model, ModelEventSink, ModelOutput, ModelRequest};
use crate::backend::sandbox::local::LocalSandbox;
use crate::backend::sandbox::{
    ApprovalPolicy, NetworkAccess, Sandbox, SandboxMode, SandboxPermissions,
};
use crate::middleware::tools::ToolExposure;
use crate::protocol::{FrontendBlockState, TokenUsage, ToolCallBeginEvent, ToolCallEndEvent};

struct ImageModel(Vec<u8>, bool);

impl Model for ImageModel {
    fn supports_image_generation(&self) -> bool {
        self.1
    }

    fn supports_image_input(&self) -> bool {
        self.1
    }

    fn supports_tool_image_input(&self) -> bool {
        self.1
    }

    fn generate_image<'a>(
        &'a self,
        request: ImageGenerationRequest<'a>,
    ) -> BoxFuture<'a, Result<GeneratedImage>> {
        assert_eq!(request.model, "gpt-image-2.5-sunburst");
        assert_eq!(request.image_aspect, ImageAspect::Landscape);
        assert_eq!(request.size, Some("2K"));
        assert_eq!(request.quality, Some("high"));
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
    router.set_image_defaults(
        "openai::gpt-image-2.5-sunburst".into(),
        crate::backend::model::ImageDefaults {
            aspect: ImageAspect::Landscape,
            size: Some("2K".into()),
            quality: Some("high".into()),
        },
    );
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
        model_route: "image".into(),
        model: "image".into(),
        ..crate::middleware::tests::lifecycle_runtime(state.path())
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
        .publish_artifact(
            &"session".parse().expect("session ID"),
            "source.png".into(),
            "image/png".into(),
            &png,
        )
        .await
        .expect("source image");
    let tool = GenerateImage {
        models: Arc::clone(&middleware.models),
        store: Arc::new(store.clone()),
        session_id: "session".parse().expect("session ID"),
        route: None,
    };
    assert_eq!(tool.exposure(), ToolExposure::Deferred);
    assert_eq!(tool.approval(), ApprovalRequirement::Always);
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(LocalSandbox::new(state.path()).expect("sandbox")),
        ApprovalPolicy::Ask,
    ));
    let permissions = SandboxPermissions::restore(
        &"session".parse().expect("session ID"),
        SandboxMode::WorkspaceWrite,
        NetworkAccess::Allowed,
        ["image-call".into()],
    );
    crate::middleware::tools::Tools::new(Vec::new())
        .register(&mut catalog, &runtime)
        .expect("register tool discovery");
    catalog.finalize().expect("finalize image tools");
    let visible = ["generate_image".into()].into_iter().collect();
    let call = catalog.bind_call(
            crate::protocol::ToolCall {
                call_id: "image-call".into(),
                name: "generate_image".into(),
                arguments: serde_json::json!({"prompt":"a red landscape", "reference_file_ids":[source.id]}),
            },
            &visible,
            &visible,
        ).expect("bind image call");
    let mut results = crate::middleware::tools::execute_batch(
        &catalog,
        [call],
        Arc::clone(&sandbox),
        &permissions,
        &crate::middleware::tools::ToolStep {
            turn_id: &"turn".into(),
            model_step_id: Some(&"step".into()),
            model_route: &"image".into(),
            author: &crate::protocol::MessageAuthor::User.into(),
        },
    )
    .await;
    assert_eq!(results.len(), 1);
    let result = results.pop().expect("image result");
    assert!(!result.is_error, "{}", result.output.text());
    assert_eq!(
        result
            .usage
            .as_ref()
            .map(|reported| (&*reported.route, reported.usage.total_tokens)),
        Some(("openai::gpt-image-2.5-sunburst", 1))
    );
    let response = result.output;
    let Some(ContentPart::Image { image }) = response.0.first() else {
        panic!("generated image part");
    };
    assert!(image.width > 0 && image.height > 0);
    let file = response.files().next().expect("published file").clone();
    assert_eq!(
        store
            .read_file(&"session".parse().expect("session ID"), &file)
            .await
            .expect("image"),
        png
    );
    let artifacts = store
        .list_files(
            &"session".parse().expect("session ID"),
            &[crate::backend::session_files::SessionFileOrigin::Artifact],
        )
        .await
        .expect("artifacts");
    assert_eq!(artifacts.len(), 2);
    assert!(artifacts.iter().any(|(_, stored)| stored == &file));
    assert_eq!(
        store
            .list_files(
                &"session".parse().expect("session ID"),
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
                    model_step_id: None,
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
                model_step_id: None,
                turn_id: "turn".into(),
                call_id: "image-call".into(),
                name: "generate_image".into(),
                output: response,
                is_error: false,
            }),
            "session",
        )
        .expect("completed block");
    assert_eq!(begin.id, end.id);
    assert_eq!(begin.state, FrontendBlockState::Pending);
    assert_eq!(
        (begin.title.as_str(), begin.text.as_str()),
        ("Generating image", "")
    );
    assert_eq!(end.title, "Image ready");
    assert_eq!(begin.format, FrontendBlockFormat::Image);
    assert_eq!(end.state, FrontendBlockState::Complete);
    assert_eq!(end.files, vec![file.clone()]);
    assert!(matches!(
        end.content.0.as_slice(),
        [ContentPart::Image { image }] if image.file == file && image.width > 0
    ));

    let default: GenerateImageArgs =
        serde_json::from_value(serde_json::json!({"prompt": "x"})).expect("default shape");
    assert_eq!(default.image_aspect, None);
    assert!(
        serde_json::from_value::<GenerateImageArgs>(
            serde_json::json!({"prompt": "x", "image_aspect": "auto"})
        )
        .is_err()
    );

    let unsupported_tool = GenerateImage {
        models: Arc::clone(&unsupported.models),
        store: Arc::new(store),
        session_id: "session".parse().expect("session ID"),
        route: None,
    };
    let schema = &unsupported_tool.definition().parameters;
    assert_eq!(schema["properties"]["prompt"]["maxLength"], 32_000);
    assert_eq!(schema["properties"]["reference_file_ids"]["maxItems"], 4);
    let unsupported_context = ToolContext::new(sandbox, permissions.for_call("image-call"), "turn")
        .with_model_route("unsupported");
    assert!(matches!(
        unsupported_tool
            .call(
                unsupported_context,
                serde_json::json!({"prompt": "a red square"})
            )
            .await,
        Err(Error::Invalid(_))
    ));
}
