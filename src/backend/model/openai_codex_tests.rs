use super::*;

#[test]
fn token_failures_explain_expired_sessions_without_exposing_response_bodies() {
    for (code, expected) in [
        (
            "refresh_token_expired",
            "ChatGPT session expired; sign in again",
        ),
        (
            "refresh_token_reused",
            "ChatGPT session is no longer valid; sign in again",
        ),
        (
            "refresh_token_invalidated",
            "ChatGPT session is no longer valid; sign in again",
        ),
        (
            "unknown",
            "ChatGPT token refresh failed with HTTP 401 Unauthorized",
        ),
    ] {
        let body =
            serde_json::json!({"error": {"code": code, "message": "sensitive-provider-data"}});
        let error = token_failure(
            StatusCode::UNAUTHORIZED,
            "refresh",
            body.to_string().as_bytes(),
        );
        assert!(matches!(error, Error::Auth(message) if message == expected));
    }
    for (status, operation, body, expected) in [
        (
            StatusCode::UNAUTHORIZED,
            "refresh",
            b"invalid-json".as_slice(),
            "ChatGPT token refresh failed with HTTP 401 Unauthorized",
        ),
        (
            StatusCode::BAD_GATEWAY,
            "refresh",
            br#"{"error":{"code":"refresh_token_expired"}}"#.as_slice(),
            "ChatGPT token refresh failed with HTTP 502 Bad Gateway",
        ),
        (
            StatusCode::UNAUTHORIZED,
            "exchange",
            br#"{"error":{"code":"refresh_token_expired"}}"#.as_slice(),
            "ChatGPT token exchange failed with HTTP 401 Unauthorized",
        ),
    ] {
        let error = token_failure(status, operation, body);
        assert!(matches!(error, Error::Auth(message) if message == expected));
    }
}

#[test]
fn provider_advertises_cross_device_login() {
    assert!(BROWSER_AUTH.supports_device_login());
}

#[test]
fn embedded_auth_metadata_preserves_native_login_and_account_routes() {
    assert_eq!(
        [
            MANIFEST.label.as_str(),
            MANIFEST.client_id.as_str(),
            MANIFEST.authorize_url.as_str(),
            MANIFEST.token_url.as_str(),
            MANIFEST.redirect_uri.as_str(),
            MANIFEST.device_user_code_url.as_str(),
            MANIFEST.device_token_url.as_str(),
            MANIFEST.device_verification_url.as_str(),
            MANIFEST.device_redirect_uri.as_str(),
            MANIFEST.usage_url.as_str(),
            MANIFEST.scope.as_str(),
            MANIFEST.jwt_auth_claim.as_str(),
        ],
        [
            "ChatGPT",
            "app_EMoamEEZ73f0CkXaXp7hrann",
            "https://auth.openai.com/oauth/authorize",
            "https://auth.openai.com/oauth/token",
            "http://localhost:1455/auth/callback",
            "https://auth.openai.com/api/accounts/deviceauth/usercode",
            "https://auth.openai.com/api/accounts/deviceauth/token",
            "https://auth.openai.com/codex/device",
            "https://auth.openai.com/deviceauth/callback",
            "https://chatgpt.com/backend-api/wham/usage",
            "openid profile email offline_access",
            "https://api.openai.com/auth",
        ]
    );
}

#[tokio::test]
async fn usage_reader_validates_configured_transport_before_reading_credentials() {
    let settings = super::super::super::ModelTransportSettings {
        oauth_request_timeout_ms: 0,
        ..Default::default()
    };
    let error = BROWSER_AUTH
        .usage_limits_with_transport(Path::new("missing-credential.json"), settings)
        .expect("provider supports usage reporting")
        .await
        .expect_err("invalid authentication deadline");
    assert!(matches!(error, Error::Config(_)));
}

#[test]
fn device_code_response_accepts_the_upstream_usercode_alias() {
    let response: DeviceUserCodeResponse = serde_json::from_value(serde_json::json!({
        "device_auth_id": "device-1",
        "usercode": "ABCD-1234",
        "interval": "5"
    }))
    .expect("device-code response");

    assert_eq!(
        (
            response.device_auth_id.as_str(),
            response.user_code.as_str()
        ),
        ("device-1", "ABCD-1234")
    );
}

