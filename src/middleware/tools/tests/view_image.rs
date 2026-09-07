use base64::Engine as _;

use super::*;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn view_image_call() -> ToolCall {
    ToolCall {
        call_id: "view".into(),
        name: "view_image".into(),
        arguments: serde_json::json!({"path": "page.png"}),
    }
}

async fn execute_view_image(bytes: &[u8]) -> ToolResult {
    execute_view_images(bytes, &[view_image_call()])
        .await
        .pop()
        .expect("tool result")
}

async fn execute_view_images(bytes: &[u8], calls: &[ToolCall]) -> Vec<ToolResult> {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("page.png"), bytes).expect("write image");
    let sandbox = Arc::new(Sandbox::new(
        Arc::new(
            crate::backend::sandbox::local::LocalSandbox::new(workspace.path())
                .expect("local sandbox"),
        ),
        crate::backend::sandbox::ApprovalPolicy::Ask,
    ));
    let mut catalog = Catalog::default();
    catalog
        .register(Arc::new(ViewImage))
        .expect("register view image");
    let calls = finalize_and_bind(&mut catalog, calls);

    execute_batch(&catalog, &calls, sandbox, &test_permissions(&[]), "turn").await
}

#[tokio::test]
async fn view_image_adds_workspace_image_to_model_input() {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(PNG)
        .expect("decode PNG");

    let result = execute_view_image(&bytes).await;

    assert_eq!(
        result,
        ToolResult {
            call_id: "view".into(),
            name: "view_image".into(),
            output: "viewed image page.png".into(),
            is_error: false,
            handler_executed: true,
            additional_input: vec![serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "input_image",
                    "media_type": "image/png",
                    "data": PNG
                }],
                (crate::protocol::INTERNAL_MESSAGE_FIELD): "view_image"
            })],
            events: Vec::new(),
        }
    );
}

#[tokio::test]
async fn view_image_rejects_non_images() {
    let result = execute_view_image(b"not an image").await;

    assert!(
        result.is_error
            && result.additional_input.is_empty()
            && result.output.contains("supported PNG, JPEG, WebP, or GIF")
    );
}

#[tokio::test]
async fn view_image_limits_each_tool_batch_to_one_image() {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(PNG)
        .expect("decode PNG");
    let mut second = view_image_call();
    second.call_id = "view-2".into();

    let results = execute_view_images(&bytes, &[view_image_call(), second]).await;

    assert_eq!(results.len(), 2);
    assert!(!results[0].is_error);
    assert_eq!(results[0].additional_input.len(), 1);
    assert!(results[1].is_error);
    assert!(results[1].additional_input.is_empty());
    assert!(results[1].output.contains("only one image"));
}
