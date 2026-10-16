use super::*;
use crate::telemetry::Trigger;
use crate::telemetry::{Telemetry, TelemetrySink};

fn sink(url: String) -> TelemetrySink {
    serde_json::from_value(serde_json::json!({"id":"test", "url":url, "every_seconds":15, "sections":["activity"], "events":["custom_received"]})).unwrap()
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn shutdown_remains_live_when_activity_and_routine_dispatch_contend_for_host_state() {
    for hook in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let (mut server, _) = configured_test_server(root.path().join("state")).await;
        if hook {
            server.config.telemetry.activity_hook = Some(crate::telemetry::ActivityHookConfig {
                command: vec!["/bin/cat".into()],
                idle_grace_seconds: 60,
                timeout_seconds: 60,
                retry_seconds: 1,
            });
        } else {
            server
                .host
                .configure_telemetry(0, vec![sink("https://127.0.0.1:1/collect".into())], &[])
                .await
                .unwrap();
        }
        let (entered, release, actor) = server.host.pause_activity_reply_for_test().await;
        let ready = server.notify_ready();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let mut serving = tokio::spawn(server.serve_until(async move {
            let _ = stopped.await;
        }));
        ready.await.unwrap();
        entered.await.unwrap();
        tokio::time::advance(ROUTINE_TICK).await;
        tokio::task::yield_now().await;
        tokio::time::resume();
        stop.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), &mut serving).await;
        if result.is_err() {
            serving.abort();
            let _ = serving.await;
        }
        let _ = release.send(());
        actor.await.unwrap();
        result
            .expect("shutdown must progress while the activity reply remains paused")
            .unwrap()
            .unwrap();
        tokio::time::pause();
    }
}

