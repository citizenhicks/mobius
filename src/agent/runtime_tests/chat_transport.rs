//! Real Chat Completions transport failures through the agent retry loop.

use super::*;
use crate::backend::model::RawModelTransportSettings;
use crate::backend::model::kimi::Kimi;
use crate::backend::model::provider::BaseUrl;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

enum Reply {
    Timeout,
    Status(u16),
    Success,
}

async fn read_request(socket: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 4_096];
        let count = socket.read(&mut buffer).await.expect("request bytes");
        assert_ne!(count, 0, "request ended before its body");
        bytes.extend_from_slice(&buffer[..count]);
        let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let length = std::str::from_utf8(&bytes[..end])
            .expect("request headers")
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|length| length.trim().parse::<usize>().expect("content length"))
            })
            .expect("request content length");
        if bytes.len() >= end + 4 + length {
            return serde_json::from_slice(&bytes[end + 4..]).expect("request body");
        }
    }
}

async fn run_chat(replies: &'static [Reply]) -> (Vec<Value>, Vec<EventMsg>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let base_url = format!("http://{}/private-endpoint", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        for reply in replies {
            let (mut socket, _) = listener.accept().await.expect("connection");
            let request = read_request(&mut socket).await;
            captured.lock().expect("captured requests").push(request);
            let (status, body) = match reply {
                Reply::Timeout => {
                    // Wait for the client's read deadline, without a competing server timer.
                    assert_eq!(socket.read(&mut [0; 1]).await.expect("client close"), 0);
                    continue;
                }
                Reply::Status(status) => (*status, "{\"error\":{\"message\":\"unavailable\"}}"),
                Reply::Success => (
                    200,
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Recovered.\"}}]}\n\ndata: [DONE]\n\n",
                ),
            };
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nRetry-After: 0\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response");
        }
    });
    let settings = ModelTransportSettings::try_from(RawModelTransportSettings {
        http_idle_timeout_ms: 100,
        stream_retry_limit: 1,
        stream_retry_backoff_ms: 1,
        stream_retry_max_backoff_ms: 1,
        ..RawModelTransportSettings::default()
    })
    .expect("transport settings");
    let model = Kimi::new_with_transport(
        "private-test-key",
        BaseUrl::new(base_url).expect("base URL"),
        "test-model",
        settings,
    )
    .expect("Kimi provider");
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3")).expect("store"),
    );
    let mut agent = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "chat-transport",
        "kimi",
        Arc::new(model),
    ))
    .await
    .expect("agent");
    agent.sender().submit(user_op("hello")).expect("submit");
    let events = tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = Vec::new();
        loop {
            let event = agent.next_event().await.expect("agent event").msg;
            let terminal = matches!(event, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_));
            events.push(event);
            if terminal {
                return events;
            }
        }
    })
    .await
    .expect("bounded model retries");
    server.abort();
    if let Err(error) = server.await {
        assert!(error.is_cancelled(), "HTTP server failed: {error}");
    }
    let requests = requests.lock().expect("captured requests").clone();
    (requests, events)
}

#[tokio::test]
async fn chat_send_timeout_and_transient_statuses_retry_the_same_request() {
    for replies in [
        &[Reply::Timeout, Reply::Success][..],
        &[Reply::Status(408), Reply::Success][..],
        &[Reply::Status(429), Reply::Success][..],
        &[Reply::Status(503), Reply::Success][..],
    ] {
        let (requests, events) = run_chat(replies).await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EventMsg::ModelStepStarted(_)))
                .count(),
            2
        );
        assert!(events.iter().any(|event| matches!(event,
            EventMsg::ModelStepCompleted(step) if step.outcome == ModelStepOutcome::Retrying
        )));
        assert!(events.iter().any(|event| matches!(event,
            EventMsg::AssistantMessage(message) if message.reply_text().as_deref() == Some("Recovered.")
        )));
        assert!(matches!(events.last(), Some(EventMsg::TurnComplete(_))));
    }
}

#[tokio::test]
async fn chat_retry_exhaustion_and_authentication_failure_preserve_error_metadata() {
    for (replies, attempts, retryable, status, retry_after) in [
        (&[Reply::Timeout, Reply::Timeout][..], 2, true, None, None),
        (
            &[Reply::Status(429), Reply::Status(429)][..],
            2,
            true,
            Some(429),
            Some("0"),
        ),
        (&[Reply::Status(401)][..], 1, false, Some(401), Some("0")),
    ] {
        let (requests, events) = run_chat(replies).await;
        assert_eq!(requests.len(), attempts);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EventMsg::ModelStepStarted(_)))
                .count(),
            attempts
        );
        let failure = events
            .iter()
            .find_map(|event| match event {
                EventMsg::Error(error) => Some(error),
                _ => None,
            })
            .expect("terminal error");
        assert_eq!(failure.kind, ErrorKind::Provider);
        assert_eq!(failure.retryable, retryable);
        assert_eq!(failure.status, status);
        assert_eq!(failure.retry_after.as_deref(), retry_after);
        assert!(!failure.message.contains("private-endpoint"));
        assert!(!failure.message.contains("private-test-key"));
        if status.is_none() {
            assert!(failure.message.contains("timed out"));
        }
        assert!(matches!(events.last(), Some(EventMsg::TurnAborted(_))));
    }
}
