use super::*;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt};

#[tokio::test]
async fn model_transports_classify_send_failures_before_and_after_authorization() {
    use crate::backend::model::anthropic::Anthropic;
    use crate::backend::model::responses::OpenAi;
    use crate::backend::model::{Model, ModelRequest};

    let (listener, address) = crate::backend::model::test_support::loopback_listener().await;
    drop(listener);
    let base_url = || BaseUrl::new(format!("http://{address}/private-endpoint")).expect("URL");
    let models: [Box<dyn Model>; 3] = [
        Box::new(Anthropic::new("private-test-key", base_url(), "test-model").expect("Anthropic")),
        Box::new(OpenAi::new("private-test-key", base_url(), "test-model").expect("OpenAI")),
        Box::new(
            OpenAi::from_api_key(
                None,
                base_url(),
                "test-model".into(),
                ModelTransportSettings::default()
                    .streaming_client()
                    .expect("client"),
                ModelTransportSettings::default(),
            )
            .expect("credentialless Responses"),
        ),
    ];
    let input = vec![crate::backend::model::user_message("hello")];
    let request = ModelRequest {
        session_id: None,
        cancellation: None,
        prompt_cache: None,
        instructions: "",
        input: (&input).into(),
        catalog_revision: "test",
        tools: &[],
        deferred_tools: &[],
        allow_hosted_tools: false,
        allow_continuation: false,
    };
    for model in models {
        let error = model
            .respond(request, Arc::new(|_| Box::pin(async { Ok(()) })))
            .await
            .expect_err("connection refused");
        let event = crate::protocol::ErrorEvent::from_error(&error);
        assert!(matches!(error, Error::Provider(error) if error.is_retryable()));
        assert_eq!(event.status, None);
        assert_eq!(event.retry_after, None);
        assert!(
            event
                .message
                .contains("request connection failed: connection refused")
        );
        assert!(!event.message.contains("private-endpoint"));
        assert!(!event.message.contains("private-test-key"));
    }
}

#[tokio::test]
async fn invalid_request_configuration_is_terminal_and_omits_the_url() {
    let client = ModelTransportSettings::default()
        .streaming_client()
        .expect("client");
    let error = client
        .post("http://127.0.0.1/private?key=sensitive")
        .bearer_auth("invalid\ncredential")
        .send()
        .await
        .expect_err("invalid authorization header");
    assert!(error.is_builder());
    let error = request_error(error, "test");
    assert!(!crate::protocol::ErrorEvent::from_error(&error).retryable);
    assert!(matches!(&error, Error::Http(error) if error.url().is_none()));
}

#[tokio::test]
async fn tls_protocol_errors_remain_terminal() {
    let (listener, address) = crate::backend::model::test_support::loopback_listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("connection");
        let mut bytes = [0; 1_024];
        assert_ne!(stream.read(&mut bytes).await.expect("TLS client hello"), 0);
        stream
            .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
            .await
            .expect("invalid TLS response");
        std::future::pending::<()>().await;
    });
    let error = ModelTransportSettings::default()
        .streaming_client()
        .expect("client")
        .post(format!("https://{address}/private?key=sensitive"))
        .send()
        .await
        .expect_err("invalid TLS protocol");
    server.abort();
    assert!(error.is_connect());
    let error = request_error(error, "test");
    assert!(!crate::protocol::ErrorEvent::from_error(&error).retryable);
    assert!(matches!(&error, Error::Http(error) if error.url().is_none()));
}

#[test]
fn transport_settings_validate_once_on_construction_and_decoding() {
    let settings = ModelTransportSettings::default();
    assert_eq!(
        ModelTransportSettings::try_from(*settings).ok(),
        Some(settings)
    );
    let wire = serde_json::to_value(settings).expect("encode");
    assert_eq!(wire, serde_json::to_value(*settings).expect("encode raw"));
    assert_eq!(
        serde_json::from_value::<ModelTransportSettings>(wire).ok(),
        Some(settings)
    );
    assert!(
        serde_json::from_str::<ModelTransportSettings>(r#"{"http_idle_timeout_ms":0}"#).is_err()
    );
    let inverted = RawModelTransportSettings {
        stream_retry_backoff_ms: 2,
        stream_retry_max_backoff_ms: 1,
        ..*settings
    };
    assert!(ModelTransportSettings::try_from(inverted).is_err());
}

#[test]
fn quota_errors_are_terminal_while_transient_limits_remain_retryable() {
    for (code, retryable) in [
        ("rate_limit_exceeded", true),
        ("insufficient_quota", false),
        ("billing_hard_limit_reached", false),
    ] {
        let body = serde_json::json!({"error":{"code":code,"message":"Limit reached"}});
        let error = http_provider_error(
            "Responses",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            &serde_json::to_vec(&body).expect("JSON error"),
            Some("3".into()),
        );
        assert_eq!(error.is_retryable(), retryable);
        assert_eq!(error.status(), Some(429));
        assert_eq!(error.retry_after(), Some("3"));
    }
}

#[test]
fn provider_failures_are_classified_by_status_code_and_message() {
    use crate::FailureClass::*;
    for (status, body, class) in [
        (401, serde_json::json!({}), Authentication),
        (402, serde_json::json!({}), OutOfCredits),
        (403, serde_json::json!({}), Forbidden),
        (
            400,
            serde_json::json!({"error":{"message":"This request requires more credits"}}),
            OutOfCredits,
        ),
        (
            429,
            serde_json::json!({"error":{"code":"insufficient_quota"}}),
            OutOfCredits,
        ),
        (503, serde_json::json!({}), Transient),
        (
            529,
            serde_json::json!({"error":{"type":"overloaded_error"}}),
            Transient,
        ),
        (
            400,
            serde_json::json!({"error":{"code":"context_length_exceeded"}}),
            ContextLengthExceeded,
        ),
        (
            400,
            serde_json::json!({"error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens"}}),
            ContextLengthExceeded,
        ),
        (
            400,
            serde_json::json!({"error":{"code":"content_policy_violation"}}),
            ContentPolicy,
        ),
        (
            400,
            serde_json::json!({"error":{"message":"bad tool schema"}}),
            InvalidRequest,
        ),
    ] {
        let error = http_provider_error(
            "Test",
            reqwest::StatusCode::from_u16(status).expect("status"),
            &serde_json::to_vec(&body).expect("JSON error"),
            None,
        );
        assert_eq!(error.class(), class, "{status} {body}");
    }
}