#[test]
fn usage_parser_flattens_present_windows_without_inventing_missing_ones() {
    let limits = parse_usage_limits(serde_json::json!({
        "rate_limit": {
            "primary_window": {
                "used_percent": 25,
                "limit_window_seconds": 300,
                "reset_at": 123
            },
            "secondary_window": null
        },
        "additional_rate_limits": [{
            "metered_feature": "codex-terra",
            "limit_name": "gpt-5.6-terra",
            "rate_limit": {
                "secondary_window": {
                    "used_percent": 50,
                    "limit_window_seconds": 3600,
                    "reset_at": null
                }
            }
        }]
    }))
    .expect("usage limits");

    assert_eq!(
        limits,
        vec![
            UsageLimit {
                id: "codex:primary".into(),
                label: "Codex".into(),
                remaining_fraction: 0.75,
                window_seconds: 300,
                resets_at: Some(123),
            },
            UsageLimit {
                id: "codex-terra:secondary".into(),
                label: "gpt-5.6-terra".into(),
                remaining_fraction: 0.5,
                window_seconds: 3600,
                resets_at: None,
            },
        ]
    );
    assert!(
        parse_usage_limits(serde_json::json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 101,
                    "limit_window_seconds": 300
                }
            }
        }))
        .is_err()
    );
    for (name, payload) in [
        (
            "nonobject additional bucket",
            serde_json::json!({"additional_rate_limits": [42]}),
        ),
        (
            "missing metered feature",
            serde_json::json!({
                "additional_rate_limits": [{"limit_name": "extra"}]
            }),
        ),
        (
            "invalid limit name",
            serde_json::json!({
                "additional_rate_limits": [{
                    "metered_feature": "extra",
                    "limit_name": null
                }]
            }),
        ),
        (
            "duplicate main bucket",
            serde_json::json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 1,
                        "limit_window_seconds": 60
                    }
                },
                "additional_rate_limits": [{
                    "metered_feature": "codex",
                    "limit_name": "extra",
                    "rate_limit": {
                        "primary_window": {
                            "used_percent": 2,
                            "limit_window_seconds": 60
                        }
                    }
                }]
            }),
        ),
        (
            "reset outside Unix calendar range",
            serde_json::json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 1,
                        "limit_window_seconds": 60,
                        "reset_at": 253402300800_i64
                    }
                }
            }),
        ),
    ] {
        assert!(parse_usage_limits(payload).is_err(), "{name}");
    }
}

#[cfg(unix)]
#[test]
fn saved_auth_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("auth.json");
    let credential = OAuthCredential {
        access: "access-token".into(),
        refresh: "refresh-token".into(),
        expires: u64::MAX,
        account_id: "account-123".into(),
    };

    write_credential(&path, &credential).expect("save credential");

    assert_eq!(
        fs::metadata(path)
            .expect("auth metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[tokio::test]
async fn codex_requests_include_session_and_thread_identity() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("auth.json");
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({
            (MANIFEST.jwt_auth_claim.as_str()): {"chatgpt_account_id": "account-123"}
        }))
        .expect("JWT payload"),
    );
    write_credential(
        &path,
        &OAuthCredential {
            access: format!("e30.{payload}.signature"),
            refresh: "refresh-token".into(),
            expires: u64::MAX,
            account_id: "account-123".into(),
        },
    )
    .expect("save credential");
    let auth = ChatGptAuth::load(path).expect("load auth");

    let compact = auth
        .authorize_http(false, Some("session-123"))
        .await
        .expect("compaction authorization");
    assert_eq!(header(&compact, "version"), Some("0.160.1"));
    assert_eq!(header(&compact, "originator"), Some("mobius"));
    assert!(
        compact
            .headers
            .iter()
            .filter(|(name, _)| ["originator", "version", "user-agent"].contains(name))
            .all(|(_, value)| matches!(value, Cow::Borrowed(_))),
        "static authorization values remain borrowed"
    );
    assert_eq!(header(&compact, "session-id"), Some("session-123"));
    assert_eq!(header(&compact, "thread-id"), Some("session-123"));
    assert_eq!(header(&compact, "x-client-request-id"), None);
    assert_eq!(header(&compact, "openai-beta"), None);

    let responses = auth
        .authorize_http(true, Some("session-123"))
        .await
        .expect("Responses authorization");
    assert_eq!(header(&responses, "version"), Some("0.160.1"));
    assert_eq!(
        header(&responses, "x-client-request-id"),
        Some("session-123")
    );
    assert_eq!(header(&responses, "openai-beta"), None);

    let websocket = auth
        .authorize_websocket("session-123")
        .await
        .expect("WebSocket authorization");
    assert_eq!(header(&websocket, "version"), Some("0.160.1"));
    assert_eq!(header(&websocket, "originator"), Some("mobius"));
    assert_eq!(header(&websocket, "session-id"), Some("session-123"));
    assert_eq!(header(&websocket, "thread-id"), Some("session-123"));
    assert_eq!(
        header(&websocket, "x-client-request-id"),
        Some("session-123")
    );
    assert_eq!(
        header(&websocket, "openai-beta"),
        Some("responses_websockets=2026-02-06")
    );
}

