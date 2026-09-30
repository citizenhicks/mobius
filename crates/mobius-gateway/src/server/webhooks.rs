//! Bounded HTTP adapter for configured, Bot-scoped external event sources.

use super::{ConnectionContext, PRE_AUTH_TIMEOUT};
use crate::bots::BotStore;
use crate::{Error, Result};
use chrono::Utc;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};

const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024;
const REPLAY_WINDOW_SECONDS: u64 = 300;

pub(super) async fn serve<S>(
    stream: S,
    bots: Arc<BotStore>,
    expected_host: Option<&str>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut stream = BufReader::new(stream);
    let result =
        tokio::time::timeout(PRE_AUTH_TIMEOUT, receive(&mut stream, &bots, expected_host)).await;
    let status = match result {
        Ok(Ok(true)) => "202 Accepted",
        Ok(Ok(false)) => "200 OK",
        Ok(Err(Error::Unauthorized)) => "401 Unauthorized",
        Ok(Err(Error::Protocol(_) | Error::Json(_))) => "400 Bad Request",
        Ok(Err(Error::Io(error))) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            "400 Bad Request"
        }
        Ok(Err(Error::Config(_))) => "409 Conflict",
        Err(_) => "408 Request Timeout",
        Ok(Err(_)) => "503 Service Unavailable",
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n"
    );
    tokio::time::timeout(PRE_AUTH_TIMEOUT, stream.write_all(response.as_bytes()))
        .await
        .map_err(|_| Error::Protocol("webhook response timed out".into()))??;
    Ok(())
}

pub(super) async fn serve_tls<S>(
    stream: S,
    connection: ConnectionContext,
    deadline: tokio::time::Instant,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut stream = BufReader::new(stream);
    let first = tokio::time::timeout_at(deadline, stream.fill_buf())
        .await
        .map_err(|_| Error::Unauthorized)??;
    if first.first() == Some(&b'P') {
        serve(stream, Arc::clone(&connection.bots), None).await
    } else {
        super::serve_connection(stream, connection, deadline, None).await
    }
}

async fn receive<S>(
    stream: &mut BufReader<S>,
    bots: &Arc<BotStore>,
    expected_host: Option<&str>,
) -> Result<bool>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() == MAX_HEADER_BYTES {
            return Err(invalid());
        }
        header.push(stream.read_u8().await?);
    }
    let header = std::str::from_utf8(&header).map_err(|_| invalid())?;
    let (source_id, headers) = parse_headers(header, expected_host)?;
    let token = headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty() && token.len() <= 128)
        .ok_or(Error::Unauthorized)?;
    let delivery_id = headers
        .get("x-mobius-delivery-id")
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        })
        .ok_or_else(invalid)?;
    let timestamp = headers
        .get("x-mobius-timestamp")
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(invalid)?;
    let now = Utc::now().timestamp();
    if timestamp.abs_diff(now) > REPLAY_WINDOW_SECONDS {
        return Err(Error::Unauthorized);
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|length| *length > 0 && *length <= MAX_BODY_BYTES)
        .ok_or_else(invalid)?;
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    let body_digest = Sha256::digest(&body).into();
    let text = String::from_utf8(body).map_err(|_| invalid())?;
    let source_id = source_id.to_owned();
    let delivery_id = (*delivery_id).to_owned();
    let token_hash = Sha256::digest(token.as_bytes()).into();
    let bots = Arc::clone(bots);
    tokio::task::spawn_blocking(move || {
        bots.accept_webhook(
            &crate::bots::WebhookDelivery {
                source_id: &source_id,
                token_hash,
                delivery_id: &delivery_id,
                body_digest,
                body: &text,
                timestamp,
            },
            now,
        )
    })
    .await
    .map_err(|error| Error::Io(std::io::Error::other(error)))?
}

