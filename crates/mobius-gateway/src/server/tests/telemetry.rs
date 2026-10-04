use super::*;
use crate::telemetry::{SinkAuth, Trigger};
use crate::telemetry::{Telemetry, TelemetrySink};

fn sink(url: String) -> TelemetrySink {
    serde_json::from_value(serde_json::json!({"id":"test", "url":url, "every_seconds":15, "sections":["activity"], "events":["custom_received"]})).unwrap()
}

#[tokio::test]
async fn telemetry_delivers_headers_snapshots_and_tracks_failures() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut endpoint = sink(format!("http://{}", listener.local_addr().unwrap()));
    let token_path = root.path().join("state/telemetry-token");
    fs::write(&token_path, "test-token").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    endpoint.bearer_file = Some("telemetry-token".into());
    server
        .host
        .configure_telemetry(0, vec![endpoint], &[])
        .await
        .unwrap();
    let receiver = tokio::spawn(async move {
        let mut startup = None;
        for status in [202, 500] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    assert!(headers.contains("authorization: Bearer test-token"));
                    let len: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= end + 4 + len {
                        let envelope: serde_json::Value =
                            serde_json::from_slice(&bytes[end + 4..end + 4 + len]).unwrap();
                        assert_eq!(envelope["reason"], "manual");
                        assert_eq!(envelope["activity"]["connected_clients"], 0);
                        assert_eq!(envelope["protocol_version"], 90);
                        let started_at_ms = envelope["started_at_ms"].as_i64().unwrap();
                        assert!(started_at_ms > 0);
                        assert!(
                            started_at_ms <= envelope["sent_at"].as_i64().unwrap() * 1_000 + 999
                        );
                        let identity = (&envelope["instance"], started_at_ms);
                        if let Some((instance, started)) = &startup {
                            assert_eq!(identity.0, instance);
                            assert_eq!(identity.1, *started);
                        } else {
                            startup = Some((identity.0.clone(), identity.1));
                        }
                        break;
                    }
                }
            }
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Result\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    for (failures, status) in [(0, 202), (1, 500)] {
        let mut tasks = JoinSet::new();
        Telemetry::tick(&server.host, 0, Trigger::Manual, &mut tasks).await;
        while tasks.join_next().await.is_some() {}
        let (_, reports) = server.host.telemetry_report().await.unwrap();
        assert_eq!(reports[0].status.consecutive_failures, failures);
        assert_eq!(reports[0].status.last_status, Some(status));
    }
    receiver.await.unwrap();
    server.host.shutdown().await;
}

#[tokio::test]
async fn telemetry_configuration_rejects_stale_revisions_and_secrets_in_headers() {
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let mut endpoint = sink("https://example.com/collect".into());
    endpoint
        .headers
        .insert("Authorization".into(), "secret".into());
    assert!(
        server
            .host
            .configure_telemetry(0, vec![endpoint.clone()], &[])
            .await
            .is_err()
    );
    endpoint.headers.clear();
    endpoint.bearer_env = Some("PRIVATE_TOKEN_NAME".into());
    server
        .host
        .configure_telemetry(0, vec![endpoint], &[])
        .await
        .unwrap();
    assert!(
        server
            .host
            .configure_telemetry(0, vec![], &[])
            .await
            .is_err()
    );
    let (_, report) = server.host.telemetry_report().await.unwrap();
    assert_eq!(report[0].auth, SinkAuth::BearerEnv);
    assert!(report[0].sink.bearer_env.is_none());
    let mut edited = report[0].sink.clone();
    edited.enabled = false;
    server
        .host
        .configure_telemetry(1, vec![edited], &["test".into()])
        .await
        .unwrap();
    let saved = server.host.telemetry.config().unwrap();
    assert_eq!(
        saved.sinks[0].bearer_env.as_deref(),
        Some("PRIVATE_TOKEN_NAME")
    );
    server.host.shutdown().await;
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

#[cfg(unix)]
#[tokio::test]
async fn sprite_hold_uses_provider_socket_without_an_exec_helper() {
    use tokio::net::UnixListener;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("hold.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let receiver = tokio::spawn(async move {
        for method in ["PUT", "DELETE"] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 1024];
            let count = stream.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..count]);
            assert!(request.starts_with(&format!("{method} /v1/tasks/mobius-gateway HTTP/1.1")));
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        }
    });
    crate::telemetry::update_hold(&path, true).await.unwrap();
    crate::telemetry::update_hold(&path, false).await.unwrap();
    receiver.await.unwrap();
}

#[tokio::test]
async fn storage_reports_flat_limit_and_refreshes_each_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let first = server.host.storage_usage().await.unwrap();
    assert_eq!(first.limit_bytes, None);
    assert_eq!(first.used_bytes, 0);
    fs::write(root.path().join("state/new-file"), b"data").unwrap();
    assert!(
        server
            .host
            .storage_usage()
            .await
            .unwrap()
            .gateway_total
            .bytes
            >= first.gateway_total.bytes + 4
    );
    server.host.shutdown().await;
}

