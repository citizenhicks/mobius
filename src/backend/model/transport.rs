use std::time::Duration;

use reqwest::Client;
use reqwest::Response;

use crate::Error;
use crate::ProviderError;
use crate::Result;

pub(crate) fn retry_delay(
    error: &crate::ProviderError,
    retry: usize,
    request_id: &str,
    transport: &crate::backend::model::ModelTransportSettings,
) -> Duration {
    let multiplier = 1_u64
        .checked_shl(u32::try_from(retry).unwrap_or(u32::MAX))
        .unwrap_or(u64::MAX);
    let exponential_ms = transport
        .stream_retry_backoff_ms
        .saturating_mul(multiplier)
        .min(transport.stream_retry_max_backoff_ms);
    let jitter = request_id
        .bytes()
        .fold(u64::try_from(retry).unwrap_or(u64::MAX), |value, byte| {
            value.wrapping_mul(16_777_619).wrapping_add(u64::from(byte))
        });
    let jitter_percent = 80 + jitter % 41;
    let backoff = Duration::from_millis(exponential_ms.saturating_mul(jitter_percent) / 100);
    server_retry_delay(error).map_or(backoff, |delay| delay.max(backoff))
}

pub(super) fn server_retry_delay(error: &ProviderError) -> Option<Duration> {
    let value = error.retry_after()?.trim();
    let delay = value
        .parse::<u64>()
        .map(Duration::from_secs)
        .ok()
        .or_else(|| {
            let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
            Some(
                (date.with_timezone(&chrono::Utc) - chrono::Utc::now())
                    .to_std()
                    .unwrap_or_default(),
            )
        })?;
    std::time::Instant::now().checked_add(delay).map(|_| delay)
}

pub(super) const MAX_ERROR_BYTES: usize = 64 * 1024;
pub(super) const MAX_SSE_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;
crate::embedded_config! {
    copy;
    /// Runtime policy for model HTTP, sockets, voice and authentication.
    /// Millisecond units allow short local test deadlines without changing wire protocols.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ModelTransportSettings {
        /// Complete serialized outgoing request limit, including text and tool schemas.
        pub max_request_bytes: usize,
        /// HTTP connection deadline.
        pub http_connect_timeout_ms: u64,
        /// Maximum gap between HTTP response chunks.
        pub http_idle_timeout_ms: u64,
        /// WebSocket connection deadline.
        pub socket_connect_timeout_ms: u64,
        /// WebSocket write and close deadline.
        pub socket_io_timeout_ms: u64,
        /// Maximum gap between WebSocket stream events.
        pub socket_idle_timeout_ms: u64,
        /// Retries before a model or image request fails.
        pub stream_retry_limit: u32,
        /// Initial model and image retry backoff.
        pub stream_retry_backoff_ms: u64,
        /// Ceiling for local exponential model and image backoff.
        pub stream_retry_max_backoff_ms: u64,
        /// Retries before native compaction fails.
        pub compaction_retry_limit: u32,
        /// Initial native compaction retry backoff.
        pub compaction_retry_backoff_ms: u64,
        /// Voice negotiation deadline.
        pub voice_start_timeout_ms: u64,
        /// Voice socket write and cleanup deadline.
        pub voice_io_timeout_ms: u64,
        /// Maximum duration of one voice call.
        pub voice_call_timeout_ms: u64,
        /// OAuth HTTP request deadline.
        pub oauth_request_timeout_ms: u64,
        /// Browser OAuth completion deadline.
        pub oauth_callback_timeout_ms: u64,
        /// Deadline for reading one OAuth callback request.
        pub oauth_callback_request_timeout_ms: u64,
        /// Device-code authentication completion deadline.
        pub device_code_timeout_ms: u64,
    }
    defaults = include_str!("transport.toml");
}