#[tokio::test]
async fn usage_limits_read_saved_auth_and_mocked_http() {
    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;

    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("auth.json");
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({
            (MANIFEST.jwt_auth_claim.as_str()): {"chatgpt_account_id": "account-123"}
        }))
        .expect("JWT payload"),
    );
    write_credential(
        &path,
        &OAuthCredential {
            access: format!("e30.{payload}.signature"),
            refresh: "refresh-token".into(),
            expires: u64::MAX,
            account_id: "account-123".into(),
        },
    )
    .expect("save credential");

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("usage listener");
    let address = listener.local_addr().expect("usage address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("usage connection");
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut chunk = [0; 1_024];
            let count = stream.read(&mut chunk).await.expect("usage request");
            assert_ne!(count, 0, "usage request ended before headers");
            request.extend_from_slice(&chunk[..count]);
        }
        let body = serde_json::json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 20,
                    "limit_window_seconds": 300,
                    "reset_at": 123
                }
            }
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("usage response");
        String::from_utf8(request).expect("usage request UTF-8")
    });
    let auth = ChatGptAuth::load(path).expect("load auth");

    let limits = auth
        .usage_limits_at(&format!("http://{address}/usage"))
        .await
        .expect("usage limits");

    assert_eq!(limits[0].id, "codex:primary");
    let request = server.await.expect("usage server");
    assert!(request.starts_with("GET /usage HTTP/1.1\r\n"));
    let request = request.to_ascii_lowercase();
    assert!(request.contains("authorization: bearer e30."));
    assert!(request.contains("chatgpt-account-id: account-123"));
}

fn header<'a>(authorization: &'a ResolvedAuthorization<'_>, name: &str) -> Option<&'a str> {
    authorization
        .headers
        .iter()
        .find_map(|(header, value)| header.eq_ignore_ascii_case(name).then_some(value.as_ref()))
}

