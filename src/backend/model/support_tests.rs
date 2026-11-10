//! Loopback HTTP capture and credential fixtures shared by provider tests.

use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use super::authorization::{OpenAiAuthorization, ResolvedAuthorization};
use crate::BoxFuture;
use crate::Result;

/// Reads one HTTP/1.1 request, including any `Content-Length` body, as text.
pub(super) async fn read_http_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let header_end = loop {
        if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break index + 4;
        }
        let mut chunk = [0; 4_096];
        let count = stream.read(&mut chunk).await.expect("HTTP request");
        assert_ne!(count, 0, "HTTP request ended before its headers");
        request.extend_from_slice(&chunk[..count]);
    };
    let content_length = String::from_utf8_lossy(&request[..header_end])
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .unwrap_or(0);
    while request.len() < header_end + content_length {
        let mut chunk = [0; 4_096];
        let count = stream.read(&mut chunk).await.expect("HTTP request body");
        assert_ne!(count, 0, "HTTP request body ended early");
        request.extend_from_slice(&chunk[..count]);
    }
    String::from_utf8(request).expect("request UTF-8")
}

/// Accepts one request, answers `200 {}`, and returns the captured request text.
pub(super) async fn capture_http_request() -> (std::net::SocketAddr, tokio::task::JoinHandle<String>)
{
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("HTTP connection");
        let request = read_http_request(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .await
            .expect("HTTP response");
        request
    });
    (address, server)
}

/// Starts with `rejected-token` and recovers it once to `fresh-token`.
pub(super) struct RefreshingAuthorization {
    token: Mutex<String>,
    pub(super) authorizations: Mutex<Vec<String>>,
    pub(super) refreshes: AtomicUsize,
}

impl RefreshingAuthorization {
    pub(super) fn new() -> Self {
        Self {
            token: Mutex::new("rejected-token".into()),
            authorizations: Mutex::new(Vec::new()),
            refreshes: AtomicUsize::new(0),
        }
    }

    pub(super) fn refreshes(&self) -> usize {
        self.refreshes.load(Ordering::Relaxed)
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
                drop(token);
                self.refreshes.fetch_add(1, Ordering::Relaxed);
            }
            Ok(true)
        })
    }
}
