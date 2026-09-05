use super::*;

#[test]
fn provider_advertises_cross_device_login() {
    assert!(BROWSER_AUTH.supports_device_login());
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

#[cfg(unix)]
#[test]
fn saved_auth_is_owner_only() {
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
            (JWT_AUTH_CLAIM): {"chatgpt_account_id": "account-123"}
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
    assert_eq!(header(&compact, "version"), Some("0.153.4"));
    assert_eq!(header(&compact, "originator"), Some("mobius"));
    assert_eq!(header(&compact, "session-id"), Some("session-123"));
    assert_eq!(header(&compact, "thread-id"), Some("session-123"));
    assert_eq!(header(&compact, "x-client-request-id"), None);
    assert_eq!(header(&compact, "openai-beta"), None);

    let responses = auth
        .authorize_http(true, Some("session-123"))
        .await
        .expect("Responses authorization");
    assert_eq!(header(&responses, "version"), Some("0.153.4"));
    assert_eq!(
        header(&responses, "x-client-request-id"),
        Some("session-123")
    );
    assert_eq!(header(&responses, "openai-beta"), None);

    let websocket = auth
        .authorize_websocket("session-123")
        .await
        .expect("WebSocket authorization");
    assert_eq!(header(&websocket, "version"), Some("0.153.4"));
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

fn header<'a>(authorization: &'a ResolvedAuthorization, name: &str) -> Option<&'a str> {
    authorization
        .headers
        .iter()
        .find_map(|(header, value)| header.eq_ignore_ascii_case(name).then_some(value.as_str()))
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