#[tokio::test]
async fn callback_rejects_wrong_state_then_accepts_the_expected_state() {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("callback listener");
    let address = listener.local_addr().expect("callback address");
    let callback = tokio::spawn(wait_for_callback(listener, "expected"));

    let mut wrong = TcpStream::connect(address).await.expect("wrong callback");
    wrong
        .write_all(b"GET /auth/callback?code=wrong&state=other HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("write wrong callback");
    let mut response = Vec::new();
    wrong
        .read_to_end(&mut response)
        .await
        .expect("read wrong response");
    assert!(response.starts_with(b"HTTP/1.1 400"));

    let mut correct = TcpStream::connect(address).await.expect("correct callback");
    correct
        .write_all(
            b"GET /auth/callback?code=accepted&state=expected HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .expect("write correct callback");

    assert_eq!(
        callback
            .await
            .expect("callback task")
            .expect("callback result"),
        "accepted"
    );
}

#[tokio::test]
async fn browser_login_completion_obeys_configured_deadline_and_releases_callback() {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("callback listener");
    let address = listener.local_addr().expect("callback address");
    let settings = super::super::super::ModelTransportSettings {
        oauth_callback_timeout_ms: 1,
        ..Default::default()
    };
    let login = BrowserLogin {
        settings,
        listener,
        verifier: "verifier".into(),
        state: "state".into(),
        url: "https://example.invalid/authorize".into(),
        client: http_client_with_settings(&settings).expect("HTTP client"),
    };
    let error = login
        .complete(PathBuf::from("unused-credential.json"))
        .await
        .expect_err("missing callback must time out");
    assert!(matches!(error, Error::Auth(message) if message == "ChatGPT login timed out"));
    TcpListener::bind(address)
        .await
        .expect("completion timeout must release the callback listener");
}

#[tokio::test]
async fn callback_timeout_bounds_the_whole_connection() {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("callback listener");
    let address = listener.local_addr().expect("callback address");
    let client = tokio::spawn(async move {
        let mut stream = TcpStream::connect(address).await.expect("callback client");
        stream.write_all(b"G").await.expect("partial callback");
        std::future::pending::<()>().await;
    });
    let (mut stream, _) = listener.accept().await.expect("callback connection");

    let error = read_callback_request_with_timeout(&mut stream, Duration::from_millis(10))
        .await
        .expect_err("partial callback should time out");
    client.abort();

    assert_eq!(
        error.to_string(),
        "authentication error: OAuth callback request timed out"
    );
}

#[tokio::test]
async fn callback_limit_applies_before_the_header_delimiter() {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("callback listener");
    let address = listener.local_addr().expect("callback address");
    let client = tokio::spawn(async move {
        let mut stream = TcpStream::connect(address).await.expect("callback client");
        let mut request = vec![b'X'; CALLBACK_LIMIT];
        request.extend_from_slice(b"\r\n\r\n");
        stream
            .write_all(&request)
            .await
            .expect("oversized callback");
    });
    let (mut stream, _) = listener.accept().await.expect("callback connection");

    let error = read_callback_request(&mut stream)
        .await
        .expect_err("oversized callback should fail");
    client.await.expect("callback client");

    assert_eq!(
        error.to_string(),
        "authentication error: OAuth callback request was too large"
    );
}

#[test]
fn credentials_refresh_with_clock_skew_leeway() {
    let credential = OAuthCredential {
        access: "access-token".into(),
        refresh: "refresh-token".into(),
        expires: now() + REFRESH_LEEWAY.as_secs() - 1,
        account_id: "account-123".into(),
    };

    assert!(expired(&credential));
}

#[test]
fn rejected_credentials_refresh_before_expiry() {
    let credential = OAuthCredential {
        access: "rejected-token".into(),
        refresh: "refresh-token".into(),
        expires: u64::MAX,
        account_id: "account-123".into(),
    };

    assert!(refresh_required(&credential, Some("rejected-token")));
    assert!(!refresh_required(&credential, Some("newer-token")));
}

#[tokio::test]
async fn codex_custom_root_routes_http_fallback_to_proxy() {
    use crate::backend::model::provider::{HostedWebSearch, ProviderBuildConfig};
    use crate::backend::model::{ModelRequest, ModelTransportSettings};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let directory = tempfile::tempdir().expect("auth directory");
    let path = directory.path().join("auth.json");
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({
            (MANIFEST.jwt_auth_claim.as_str()): {"chatgpt_account_id": "proxy-account"}
        }))
        .expect("JWT payload"),
    );
    let access = format!("e30.{payload}.signature");
    let credential = OAuthCredential {
        access,
        refresh: "unused-refresh".into(),
        expires: u64::MAX,
        account_id: "proxy-account".into(),
    };
    write_credential(&path, &credential).expect("stored credential");
    let access = credential.access;
    let auth = BROWSER_AUTH.load(&path).expect("browser credential");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("proxy listener");
    let address = listener.local_addr().expect("proxy address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("proxy connection");
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut chunk = [0; 1024];
            let count = stream.read(&mut chunk).await.expect("proxy request");
            assert_ne!(count, 0);
            request.extend_from_slice(&chunk[..count]);
        }
        let request = String::from_utf8(request).expect("HTTP UTF-8");
        assert!(request.starts_with("POST /tenant/native/responses HTTP/1.1"));
        assert!(request.contains(&format!("authorization: Bearer {access}")));
        assert!(request.contains("chatgpt-account-id: proxy-account"));
        stream
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("proxy response");
    });
    let settings = ModelTransportSettings {
        stream_retry_limit: 0,
        ..ModelTransportSettings::default()
    };
    let model = super::super::provider()
        .build(ProviderBuildConfig {
            credential: auth,
            model: "gpt-6.1-sol".into(),
            base_url: Some(format!("http://{address}/tenant/native/")),
            reasoning_effort: None,
            service_tier: None,
            web_search: HostedWebSearch::Off,
            http: settings.streaming_client().expect("HTTP client"),
            transport: settings,
        })
        .expect("Codex proxy provider");
    assert!(model.supports_realtime_voice());
    assert!(model.supports_image_generation());
    assert_eq!(model.transport_settings(), settings);
    model
        .fallback_transport("proxy-session")
        .await
        .expect("fallback selection");
    let error = model
        .respond(
            ModelRequest {
                session_id: "proxy-session",
                prompt_cache: None,
                instructions: "test",
                input: &[],
                catalog_revision: "test",
                tools: &[],
                deferred_tools: &[],
                allow_hosted_tools: false,
                allow_continuation: false,
            },
            Arc::new(|_| Box::pin(async { Ok(()) })),
        )
        .await
        .expect_err("proxy denial");
    assert!(matches!(error, Error::Provider(ref error) if error.status() == Some(403)));
    server.await.expect("proxy assertion");
}