#[test]
fn sse_framing_handles_one_byte_chunks_fragmented_utf8_and_a_leading_bom() {
    let input = "\u{feff}data: héllo\r\n\r\ndata: [DONE]\n\n".as_bytes();
    let mut decoder = SseDecoder::default();
    for chunk in input.chunks(1) {
        decoder.push(chunk, "test").expect("valid chunk");
    }

    assert_eq!(
        frame_data(
            decoder
                .next_frame()
                .expect("valid frame")
                .expect("UTF-8 frame")
        )
        .as_deref(),
        Some("héllo")
    );
    assert_eq!(
        frame_data(
            decoder
                .next_frame()
                .expect("valid frame")
                .expect("DONE frame")
        )
        .as_deref(),
        Some("[DONE]")
    );
    assert!(decoder.next_frame().expect("EOF").is_none());
}

#[test]
fn sse_data_borrows_single_lines_and_preserves_multiline_payloads() {
    for (frame, expected, borrowed) in [
        (": keepalive\nevent: message", None, false),
        ("data:", Some(""), true),
        (
            "event: message\r\ndata:\t héllo  \r\nid: 4",
            Some("héllo  "),
            true,
        ),
        ("data: [DONE]\n", Some("[DONE]"), true),
        (
            "data: first\r\ndata: second\r\n",
            Some("first\nsecond"),
            false,
        ),
        ("data:\ndata: second\ndata:", Some("\nsecond\n"), false),
        (
            "data: first\nignored\ndata:\u{2003}second",
            Some("first\nsecond"),
            false,
        ),
    ] {
        let payload = frame_data(frame);
        assert_eq!(payload.as_deref(), expected, "{frame:?}");
        assert_eq!(
            matches!(payload, Some(Cow::Borrowed(_))),
            borrowed,
            "{frame:?}"
        );
    }
}

#[test]
fn sse_framing_accepts_fragmented_delimiters_at_the_frame_limit() {
    let frame = vec![b'x'; MAX_SSE_FRAME_BYTES];
    for delimiter in [b"\n\n".as_slice(), b"\r\n\r\n"] {
        for split in 1..delimiter.len() {
            let mut decoder = SseDecoder::default();
            decoder.push(&frame, "test").expect("stream limit");
            decoder
                .push(&delimiter[..split], "test")
                .expect("stream limit");
            assert!(decoder.next_frame().expect("partial delimiter").is_none());
            assert!(decoder.next_frame().expect("unfinished at EOF").is_none());
            decoder
                .push(&delimiter[split..], "test")
                .expect("stream limit");
            assert_eq!(
                decoder
                    .next_frame()
                    .expect("complete delimiter")
                    .map(str::len),
                Some(MAX_SSE_FRAME_BYTES)
            );

            let mut decoder = SseDecoder::default();
            decoder.push(&frame, "test").expect("stream limit");
            decoder
                .push(&delimiter[..split], "test")
                .expect("stream limit");
            assert!(decoder.next_frame().expect("partial delimiter").is_none());
            decoder.push(b"x", "test").expect("stream limit");
            assert!(decoder.next_frame().is_err());
        }
    }
}

#[tokio::test]
async fn authenticated_streaming_clients_do_not_follow_redirects() {
    use tokio::io::AsyncWriteExt as _;
    let (destination, target) = crate::backend::model::test_support::loopback_listener().await;
    let (source, address) = crate::backend::model::test_support::loopback_listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = source.accept().await.expect("source connection");
        crate::backend::model::test_support::read_http_request(&mut stream).await;
        stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://{target}/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.expect("redirect response");
    });
    let response = ModelTransportSettings::default()
        .streaming_client()
        .expect("client")
        .get(format!("http://{address}"))
        .bearer_auth("test-token")
        .header("chatgpt-account-id", "test-account")
        .send()
        .await
        .expect("first response");
    assert_eq!(response.status(), reqwest::StatusCode::FOUND);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), destination.accept())
            .await
            .is_err()
    );
    server.await.expect("source server");
}