impl ModelTransportSettings {
    /// Rejects zero deadlines, excessive retry budgets and overflowing backoff policy.
    /// # Errors
    /// Returns a configuration error for invalid operational policy.
    pub fn validate(&self) -> Result<()> {
        if self.max_request_bytes == 0 {
            return Err(Error::Config("model request limit must be positive".into()));
        }
        let deadlines = [
            self.http_connect_timeout_ms,
            self.http_idle_timeout_ms,
            self.socket_connect_timeout_ms,
            self.socket_io_timeout_ms,
            self.socket_idle_timeout_ms,
            self.stream_retry_backoff_ms,
            self.stream_retry_max_backoff_ms,
            self.compaction_retry_backoff_ms,
            self.voice_start_timeout_ms,
            self.voice_io_timeout_ms,
            self.voice_call_timeout_ms,
            self.oauth_request_timeout_ms,
            self.oauth_callback_timeout_ms,
            self.oauth_callback_request_timeout_ms,
            self.device_code_timeout_ms,
        ];
        if deadlines
            .into_iter()
            .any(|value| value == 0 || value > 365 * 24 * 60 * 60 * 1_000)
        {
            return Err(Error::Config(
                "model transport deadlines must be positive and at most one year".into(),
            ));
        }
        if self.stream_retry_limit > 100 || self.compaction_retry_limit > 100 {
            return Err(Error::Config(
                "model transport retry limits must be at most 100".into(),
            ));
        }
        if self.stream_retry_backoff_ms > self.stream_retry_max_backoff_ms {
            return Err(Error::Config(
                "initial model retry backoff exceeds its ceiling".into(),
            ));
        }
        Ok(())
    }

    /// Builds an HTTP streaming pool from this validated policy.
    /// # Errors
    /// Returns invalid configuration or HTTP client construction errors.
    pub fn streaming_client(&self) -> Result<Client> {
        self.validate()?;
        Ok(Client::builder()
            // Provider credentials and account headers must never follow redirects.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(self.http_connect_timeout_ms))
            .read_timeout(Duration::from_millis(self.http_idle_timeout_ms))
            .build()?)
    }
}

/// Builds the streaming HTTP client shared by provider construction.
///
/// Clone the returned client freely: every clone shares one connection pool.
/// # Errors
///
/// Returns an error if validation or an operation required by this function fails.
pub fn streaming_client() -> Result<Client> {
    ModelTransportSettings::default().streaming_client()
}

#[cfg(test)]
fn streaming_client_with_idle_timeout(idle_timeout: Duration) -> Result<Client> {
    ModelTransportSettings {
        http_idle_timeout_ms: u64::try_from(idle_timeout.as_millis()).expect("test timeout fits"),
        ..ModelTransportSettings::default()
    }
    .streaming_client()
}

#[cfg(test)]
pub(super) async fn capture_http_request() -> (std::net::SocketAddr, tokio::task::JoinHandle<String>)
{
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("HTTP connection");
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut chunk = [0; 1_024];
            let count = stream.read(&mut chunk).await.expect("HTTP request");
            assert_ne!(count, 0, "request ended before its headers");
            request.extend_from_slice(&chunk[..count]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .await
            .expect("HTTP response");
        String::from_utf8(request).expect("request UTF-8")
    });
    (address, server)
}

pub(super) fn account_stream_bytes(total: &mut usize, added: usize, provider: &str) -> Result<()> {
    *total = total
        .checked_add(added)
        .filter(|total| *total <= MAX_STREAM_BYTES)
        .ok_or_else(|| Error::Provider(format!("{provider} stream exceeded size limit").into()))?;
    Ok(())
}

#[derive(Default)]
pub(super) struct SseDecoder {
    bytes: Vec<u8>,
    frame_start: usize,
    scan: usize,
    total: usize,
}

