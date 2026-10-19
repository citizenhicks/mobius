use super::super::*;
use crate::protocol::ModelEvent;

#[tokio::test]
async fn visible_delta_before_eof_is_still_a_retryable_stream_failure() {
    let (sender, mut messages) = mpsc::unbounded_channel();
    sender
        .send(SocketEvent::Message(Message::text(
            serde_json::json!({
                "type": "response.output_text.delta",
                "delta": "partial"
            })
            .to_string(),
        )))
        .expect("text delta");
    drop(sender);
    let delivered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink_delivered = Arc::clone(&delivered);
    let events: ModelEventSink = Arc::new(move |_| {
        sink_delivered.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Ok(()) })
    });

    let exchange = read_exchange(&mut messages, &events)
        .await
        .expect("exchange result");

    assert!(matches!(exchange, Exchange::Reconnect));
    assert_eq!(delivered.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn completed_tool_call_is_emitted_before_websocket_completion() {
    let (sender, mut messages) = mpsc::unbounded_channel();
    sender
        .send(SocketEvent::Message(Message::text(
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "id": "item-1",
                    "call_id": "call-1",
                    "name": "read_file",
                    "arguments": "{\"path\":\"README.md\"}"
                }
            })
            .to_string(),
        )))
        .expect("tool call item");
    sender
        .send(SocketEvent::Message(Message::text(
            serde_json::json!({
                "type": "response.completed",
                "response": {"id": "response-1", "output": []}
            })
            .to_string(),
        )))
        .expect("completion event");
    drop(sender);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let events: ModelEventSink = Arc::new(move |event| {
        sink_seen.lock().expect("events lock").push(event);
        Box::pin(async { Ok(()) })
    });

    let exchange = read_exchange(&mut messages, &events)
        .await
        .expect("exchange result");

    assert!(matches!(exchange, Exchange::Completed(_)));
    assert_eq!(
        *seen.lock().expect("events lock"),
        vec![ModelEvent::ToolCallReady(crate::protocol::ToolCall {
            call_id: "call-1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "README.md"}),
        })]
    );
}

#[tokio::test]
async fn websocket_burst_waits_for_the_event_sink_in_order() {
    const DELTAS: usize = 2048;
    let (sender, mut messages) = mpsc::unbounded_channel();
    for index in 0..DELTAS {
        sender
            .send(SocketEvent::Message(Message::text(
                serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": index.to_string(),
                })
                .to_string(),
            )))
            .expect("text delta");
    }
    sender
        .send(SocketEvent::Message(Message::text(
            serde_json::json!({
                "type": "response.completed",
                "response": {"id": "response-burst", "output": []},
            })
            .to_string(),
        )))
        .expect("completion");
    drop(sender);
    let (delivered, mut received) = mpsc::channel(1);
    let events: ModelEventSink = Arc::new(move |event| {
        let delivered = delivered.clone();
        Box::pin(async move {
            delivered
                .send(event)
                .await
                .map_err(|error| Error::Stopped(format!("test event consumer closed: {error}")))
        })
    });
    let exchange = tokio::spawn(async move { read_exchange(&mut messages, &events).await });
    assert_eq!(
        received.recv().await,
        Some(ModelEvent::TextDelta("0".into()))
    );
    assert!(
        !exchange.is_finished(),
        "the stream must wait for the consumer"
    );
    for index in 1..DELTAS {
        assert_eq!(
            received.recv().await,
            Some(ModelEvent::TextDelta(index.to_string())),
        );
    }
    assert!(matches!(
        exchange.await.expect("exchange task").expect("exchange"),
        Exchange::Completed(_)
    ));
}