#[test]
fn telemetry_configuration_validation_covers_transport_and_secret_boundaries() {
    let base = serde_json::to_value(sink("https://example.com/collect".into())).unwrap();
    for patch in [
        serde_json::json!({"id":"Invalid"}),
        serde_json::json!({"url":"http://example.com/collect"}),
        serde_json::json!({"url":"https://user:password@example.com"}),
        serde_json::json!({"every_seconds":16}),
        serde_json::json!({"method":"get"}),
        serde_json::json!({"method":"get","events":[],"upload_admission":true}),
        serde_json::json!({"bearer_file":"../token"}),
        serde_json::json!({"bearer_file":"/token"}),
        serde_json::json!({"bearer_file":"token","bearer_env":"TOKEN"}),
        serde_json::json!({"headers":{"X-Test":"value\r\nInjected: true"}}),
    ] {
        let mut value = base.clone();
        value
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        let mut config = GatewayConfig::new("127.0.0.1:8741".parse().unwrap(), None).unwrap();
        config.telemetry.sinks = vec![serde_json::from_value(value).unwrap()];
        assert!(config.validate().is_err(), "accepted {patch}");
    }
    let mut config = GatewayConfig::new("127.0.0.1:8741".parse().unwrap(), None).unwrap();
    let mut authority = sink("https://example.com/collect".into());
    authority.upload_admission = true;
    let mut second = authority.clone();
    second.id = "second".into();
    config.telemetry.sinks = vec![authority, second];
    assert!(config.validate().is_err());
    config.telemetry.sinks[1].enabled = false;
    config.validate().unwrap();
}

#[test]
fn protocol_90_telemetry_and_storage_requests_round_trip() {
    for request in [
        ClientMessage::GetTelemetry {
            request_id: "read".into(),
        },
        ClientMessage::ConfigureTelemetry {
            request_id: "edit".into(),
            expected_revision: 4,
            preserve_auth: Vec::new(),
            sinks: vec![sink("https://example.com/collect".into())],
        },
        ClientMessage::SendTelemetry {
            request_id: "send".into(),
            sink_id: "test".into(),
        },
        ClientMessage::GetStorageUsage {
            request_id: "storage".into(),
        },
    ] {
        let frame = ClientFrame::new(request);
        let encoded = serde_json::to_value(&frame).unwrap();
        assert_eq!(encoded["version"], 90);
        let decoded: ClientFrame = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);
    }
}

#[tokio::test]
async fn idle_exit_delivers_start_and_stop_snapshots() {
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    server
        .host
        .configure_telemetry(
            0,
            vec![sink(format!("http://{}", listener.local_addr().unwrap()))],
            &[],
        )
        .await
        .unwrap();
    let receiver = tokio::spawn(async move {
        for reason in ["start", "stop"] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n")
                    && let Ok(envelope) =
                        serde_json::from_slice::<serde_json::Value>(&bytes[end + 4..])
                {
                    assert_eq!(envelope["reason"], reason);
                    if reason == "stop" {
                        assert_eq!(envelope["cause"], "idle");
                        assert!(envelope.get("activity").is_none());
                        assert!(envelope.get("storage").is_none());
                    }
                    break;
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        }
    });
    tokio::time::timeout(
        Duration::from_secs(3),
        server.serve_until_inactive(std::future::pending(), Duration::from_millis(100)),
    )
    .await
    .unwrap()
    .unwrap();
    receiver.await.unwrap();
}