#[tokio::test]
async fn committed_events_retry_and_advance_only_after_delivery() {
    use crate::wire::{HookData, HookEvent, HookSource};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let root = tempfile::tempdir().unwrap();
    let state_dir = root.path().join("state");
    let (server, _) = configured_test_server(state_dir.clone()).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = sink(format!("http://{}", listener.local_addr().unwrap()));
    server
        .host
        .configure_telemetry(0, vec![endpoint.clone()], &[])
        .await
        .unwrap();
    let bot = server.bots.bots().unwrap().remove(0);
    let event = HookEvent {
        id: "telemetry-fact".into(),
        bot_id: bot.id.clone(),
        occurred_at: 1,
        source: HookSource::Bot {
            bot_id: bot.id.clone(),
        },
        cause_id: None,
        ancestry: vec![],
        data: HookData::CustomReceived {
            name: "test".into(),
            data: serde_json::json!({}),
        },
    };
    server.bots.record_hook(&event).unwrap();
    assert_eq!(server.bots.telemetry_batch(&endpoint).unwrap().1, 1);
    let receiver = tokio::spawn(async move {
        for status in [500, 202, 400] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 8192];
            let _ = stream.read(&mut buffer).await.unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    for (offset, pending) in [(0, 1), (15, 0)] {
        let mut tasks = JoinSet::new();
        Telemetry::tick_at(
            &server.host,
            0,
            Trigger::Events,
            &mut tasks,
            Utc::now().timestamp() + offset,
        )
        .await
        .unwrap();
        while tasks.join_next().await.is_some() {}
        assert_eq!(server.bots.telemetry_batch(&endpoint).unwrap().1, pending);
    }
    let mut second = event;
    second.id = "second-fact".into();
    second.occurred_at = 0;
    server.bots.record_hook(&second).unwrap();
    let mut tasks = JoinSet::new();
    Telemetry::tick(&server.host, 0, Trigger::Events, &mut tasks).await;
    while tasks.join_next().await.is_some() {}
    assert_eq!(server.bots.telemetry_batch(&endpoint).unwrap().1, 0);
    assert_eq!(
        server.host.telemetry.status("test").unwrap().last_status,
        Some(400)
    );
    assert!(
        server
            .host
            .telemetry
            .status("test")
            .unwrap()
            .last_error
            .is_some()
    );
    let reopened = BotStore::open(&state_dir).unwrap();
    assert_eq!(reopened.telemetry_batch(&endpoint).unwrap().1, 0);
    receiver.await.unwrap();
    server.host.shutdown().await;
}

#[tokio::test]
async fn ingress_rejects_raw_frames_and_accepts_noise_websockets() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let root = tempfile::tempdir().unwrap();
    let (mut server, grant) = configured_test_server(root.path().join("state")).await;
    let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ingress = reservation.local_addr().unwrap();
    drop(reservation);
    server.config.runtime.ingress = Some(ingress);
    let ready = server.notify_ready();
    let (stop, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async {
        let _ = signal.await;
    }));
    ready.await.unwrap();
    let mut raw = TcpStream::connect(ingress).await.unwrap();
    raw.write_all(&[0, 0, 0, 2, b'{', b'}']).await.unwrap();
    raw.shutdown().await.unwrap();
    let mut byte = [0];
    let result = tokio::time::timeout(Duration::from_secs(6), raw.read(&mut byte))
        .await
        .unwrap();
    assert!(result.is_err() || result.unwrap() == 0);
    let mut request = format!("ws://{ingress}").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", "mobius-noise-v1".parse().unwrap());
    let (mut websocket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    crate::channel::client_handshake(&mut websocket, &grant.code)
        .await
        .unwrap();
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn telemetry_oversized_event_advances_cursor_with_a_permanent_diagnostic() {
    use crate::wire::{HookData, HookEvent, HookSource};
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let mut endpoint = sink("http://127.0.0.1:1".into());
    for index in 0..4 {
        endpoint
            .fields
            .insert(format!("label{index}"), "x".repeat(1024));
    }
    server
        .host
        .configure_telemetry(0, vec![endpoint.clone()], &[])
        .await
        .unwrap();
    let bot = server.bots.bots().unwrap().remove(0);
    server
        .bots
        .record_hook(&HookEvent {
            id: "oversized-fact".into(),
            bot_id: bot.id.clone(),
            occurred_at: 1,
            source: HookSource::Bot { bot_id: bot.id },
            cause_id: None,
            ancestry: vec![],
            data: HookData::CustomReceived {
                name: "large".into(),
                data: serde_json::json!("x".repeat(63 * 1024)),
            },
        })
        .unwrap();
    let mut tasks = JoinSet::new();
    Telemetry::tick(&server.host, 0, Trigger::Events, &mut tasks).await;
    while tasks.join_next().await.is_some() {}
    assert_eq!(server.bots.telemetry_count(&endpoint).unwrap(), 0);
    assert_eq!(
        server
            .host
            .telemetry
            .status("test")
            .unwrap()
            .last_error
            .as_deref(),
        Some("telemetry envelope exceeds 64 KiB")
    );
    server.host.shutdown().await;
}

#[tokio::test]
async fn paired_clients_cannot_redirect_collectors_or_select_host_credentials() {
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let mut endpoint = sink("https://client-controlled.example/collect".into());
    endpoint.bearer_env = Some("OPENAI_API_KEY".into());
    let mut bytes = Vec::new();
    dispatch::handle_runtime_message(
        ClientMessage::ConfigureTelemetry {
            request_id: "attempt".into(),
            expected_revision: 0,
            sinks: vec![endpoint],
            preserve_auth: vec![],
        },
        &server.host,
        false,
        &mut bytes,
    )
    .await
    .unwrap();
    let frame = read_frame::<ServerFrame>(&mut crate::wire::FrameReader::new(bytes.as_slice()))
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(frame.message, ServerMessage::Rejected { ref code, fatal: false, .. } if code == "operator_required")
    );
    let (_, saved) = ConfigStore::open(root.path().join("state")).unwrap();
    assert!(saved.telemetry.sinks.is_empty());
    assert_eq!(saved.telemetry.revision, 0);
    server.host.shutdown().await;
}

#[tokio::test]
async fn collector_failure_body_cannot_echo_credentials_into_public_status() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut endpoint = sink(format!("http://{}", listener.local_addr().unwrap()));
    endpoint
        .headers
        .insert("x-api-key".into(), "synthetic-secret".into());
    server
        .host
        .configure_telemetry(0, vec![endpoint], &[])
        .await
        .unwrap();
    let receiver = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 8192];
        let received = stream.read(&mut request).await.unwrap();
        assert!(received > 0, "collector received no request");
        let response = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 16\r\nConnection: close\r\n\r\nsynthetic-secret";
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let mut tasks = JoinSet::new();
    Telemetry::tick(&server.host, 0, Trigger::Manual, &mut tasks).await;
    while tasks.join_next().await.is_some() {}
    let (_, reports) = server.host.telemetry_report().await.unwrap();
    assert_eq!(
        reports[0].status.last_error.as_deref(),
        Some("telemetry collector returned HTTP 401")
    );
    receiver.await.unwrap();
    server.host.shutdown().await;
}
