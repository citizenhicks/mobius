use super::super::*;
use crate::protocol::ModelEvent;

#[tokio::test]
async fn metadata_only_event_does_not_count_as_sink_delivery() {
    let (sender, mut messages) = mpsc::unbounded_channel();
    sender
        .send(SocketEvent::Message(Message::text(
            serde_json::json!({
                "type": "response.output_item.added",
                "item": {
                    "id": "commentary-1",
                    "type": "message",
                    "phase": "commentary"
                }
            })
            .to_string(),
        )))
        .expect("metadata event");
    drop(sender);
    let delivered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink_delivered = Arc::clone(&delivered);
    let events: ModelEventSink = Arc::new(move |_| {
        sink_delivered.fetch_add(1, Ordering::Relaxed);
        Ok(())
    });

    let exchange = read_exchange(&mut messages, &events)
        .await
        .expect("exchange result");

    assert!(matches!(exchange, Exchange::Reconnect));
    assert_eq!(delivered.load(Ordering::Relaxed), 0);
}

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
        Ok(())
    });

    let exchange = read_exchange(&mut messages, &events)
        .await
        .expect("exchange result");

    assert!(matches!(exchange, Exchange::Reconnect));
    assert_eq!(delivered.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn server_close_before_completion_is_retryable() {
    let (sender, mut messages) = mpsc::unbounded_channel();
    sender.send(SocketEvent::Closed).expect("server close");
    let events: ModelEventSink = Arc::new(|_| Ok(()));

    let exchange = read_exchange(&mut messages, &events)
        .await
        .expect("exchange result");

    assert!(matches!(exchange, Exchange::Reconnect));
}

#[tokio::test]
async fn tool_call_readiness_does_not_complete_a_disconnected_exchange() {
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
                    "name": "dangerous_tool",
                    "arguments": "{}"
                }
            })
            .to_string(),
        )))
        .expect("tool call item");
    drop(sender);
    let delivered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink_delivered = Arc::clone(&delivered);
    let events: ModelEventSink = Arc::new(move |event| {
        assert!(matches!(event, ModelEvent::ToolCallReady(_)));
        sink_delivered.fetch_add(1, Ordering::Relaxed);
        Ok(())
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
        Ok(())
    });

    let exchange = read_exchange(&mut messages, &events)
        .await
        .expect("exchange result");

    assert!(matches!(exchange, Exchange::Completed(_)));
    assert_eq!(
        *seen.lock().expect("events lock"),
        vec![ModelEvent::ToolCallReady(crate::backend::model::ToolCall {
            call_id: "call-1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "README.md"}),
        })]
    );
}

#[tokio::test(start_paused = true)]
async fn stream_idle_timeout_is_retryable() {
    let (_sender, mut messages) = mpsc::unbounded_channel();
    let events: ModelEventSink = Arc::new(|_| Ok(()));
    let exchange = tokio::spawn(async move { read_exchange(&mut messages, &events).await });
    tokio::task::yield_now().await;

    tokio::time::advance(STREAM_IDLE_TIMEOUT).await;
    let exchange = exchange
        .await
        .expect("exchange task")
        .expect("exchange result");

    assert!(matches!(exchange, Exchange::Reconnect));
}
