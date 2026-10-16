use super::super::*;

#[tokio::test]
async fn handshake_rejection_preserves_status_and_payment_message() {
    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;

    for (status, body) in [
        (426, ""),
        (402, "Balance exhausted"),
        (
            429,
            r#"{"error":{"code":"rate_limit_exceeded","type":"insufficient_quota","message":"Quota exhausted"}}"#,
        ),
        (
            503,
            r#"{"error":{"message":"Cloud model service unavailable"}}"#,
        ),
    ] {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("WebSocket listener");
        let address = listener.local_addr().expect("WebSocket address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("WebSocket connection");
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut chunk = [0; 1_024];
                let count = stream.read(&mut chunk).await.expect("handshake request");
                assert_ne!(count, 0, "request ended before its headers");
                request.extend_from_slice(&chunk[..count]);
            }
            stream
            .write_all(
                format!("HTTP/1.1 {status} Rejected\r\nRetry-After: 7\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes(),
            )
            .await
            .expect("handshake rejection");
        });
        let auth = ApiKeyAuthorization::new("test-key".into());

        let error = match connect(
            &auth,
            &format!("ws://{address}/responses"),
            "session",
            &crate::backend::model::ModelTransportSettings::default(),
        )
        .await
        {
            Ok(connection) => {
                connection.close().await;
                panic!("handshake unexpectedly succeeded");
            }
            Err(error) => error,
        };
        server.await.expect("WebSocket server");

        let Error::Provider(error) = error else {
            panic!("expected provider error");
        };
        assert_eq!(error.status(), Some(status));
        assert_eq!(error.is_retryable(), status == 503);
        assert_eq!(error.retry_after(), Some("7"));
        if status == 503 {
            assert_eq!(
                error.to_string(),
                "WebSocket HTTP 503 Service Unavailable: Cloud model service unavailable"
            );
        }
        if status == 402 {
            assert!(error.to_string().contains("Balance exhausted"));
        }
        assert!(!error.is_stream_interrupted());
    }
}

struct RefreshingAuthorization {
    token: Mutex<String>,
    authorizations: Mutex<Vec<String>>,
    refreshes: std::sync::atomic::AtomicUsize,
}

impl RefreshingAuthorization {
    fn new() -> Self {
        Self {
            token: Mutex::new("rejected-token".into()),
            authorizations: Mutex::new(Vec::new()),
            refreshes: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn resolve(&self) -> BoxFuture<'_, Result<ResolvedAuthorization<'_>>> {
        Box::pin(async move {
            let token = self.token.lock().await.clone();
            self.authorizations.lock().await.push(token.clone());
            Ok(ResolvedAuthorization {
                token: token.into(),
                headers: Vec::new(),
            })
        })
    }
}

impl OpenAiAuthorization for RefreshingAuthorization {
    fn authorize_http<'a>(
        &'a self,
        _streaming: bool,
        _session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization<'a>>> {
        self.resolve()
    }

    fn authorize_websocket<'a>(
        &'a self,
        _session_id: &'a str,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization<'a>>> {
        self.resolve()
    }

    fn recover_unauthorized<'a>(&'a self, rejected_token: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let mut token = self.token.lock().await;
            if token.as_str() == rejected_token {
                *token = "fresh-token".into();
                self.refreshes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(true)
        })
    }
}

#[tokio::test]
async fn websocket_unauthorized_refreshes_and_retries_once() {
    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("WebSocket listener");
    let address = listener.local_addr().expect("WebSocket address");
    let server = tokio::spawn(async move {
        let (mut rejected, _) = listener.accept().await.expect("rejected connection");
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut chunk = [0; 1_024];
            let count = rejected.read(&mut chunk).await.expect("handshake request");
            assert_ne!(count, 0, "handshake ended before its headers");
            request.extend_from_slice(&chunk[..count]);
        }
        assert!(
            String::from_utf8_lossy(&request).contains("Bearer rejected-token"),
            "first handshake should use the rejected token"
        );
        rejected
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("unauthorized response");

        let (accepted, _) = listener.accept().await.expect("retried connection");
        tokio_tungstenite::accept_async(accepted)
            .await
            .expect("retried WebSocket handshake")
    });

    let auth = RefreshingAuthorization::new();
    let socket_url = format!("ws://{address}/responses");
    let socket = connect(
        &auth,
        &socket_url,
        "session-1",
        &crate::backend::model::ModelTransportSettings::default(),
    )
    .await
    .expect("connection should recover");
    drop(socket);
    drop(server.await.expect("WebSocket server"));

    assert_eq!(
        auth.authorizations.lock().await.as_slice(),
        ["rejected-token", "fresh-token"]
    );
    assert_eq!(auth.refreshes.load(std::sync::atomic::Ordering::Relaxed), 1);
}
