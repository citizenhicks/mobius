use std::time::Duration;

use reqwest::Client;
use reqwest::Response;

use crate::Error;
use crate::ProviderError;
use crate::Result;

pub(super) const MAX_ERROR_BYTES: usize = 64 * 1024;
pub(super) const MAX_SSE_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub(super) const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// Builds the streaming HTTP client shared by provider construction.
///
/// Clone the returned client freely: every clone shares one connection pool.
pub fn streaming_client() -> Result<Client> {
    streaming_client_with_idle_timeout(STREAM_IDLE_TIMEOUT)
}

fn streaming_client_with_idle_timeout(idle_timeout: Duration) -> Result<Client> {
    Ok(Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(idle_timeout)
        .build()?)
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
        if self.bytes.len() - start > MAX_SSE_FRAME_BYTES {
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
        format!(
            "{provider} HTTP {status}: {}",
            String::from_utf8_lossy(&bytes)
        ),
        status.as_u16(),
        retry_after,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

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
    fn sse_framing_rejects_unterminated_frames_over_the_limit() {
        let mut decoder = SseDecoder::default();
        let bytes = vec![b'x'; MAX_SSE_FRAME_BYTES + 1];
        decoder.push(&bytes, "test").expect("stream limit");

        assert!(decoder.next_frame().is_err());
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