#[tokio::test]
async fn telemetry_source_failure_does_not_stop_serve_or_drop_request_transport() {
    let root = tempfile::tempdir().unwrap();
    let state_dir = root.path().join("state");
    let (server, _) = configured_test_server(state_dir.clone()).await;
    let mut endpoint = sink("http://127.0.0.1:1".into());
    endpoint.events = vec![crate::wire::HookKind::CustomReceived];
    server
        .host
        .configure_telemetry(0, vec![endpoint], &[])
        .await
        .unwrap();
    let db = rusqlite::Connection::open(state_dir.join("bots.sqlite3")).unwrap();
    db.execute("DROP TABLE telemetry_cursors", []).unwrap();

    for request in [
        ClientMessage::GetTelemetry {
            request_id: "report".into(),
        },
        ClientMessage::SendTelemetry {
            request_id: "send".into(),
            sink_id: "missing".into(),
        },
    ] {
        let mut bytes = Vec::new();
        assert!(
            dispatch::handle_runtime_message(request, &server.host, &mut bytes)
                .await
                .unwrap()
                .is_none()
        );
        let frame = read_frame::<ServerFrame>(&mut crate::wire::FrameReader::new(bytes.as_slice()))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            frame.message,
            ServerMessage::Rejected { fatal: false, .. }
        ));
    }
    tokio::time::timeout(
        Duration::from_secs(3),
        server.serve_until_inactive(std::future::pending(), Duration::from_millis(100)),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn telemetry_enriches_session_without_a_surviving_bot_and_counts_without_decoding() {
    use crate::wire::{HookData, HookEvent, HookSource};
    let root = tempfile::tempdir().unwrap();
    let state_dir = root.path().join("state");
    let (server, _) = configured_test_server(state_dir.clone()).await;
    let mut endpoint = sink("http://127.0.0.1:1".into());
    endpoint.events = vec![crate::wire::HookKind::SessionTurnFinished];
    server
        .host
        .configure_telemetry(0, vec![endpoint.clone()], &[])
        .await
        .unwrap();
    let mut checkpoint = Checkpoint::empty("orphaned-chat");
    checkpoint.session_context.owner_id = "deleted-bot".into();
    SqliteCheckpoint::new(state_dir.join("checkpoints.sqlite3"))
        .unwrap()
        .save(&checkpoint, &[], None)
        .await
        .unwrap();
    let bot = server.bots.bots().unwrap().remove(0);
    server
        .bots
        .record_hook(&HookEvent {
            id: "orphaned-fact".into(),
            bot_id: bot.id,
            occurred_at: 1,
            source: HookSource::Session {
                session_id: checkpoint.session_id.clone(),
            },
            cause_id: None,
            ancestry: vec![],
            data: HookData::SessionTurnFinished {
                session_id: checkpoint.session_id,
                turn_id: "turn".into(),
                outcome: mobius::backend::checkpoint::ExecutionOutcome::Completed,
            },
        })
        .unwrap();
    let (events, cursor, pending) = server.host.telemetry_events(&endpoint).await.unwrap();
    assert_eq!(pending, 1);
    assert!(cursor.is_some());
    assert!(events[0]["session"].is_object());
    assert!(events[0]["session"].get("bot_name").is_none());

    let db = rusqlite::Connection::open(state_dir.join("bots.sqlite3")).unwrap();
    db.execute("UPDATE hook_events SET event_json=json_set(event_json,'$.source',null) WHERE id='orphaned-fact'", []).unwrap();
    assert!(server.bots.telemetry_batch(&endpoint).is_err());
    assert_eq!(server.bots.telemetry_count(&endpoint).unwrap(), 1);
    assert_eq!(
        server.host.telemetry_report().await.unwrap().1[0]
            .status
            .events_pending,
        1
    );
    server.host.shutdown().await;
}

#[tokio::test]
async fn storage_request_rejects_repeat_walks_without_disconnecting() {
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    for (id, expected) in [("first", false), ("second", true)] {
        let mut bytes = Vec::new();
        dispatch::handle_runtime_message(
            ClientMessage::GetStorageUsage {
                request_id: id.into(),
            },
            &server.host,
            &mut bytes,
        )
        .await
        .unwrap();
        let frame = read_frame::<ServerFrame>(&mut crate::wire::FrameReader::new(bytes.as_slice()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            matches!(frame.message, ServerMessage::Rejected { ref code, .. } if code == "rate_limited"),
            expected
        );
    }
    server.host.shutdown().await;
}

#[tokio::test]
async fn missing_telemetry_cursor_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let endpoint = sink("http://127.0.0.1:1".into());
    assert!(server.bots.telemetry_batch(&endpoint).is_err());
    assert!(server.bots.telemetry_count(&endpoint).is_err());
    assert!(server.bots.advance_telemetry(&endpoint.id, 1).is_err());
    server.host.shutdown().await;
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
async fn telemetry_stop_delivery_does_not_wait_for_a_slow_interval_request() {
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    server
        .host
        .configure_telemetry(
            0,
            vec![sink(format!("http://{}", listener.local_addr().unwrap()))],
            &[],
        )
        .await
        .unwrap();
    let mut tasks = JoinSet::new();
    Telemetry::tick(&server.host, 0, Trigger::Interval, &mut tasks).await;
    let (mut slow, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut bytes = [0; 8192];
    assert!(slow.read(&mut bytes).await.unwrap() > 0);
    let receiver = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let size = stream.read(&mut bytes).await.unwrap();
            assert_ne!(size, 0);
            request.extend_from_slice(&bytes[..size]);
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                && let Ok(envelope) =
                    serde_json::from_slice::<serde_json::Value>(&request[end + 4..])
            {
                assert_eq!(envelope["reason"], "stop");
                assert!(envelope.get("activity").is_none());
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    });
    tokio::time::timeout(
        Duration::from_secs(2),
        Telemetry::stop(
            &server.host,
            crate::telemetry::StopCause::Signal,
            &mut tasks,
        ),
    )
    .await
    .unwrap();
    receiver.await.unwrap();
    server.host.shutdown().await;
}

#[tokio::test]
async fn telemetry_transport_diagnostic_preserves_cause_without_collector_url() {
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let endpoint = sink(format!(
        "http://{address}/private-path?secret=private-token"
    ));
    server
        .host
        .configure_telemetry(0, vec![endpoint], &[])
        .await
        .unwrap();
    let mut tasks = JoinSet::new();
    Telemetry::tick(&server.host, 0, Trigger::Manual, &mut tasks).await;
    while tasks.join_next().await.is_some() {}
    let error = server
        .host
        .telemetry
        .status("test")
        .unwrap()
        .last_error
        .unwrap();
    assert!(error.contains("connect"), "{error}");
    assert!(!error.contains("private-path"), "{error}");
    assert!(!error.contains("private-token"), "{error}");
    server.host.shutdown().await;
}