fn parse_headers<'a>(
    header: &'a str,
    expected_host: Option<&str>,
) -> Result<(&'a str, BTreeMap<String, &'a str>)> {
    let mut lines = header.split("\r\n");
    let request = lines.next().ok_or_else(invalid)?;
    let mut request = request.split(' ');
    let (Some("POST"), Some(path), Some("HTTP/1.1"), None) = (
        request.next(),
        request.next(),
        request.next(),
        request.next(),
    ) else {
        return Err(invalid());
    };
    let id = path
        .strip_prefix("/webhooks/")
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .ok_or_else(invalid)?;
    let mut headers = BTreeMap::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or_else(invalid)?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.chars().any(|c| c.is_control() && c != '\t')
        {
            return Err(invalid());
        }
        if headers
            .insert(name.to_ascii_lowercase(), value.trim())
            .is_some()
        {
            return Err(invalid());
        }
    }
    let host = headers.get("host").ok_or_else(invalid)?;
    if expected_host.is_some_and(|expected| {
        !host.eq_ignore_ascii_case(expected)
            && !host
                .strip_suffix(":443")
                .is_some_and(|host| host.eq_ignore_ascii_case(expected))
    }) {
        return Err(Error::Unauthorized);
    }
    if headers.contains_key("origin")
        || headers.contains_key("transfer-encoding")
        || headers.contains_key("expect")
        || headers.get("content-type") != Some(&"application/json")
    {
        return Err(invalid());
    }
    Ok((id, headers))
}
fn invalid() -> Error {
    Error::Protocol("invalid webhook request".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{AgentComposition, BotAction, HookData, HookSource};

    const HOST: &str = "gateway.example.com";
    const TOKEN: &str = "fixture-webhook-token";

    fn fixture() -> (tempfile::TempDir, Arc<BotStore>, String, String) {
        let root = tempfile::tempdir().expect("state directory");
        let bots = Arc::new(BotStore::open(root.path()).expect("Bot store"));
        let bot = bots
            .create_bot(
                "Monitor",
                "Monitor server events.",
                AgentComposition::default(),
            )
            .expect("Bot");
        let source = bots
            .create_webhook(
                &bot.id,
                "outage",
                "Tell me about the outage.",
                Sha256::digest(TOKEN.as_bytes()).into(),
            )
            .expect("webhook source");
        (root, bots, bot.id, source.id)
    }

    fn post(source_id: &str, token: &str, host: &str, body: &str) -> String {
        format!(
            "POST /webhooks/{source_id} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\nX-Mobius-Delivery-ID: delivery-1\r\nX-Mobius-Timestamp: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            Utc::now().timestamp(),
            body.len(),
        )
    }

    async fn exchange(bots: Arc<BotStore>, request: &str) -> String {
        let (server, mut client) = tokio::io::duplex(MAX_HEADER_BYTES + MAX_BODY_BYTES + 1024);
        let handler = tokio::spawn(serve(server, bots, Some(HOST)));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            client
                .write_all(request.as_bytes())
                .await
                .expect("HTTP request");
            client.shutdown().await.expect("request complete");
            let mut response = String::new();
            client
                .read_to_string(&mut response)
                .await
                .expect("HTTP response");
            handler
                .await
                .expect("adapter task")
                .expect("adapter response");
            response
        })
        .await
        .expect("bounded HTTP exchange")
    }

    fn assert_no_delivery(bots: &BotStore) {
        assert!(bots.unpublished_events(10).expect("events").is_empty());
        assert!(
            bots.pending_actions(Utc::now().timestamp(), 10)
                .expect("actions")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_contended_commit_does_not_block_the_runtime() {
        let (root, bots, _bot_id, source_id) = fixture();
        let state_lock =
            std::fs::File::open(root.path().join("bots-state.lock")).expect("Bot state lock");
        state_lock.lock().expect("hold Bot state lock");
        let (progress, release) = std::sync::mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            let progressed = release
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok();
            drop(state_lock);
            progressed
        });
        let (server, mut client) = tokio::io::duplex(MAX_HEADER_BYTES);
        client
            .write_all(post(&source_id, TOKEN, HOST, "{}").as_bytes())
            .await
            .expect("HTTP request");
        let handler = tokio::spawn(serve(server, bots, Some(HOST)));
        tokio::task::yield_now().await;
        let _ = progress.send(());
        assert!(watchdog.join().expect("lock watchdog"));
        handler
            .await
            .expect("adapter task")
            .expect("adapter response");
    }

    #[tokio::test]
    async fn authenticated_post_is_one_durable_event_and_action_across_retries() {
        let (_root, bots, bot_id, source_id) = fixture();
        let body = r#"{ "server_name": "api", "Nested_Payload": {"camelKey": 9007199254740993} }"#;
        let request = post(&source_id, TOKEN, HOST, body);
        let accepted = exchange(Arc::clone(&bots), &request).await;
        assert!(accepted.starts_with("HTTP/1.1 202 Accepted\r\n"));
        let duplicate = exchange(Arc::clone(&bots), &request).await;
        assert!(duplicate.starts_with("HTTP/1.1 200 OK\r\n"));
        let compact =
            serde_json::to_string(&serde_json::from_str::<serde_json::Value>(body).expect("JSON"))
                .expect("compact JSON");
        let changed_bytes =
            exchange(Arc::clone(&bots), &post(&source_id, TOKEN, HOST, &compact)).await;
        assert!(changed_bytes.starts_with("HTTP/1.1 409 Conflict\r\n"));
        let conflict = exchange(Arc::clone(&bots), &post(&source_id, TOKEN, HOST, "{}")).await;
        assert!(conflict.starts_with("HTTP/1.1 409 Conflict\r\n"));

        let events = bots.unpublished_events(10).expect("durable facts");
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.bot_id, bot_id);
        assert_eq!(event.source, HookSource::Custom { source_id });
        assert!(matches!(&event.data, HookData::CustomReceived {name,data}
            if name == "outage" && data == &serde_json::from_str::<serde_json::Value>(body).expect("JSON")));
        let pending = bots
            .pending_actions(Utc::now().timestamp(), 10)
            .expect("durable actions");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].bot_id, bot_id);
        assert_eq!(pending[0].event, *event);
        assert_eq!(
            pending[0].action,
            BotAction::Report {
                instruction: "Tell me about the outage.".into()
            }
        );
    }

    #[tokio::test]
    async fn bad_token_and_wrong_host_never_accept_a_delivery() {
        let (_root, bots, _bot_id, source_id) = fixture();
        for request in [
            post(&source_id, "wrong-token", HOST, "{}"),
            post(&source_id, "wrong-token", HOST, "{"),
            post(&source_id, TOKEN, "other.example.com", "{}"),
        ] {
            let response = exchange(Arc::clone(&bots), &request).await;
            assert!(
                response.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
                "{response}"
            );
            assert_no_delivery(&bots);
        }
    }

    #[tokio::test]
    async fn malformed_or_oversized_requests_never_accept_a_delivery() {
        let (_root, bots, _bot_id, source_id) = fixture();
        let valid = post(&source_id, TOKEN, HOST, "{}");
        for request in [
            format!("POST /webhooks/{source_id} HTTP/1.1\r\n"),
            post(&source_id, TOKEN, HOST, "{"),
            post(&source_id, TOKEN, HOST, "{").replace("Content-Length: 1", "Content-Length: 2"),
            post(&source_id, TOKEN, HOST, ""),
            post(&source_id, TOKEN, HOST, &" ".repeat(MAX_BODY_BYTES + 1)),
            valid.replace(
                "\r\n\r\n",
                &format!("\r\nX-Filler: {}\r\n\r\n", "x".repeat(MAX_HEADER_BYTES)),
            ),
            valid.replace(
                "\r\n\r\n",
                "\r\nOrigin: https://gateway.example.com\r\n\r\n",
            ),
            valid.replace("\r\n\r\n", "\r\ncontent-length: 2\r\n\r\n"),
            valid.replace("\r\n\r\n", "\r\nTransfer-Encoding: chunked\r\n\r\n"),
        ] {
            let response = exchange(Arc::clone(&bots), &request).await;
            assert!(
                response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
                "{response}"
            );
            assert_no_delivery(&bots);
        }
    }

    #[tokio::test]
    async fn authenticated_body_at_the_byte_limit_is_accepted() {
        let (_root, bots, _bot_id, source_id) = fixture();
        let body = format!(
            "{{\"payload\":\"{}\"}}",
            "x".repeat(MAX_BODY_BYTES - r#"{"payload":""}"#.len())
        );
        assert_eq!(body.len(), MAX_BODY_BYTES);
        let response = exchange(Arc::clone(&bots), &post(&source_id, TOKEN, HOST, &body)).await;
        assert!(response.starts_with("HTTP/1.1 202 Accepted\r\n"));
        assert_eq!(bots.unpublished_events(10).expect("events").len(), 1);
        assert_eq!(
            bots.pending_actions(Utc::now().timestamp(), 10)
                .expect("actions")
                .len(),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_incomplete_request_times_out_without_a_delivery() {
        let (_root, bots, _bot_id, _source_id) = fixture();
        let (server, mut client) = tokio::io::duplex(MAX_HEADER_BYTES);
        let handler = tokio::spawn(serve(server, Arc::clone(&bots), Some(HOST)));
        let response = tokio::time::timeout(
            PRE_AUTH_TIMEOUT + std::time::Duration::from_secs(1),
            async {
                let mut response = String::new();
                client
                    .read_to_string(&mut response)
                    .await
                    .expect("HTTP response");
                response
            },
        )
        .await
        .expect("request deadline");
        handler
            .await
            .expect("adapter task")
            .expect("adapter response");
        assert!(response.starts_with("HTTP/1.1 408 Request Timeout\r\n"));
        assert_no_delivery(&bots);
    }

    #[test]
    fn bounded_post_rejects_ambiguous_auth_and_browser_ingress() {
        let id = uuid::Uuid::new_v4();
        let base = format!(
            "POST /webhooks/{id} HTTP/1.1\r\nHost: example.com\r\nContent-Type: application/json\r\n"
        );
        assert!(
            parse_headers(
                &(base.clone() + "Content-Length: 2\r\n\r\n"),
                Some("example.com")
            )
            .is_ok()
        );
        for extra in [
            "Origin: https://example.com\r\n",
            "Content-Length: 1\r\ncontent-length: 2\r\n",
            "Transfer-Encoding: chunked\r\n",
            " Authorization: Bearer bad\r\n",
        ] {
            assert!(parse_headers(&(base.clone() + extra + "\r\n"), None).is_err());
        }
        assert!(parse_headers(&(base + "\r\n"), Some("other.com")).is_err());
    }
}
