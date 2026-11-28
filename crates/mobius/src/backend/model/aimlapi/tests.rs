use serde_json::json;
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;

use super::*;
use crate::backend::model::ModelTransportSettings;
use crate::backend::model::chat_completions::{ChatStream, TextStream};
use crate::backend::model::provider::HostedWebSearch;
use crate::backend::model::test_support::{loopback_listener, read_http_request, record_events};
use crate::backend::model::transport::SseHandler;
use crate::protocol::ModelEvent;

fn build(base_url: BaseUrl, credential: ProviderCredential) -> Result<Arc<dyn Model>> {
    let http = ModelTransportSettings::default().streaming_client()?;
    provider().build(ProviderBuildConfig {
        capability: None,
        tool_discovery: None,
        credential,
        model: CATALOG.default_model.clone().expect("default model"),
        base_url: Some(base_url),
        reasoning_effort: None,
        service_tier: None,
        web_search: HostedWebSearch::Off,
        http,
        transport: ModelTransportSettings::default(),
    })
}

#[test]
fn api_key_is_sent_only_to_the_exact_aimlapi_host() {
    for allowed in [
        "https://api.aimlapi.com/v1",
        "https://api.aimlapi.com:443/v1/",
    ] {
        assert!(
            build(
                BaseUrl::new(allowed).unwrap(),
                ProviderCredential::ApiKey("test-key".into())
            )
            .is_ok(),
            "{allowed}"
        );
    }
    for foreign in [
        "https://api.aimlapi.com.example.net/v1",
        "https://aimlapi.com/v1",
        "https://evil.example/api.aimlapi.com/v1",
        "https://api.aimlapi.com:8443/v1",
        "https://tools.aimlapi.com/v1",
        "http://localhost/v1",
    ] {
        let error = build(
            BaseUrl::new(foreign).unwrap(),
            ProviderCredential::ApiKey("test-key".into()),
        )
        .err()
        .unwrap_or_else(|| panic!("{foreign} must not receive the key"));
        assert!(
            error
                .to_string()
                .contains("only to https://api.aimlapi.com")
        );
    }
    assert!(!provider().supports_credentialless_endpoints());
    assert!(
        build(
            BaseUrl::new("https://proxy.example/v1").unwrap(),
            ProviderCredential::Credentialless
        )
        .is_err(),
        "no keyless custom endpoints"
    );
}

#[test]
fn bound_endpoints_belong_to_the_provider_that_created_them() {
    use crate::backend::model::endpoints::ProviderModelKind;
    use crate::backend::model::provider::provider as definition;

    let base = provider().default_base_url().share();
    for id in ["aimlapi", "responses"] {
        let endpoint = definition(id)
            .unwrap()
            .with_group_endpoint(
                ProviderModelKind::Language,
                &base,
                "chat_completions",
                "chat/completions",
            )
            .unwrap();
        let built = build(endpoint, ProviderCredential::ApiKey("test-key".into()));
        if id == "aimlapi" {
            assert!(built.is_ok());
        } else {
            assert!(
                matches!(built, Err(Error::Invalid(message)) if message.contains("different provider"))
            );
        }
    }
}

#[tokio::test]
async fn api_key_does_not_follow_a_redirect_to_another_host() {
    let (destination, target) = loopback_listener().await;
    let (source, address) = loopback_listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = source.accept().await.expect("source connection");
        let request = read_http_request(&mut stream).await;
        stream
            .write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{target}/v1/chat/completions\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes())
            .await
            .expect("redirect response");
        request
    });
    let client = ModelTransportSettings::default()
        .streaming_client()
        .unwrap();
    let base = BaseUrl::new(format!("http://{address}/v1")).unwrap();
    super::super::chat_completions::post(
        &client,
        &base,
        Some("test-key"),
        b"{}".to_vec(),
        "AI/ML API",
        None,
    )
    .await
    .expect_err("a redirect is not followed");

    let request = server.await.expect("server").to_ascii_lowercase();
    assert!(request.starts_with("post /v1/chat/completions http/1.1\r\n"));
    assert!(request.contains("authorization: bearer test-key"));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), destination.accept())
            .await
            .is_err(),
        "the redirect target received a connection"
    );
}

#[tokio::test]
async fn stream_normalizes_text_tools_and_usage() {
    let (events, seen) = record_events();
    let mut stream = TextStream::default();
    for chunk in [
        json!({"choices": [{"delta": {"reasoning_content": "Plan.", "content": "Reading."}}], "usage": null}),
        json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": "call-1",
            "function": {"name": "read", "arguments": "{\"path\":\"README.md\"}"}
        }]}}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17}}),
    ] {
        stream
            .apply_data(&chunk.to_string(), &events)
            .await
            .expect("chunk");
    }
    stream.apply_data("[DONE]", &events).await.expect("done");

    let output = stream.finish().expect("output");
    assert_eq!(output.tool_calls()[0].arguments["path"], "README.md");
    assert_eq!(output.usage().input_tokens, 12);
    assert_eq!(output.usage().output_tokens, 5);
    assert!(matches!(
        seen.lock().expect("events").as_slice(),
        [ModelEvent::ReasoningDelta(_), ModelEvent::TextDelta(_)]
    ));
}
