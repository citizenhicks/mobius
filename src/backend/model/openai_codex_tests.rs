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

    write_credential(&path, credential)
        .expect("save credential")
        .1
        .expect("durable credential");

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
        OAuthCredential {
            access: format!("e30.{payload}.signature"),
            refresh: "refresh-token".into(),
            expires: u64::MAX,
            account_id: "account-123".into(),
        },
    )
    .expect("save credential")
    .1
    .expect("durable credential");
    let auth = ChatGptAuth::load(path).expect("load auth");

    let http = auth
        .authorize_http(false, Some("session-123"))
        .await
        .expect("HTTP authorization");
    assert_eq!(header(&http, "version"), Some("0.160.1"));
    assert_eq!(header(&http, "originator"), Some("mobius"));
    assert!(
        http.headers
            .iter()
            .filter(|(name, _)| ["originator", "version", "user-agent"].contains(name))
            .all(|(_, value)| matches!(value, Cow::Borrowed(_))),
        "static authorization values remain borrowed"
    );
    assert_eq!(header(&http, "session-id"), Some("session-123"));
    assert_eq!(header(&http, "thread-id"), Some("session-123"));
    assert_eq!(header(&http, "x-client-request-id"), None);
    assert_eq!(header(&http, "openai-beta"), None);

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

#[test]
fn credential_publication_keeps_applied_state_when_directory_sync_fails() {
    let directory = tempfile::tempdir().expect("auth directory");
    let path = directory.path().join("auth.json");
    let credential = OAuthCredential {
        access: "new-access".into(),
        refresh: "new-refresh".into(),
        account_id: "account".into(),
        expires: u64::MAX,
    };
    let access_pointer = credential.access.as_ptr();
    let (applied, durability) = write_credential_with_sync(&path, credential, |_| {
        Err(io::Error::other("injected directory sync failure"))
    })
    .expect("replacement occurred");
    assert_eq!(applied.access.as_ptr(), access_pointer);
    let saved: AuthFile =
        serde_json::from_slice(&fs::read(&path).expect("visible file")).expect("auth");
    assert_eq!(saved[PROVIDER_ID].access, applied.access);
    assert_eq!(saved[PROVIDER_ID].refresh, applied.refresh);
    let error = durability.expect_err("durability uncertainty must be reported");
    assert!(matches!(error, Error::PublicationDurability(_)));
    let event = crate::protocol::ErrorEvent::from_error(&error);
    assert!(!event.retryable);
    assert!(event.message.contains("state was replaced"));

    let rejected = OAuthCredential {
        access: "not-applied".into(),
        refresh: "not-applied".into(),
        account_id: "account".into(),
        expires: u64::MAX,
    };
    assert!(write_credential(&path.join("missing"), rejected).is_err());
    let unchanged: AuthFile =
        serde_json::from_slice(&fs::read(path).expect("unchanged file")).expect("auth");
    assert_eq!(unchanged[PROVIDER_ID].access, "new-access");
}