impl SseDecoder {
    pub(super) fn push(&mut self, chunk: &[u8], provider: &str) -> Result<()> {
        account_stream_bytes(&mut self.total, chunk.len(), provider)?;
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    pub(super) fn next_frame(&mut self) -> Result<Option<&str>> {
        self.compact();
        let start = self.frame_start;
        let mut index = self.scan.max(start);
        while index < self.bytes.len() {
            let width = if self.bytes[index..].starts_with(b"\r\n\r\n") {
                4
            } else if self.bytes[index..].starts_with(b"\n\n") {
                2
            } else {
                index += 1;
                continue;
            };
            if index - start > MAX_SSE_FRAME_BYTES {
                return Err(Error::Provider("SSE frame exceeded size limit".into()));
            }
            self.frame_start = index + width;
            self.scan = self.frame_start;
            return std::str::from_utf8(&self.bytes[start..index])
                .map(Some)
                .map_err(|error| Error::Provider(format!("invalid SSE UTF-8: {error}").into()));
        }
        self.scan = self.bytes.len().saturating_sub(3).max(start);
        let unfinished = &self.bytes[start..];
        // An incomplete delimiter is framing, rather than part of the payload budget.
        let delimiter_prefix = if unfinished.ends_with(b"\r\n\r") {
            3
        } else if unfinished.ends_with(b"\r\n") {
            2
        } else if unfinished.ends_with(b"\r") || unfinished.ends_with(b"\n") {
            1
        } else {
            0
        };
        if unfinished.len() - delimiter_prefix > MAX_SSE_FRAME_BYTES {
            return Err(Error::Provider("SSE frame exceeded size limit".into()));
        }
        Ok(None)
    }

    fn compact(&mut self) {
        let remaining = self.bytes.len() - self.frame_start;
        if self.frame_start == 0 || self.frame_start < remaining {
            return;
        }
        self.bytes.drain(..self.frame_start);
        self.scan -= self.frame_start;
        self.frame_start = 0;
    }
}

pub(super) fn frame_data(frame: &str) -> Option<String> {
    let data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect::<Vec<_>>();
    (!data.is_empty()).then(|| data.join("\n"))
}

pub(super) async fn read_limited(
    mut response: Response,
    limit: usize,
    provider: &str,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(Error::Provider(
                format!("{provider} response body exceeded size limit").into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(super) async fn status_error(mut response: Response, provider: &str) -> Error {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut bytes = Vec::new();
    while bytes.len() < MAX_ERROR_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = MAX_ERROR_BYTES - bytes.len();
                bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            Ok(None) => break,
            Err(error) => {
                return Error::Provider(ProviderError::http(
                    format!("{provider} HTTP {status}: {error}"),
                    status.as_u16(),
                    retry_after,
                ));
            }
        }
    }
    Error::Provider(ProviderError::http(
        http_error_message(provider, status, &bytes),
        status.as_u16(),
        retry_after,
    ))
}

/// `{prefix} HTTP {status}`, followed by the human part of the body when it has one, so
/// frontends show the provider's message rather than its raw JSON.
pub(super) fn http_error_message(
    prefix: &str,
    status: impl std::fmt::Display,
    body: &[u8],
) -> String {
    let detail = error_detail(body);
    if detail.is_empty() {
        format!("{prefix} HTTP {status}")
    } else {
        format!("{prefix} HTTP {status}: {detail}")
    }
}

/// A JSON error's `message` (nested under `error` or top-level, or `error` itself when it is
/// a string), else the trimmed text.
fn error_detail(body: &[u8]) -> String {
    let json = serde_json::from_slice::<serde_json::Value>(body).ok();
    let message = json.as_ref().and_then(|value| {
        let error = value.get("error").unwrap_or(value);
        error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .or_else(|| error.as_str())
    });
    match message {
        Some(message) => message.trim().to_owned(),
        None => String::from_utf8_lossy(body).trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    #[test]
    fn http_errors_show_the_provider_message_not_raw_json() {
        let message = |body: &str| {
            http_error_message("WebSocket", "503 Service Unavailable", body.as_bytes())
        };
        assert_eq!(
            message(
                r#"{"error":{"code":"cloud_model_error","message":"Cloud model service unavailable","status":503}}"#
            ),
            "WebSocket HTTP 503 Service Unavailable: Cloud model service unavailable"
        );
        assert_eq!(
            message(r#"{"message":"Rate limited"}"#),
            "WebSocket HTTP 503 Service Unavailable: Rate limited"
        );
        assert_eq!(
            message(r#"{"error":"overloaded"}"#),
            "WebSocket HTTP 503 Service Unavailable: overloaded"
        );
        assert_eq!(
            message(" upstream down \n"),
            "WebSocket HTTP 503 Service Unavailable: upstream down"
        );
        assert_eq!(message(""), "WebSocket HTTP 503 Service Unavailable");
    }

    #[test]
    fn sse_framing_handles_crlf_and_multiline_data() {
        let mut decoder = SseDecoder::default();
        decoder
            .push(
                b"event: message\r\ndata: one\r\ndata: two\r\n\r\ndata: next\n\n",
                "test",
            )
            .expect("valid chunk");

        let first = decoder
            .next_frame()
            .expect("valid frame")
            .expect("first frame");
        assert_eq!(frame_data(first).as_deref(), Some("one\ntwo"));
        assert_eq!(
            frame_data(
                decoder
                    .next_frame()
                    .expect("valid frame")
                    .expect("second frame")
            )
            .as_deref(),
            Some("next")
        );
    }

    #[test]
    fn sse_framing_handles_one_byte_chunks_and_fragmented_utf8() {
        let input = "data: héllo\r\n\r\ndata: [DONE]\n\n".as_bytes();
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
    fn sse_framing_accepts_8_mib_and_rejects_the_first_excess_byte() {
        let frame = "é".repeat(4 * 1024 * 1024);
        let mut decoder = SseDecoder::default();
        decoder
            .push(frame.as_bytes(), "test")
            .expect("stream limit");
        assert!(decoder.next_frame().expect("unfinished frame").is_none());
        decoder.push(b"\n\n", "test").expect("stream limit");
        assert_eq!(
            decoder.next_frame().expect("8 MiB frame").map(str::len),
            Some(8 * 1024 * 1024)
        );

        for ending in [b"x".as_slice(), b"x\n\n"] {
            let mut decoder = SseDecoder::default();
            decoder
                .push(frame.as_bytes(), "test")
                .expect("stream limit");
            decoder.push(ending, "test").expect("stream limit");
            assert!(decoder.next_frame().is_err());
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

    #[test]
    fn aggregate_stream_limit_rejects_the_first_excess_byte() {
        let mut total = MAX_STREAM_BYTES;
        assert!(account_stream_bytes(&mut total, 1, "test").is_err());
    }

    #[test]
    fn aggregate_stream_limit_counts_frames_without_data() {
        let mut decoder = SseDecoder {
            total: MAX_STREAM_BYTES,
            ..SseDecoder::default()
        };

        assert!(decoder.push(b": keep-alive\n\n", "test").is_err());
        assert!(decoder.bytes.is_empty());
    }

    #[tokio::test]
    async fn status_error_preserves_retry_metadata() {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("test listener");
        let address = listener.local_addr().expect("listener address");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("test connection");
            stream
                .write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\n\
                      Content-Length: 4\r\nConnection: close\r\n\r\nslow",
                )
                .await
                .expect("test response");
        });
        let response = streaming_client()
            .expect("client")
            .get(format!("http://{address}"))
            .send()
            .await
            .expect("response");

        let Error::Provider(error) = status_error(response, "test").await else {
            panic!("expected provider error");
        };

        assert_eq!(
            (error.status(), error.is_retryable(), error.retry_after()),
            (Some(429), true, Some("7"))
        );
    }

    #[tokio::test]
    async fn chunk_timeout_is_idle_not_whole_stream_timeout() {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("test listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("test connection");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
                .await
                .expect("test headers");
            std::future::pending::<()>().await;
        });
        let mut response = streaming_client_with_idle_timeout(Duration::from_millis(10))
            .expect("client")
            .get(format!("http://{address}"))
            .send()
            .await
            .expect("response headers");

        let error = response
            .chunk()
            .await
            .expect_err("idle response should time out");
        server.abort();

        assert!(error.is_timeout());
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn embedded_policy_preserves_transport_deadlines_and_retry_budgets() {
        assert_eq!(
            ModelTransportSettings::default(),
            ModelTransportSettings {
                max_request_bytes: 24 * 1024 * 1024,
                http_connect_timeout_ms: 10_000,
                http_idle_timeout_ms: 180_000,
                socket_connect_timeout_ms: 15_000,
                socket_io_timeout_ms: 5_000,
                socket_idle_timeout_ms: 300_000,
                stream_retry_limit: 5,
                stream_retry_backoff_ms: 200,
                stream_retry_max_backoff_ms: 3_200,
                compaction_retry_limit: 2,
                compaction_retry_backoff_ms: 200,
                voice_start_timeout_ms: 30_000,
                voice_io_timeout_ms: 5_000,
                voice_call_timeout_ms: 3_600_000,
                oauth_request_timeout_ms: 30_000,
                oauth_callback_timeout_ms: 900_000,
                oauth_callback_request_timeout_ms: 5_000,
                device_code_timeout_ms: 900_000,
            }
        );
    }

    #[test]
    fn partial_policy_inherits_defaults_and_rejects_invalid_deadlines() {
        let settings: ModelTransportSettings =
            toml::from_str("http_idle_timeout_ms = 17\nstream_retry_limit = 0")
                .expect("partial settings");
        assert_eq!(settings.http_idle_timeout_ms, 17);
        assert_eq!(settings.stream_retry_limit, 0);
        assert_eq!(
            settings.socket_io_timeout_ms,
            ModelTransportSettings::default().socket_io_timeout_ms
        );
        settings.validate().expect("valid policy");
        let invalid = ModelTransportSettings {
            socket_io_timeout_ms: 0,
            ..settings
        };
        assert!(invalid.streaming_client().is_err());
        assert!(toml::from_str::<ModelTransportSettings>("socket_io_timout_ms = 5").is_err());
    }

    #[tokio::test]
    async fn authenticated_streaming_clients_do_not_follow_redirects() {
        use tokio::io::AsyncWriteExt as _;
        let destination = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("destination");
        let target = destination.local_addr().expect("destination address");
        let source = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("source");
        let address = source.local_addr().expect("source address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.expect("source connection");
            stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://{target}/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.expect("redirect response");
        });
        let response = streaming_client()
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
}
