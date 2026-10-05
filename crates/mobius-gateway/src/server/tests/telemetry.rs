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
        fs::set_permissions(&token_path, mobius::owner_only::file()).unwrap();
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
                        assert_eq!(envelope["protocol_version"], crate::wire::PROTOCOL_VERSION);
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
    endpoint.bearer_env = Some("MOBIUS_PRIVATE_TOKEN_NAME".into());
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
    assert!(
        server
            .host
            .configure_telemetry(1, vec![report[0].sink.clone()], &["test".into()])
            .await
            .is_err()
    );
    // The local operator edits the complete destination from private configuration.
    let mut edited = server.host.telemetry.config().unwrap().sinks[0].clone();
    edited.bearer_env = None;
    edited.enabled = false;
    server
        .host
        .configure_telemetry(1, vec![edited], &["test".into()])
        .await
        .unwrap();
    let saved = server.host.telemetry.config().unwrap();
    assert_eq!(
        saved.sinks[0].bearer_env.as_deref(),
        Some("MOBIUS_PRIVATE_TOKEN_NAME")
    );
    server.host.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn telemetry_sink_updates_preserve_the_private_operator_activity_hook() {
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    server.host.shutdown().await;
    drop(server);
    let (store, mut config) = ConfigStore::open(root.path().join("state")).unwrap();
    config.telemetry.activity_hook = Some(crate::telemetry::ActivityHookConfig {
        command: vec!["/bin/cat".into()],
        idle_grace_seconds: 7,
        timeout_seconds: 2,
        retry_seconds: 3,
    });
    store.save(&config).unwrap();
    let server = GatewayServer::open(root.path().join("state"))
        .await
        .unwrap();
    let mut bytes = Vec::new();
    dispatch::handle_runtime_message(
        ClientMessage::ConfigureTelemetry {
            request_id: "sinks".into(),
            expected_revision: 0,
            sinks: vec![sink("https://collector.example/collect".into())],
            preserve_auth: vec![],
        },
        &server.host,
        true,
        &mut bytes,
    )
    .await
    .unwrap();
    let frame = read_frame::<ServerFrame>(&mut crate::wire::FrameReader::new(bytes.as_slice()))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        &frame.message,
        ServerMessage::Telemetry { revision: 1, .. }
    ));
    let response = serde_json::to_value(&frame.message).unwrap();
    assert!(response.get("activity_hook").is_none());
    assert!(!response.to_string().contains("/bin/cat"));
    let live = server.host.telemetry.config().unwrap();
    assert_eq!(
        live.activity_hook.as_ref(),
        config.telemetry.activity_hook.as_ref()
    );
    let (_, saved) = ConfigStore::open(root.path().join("state")).unwrap();
    assert_eq!(saved.telemetry.revision, 1);
    assert_eq!(saved.telemetry.sinks.len(), 1);
    assert_eq!(
        saved.telemetry.activity_hook,
        config.telemetry.activity_hook
    );
    server.host.shutdown().await;
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

#[cfg(unix)]
#[tokio::test]
async fn server_shutdown_and_access_lease_expiry_release_activity_hook_descendants() {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    for expires in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let pid_file = root.path().join("child.pid");
        let (mut server, _) = configured_test_server(root.path().join("state")).await;
        server.config.telemetry.activity_hook = Some(crate::telemetry::ActivityHookConfig {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                r#"sleep 30 & child=$!; printf '%s\n' "$child" > "$1"; /bin/cat >/dev/null; kill "$child"; wait "$child""#.into(),
                "activity-test".into(),
                pid_file.display().to_string(),
            ],
            idle_grace_seconds: 60,
            timeout_seconds: 1,
            retry_seconds: 1,
        });
        if expires {
            server.access_lease = Some(AccessLease {
                expires_at: SystemTime::now() + Duration::from_secs(3),
                deadline: Instant::now() + Duration::from_secs(3),
            });
        }
        let ready = server.notify_ready();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let serving = tokio::spawn(server.serve_until(async {
            let _ = stopped.await;
        }));
        ready.await.unwrap();
        let child = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(pid) = tokio::fs::read_to_string(&pid_file).await
                    && let Ok(pid) = pid.trim().parse()
                {
                    break Pid::from_raw(pid);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("activity hook child started");
        assert!(kill(child, None).is_ok());
        if !expires {
            stop.send(()).unwrap();
        }
        tokio::time::timeout(Duration::from_secs(10), serving)
            .await
            .expect("server shutdown released the activity hook")
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while kill(child, None).is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("activity hook descendant was terminated");
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
fn telemetry_and_storage_requests_round_trip() {
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
        assert_eq!(encoded["version"], crate::wire::PROTOCOL_VERSION);
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
            dispatch::handle_runtime_message(request, &server.host, true, &mut bytes)
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
async fn storage_request_returns_recent_measurement_when_cleanup_is_reopened() {
    let root = tempfile::tempdir().unwrap();
    let (mut server, grant) = configured_test_server(root.path().join("state")).await;
    let endpoint: Endpoint = format!("tcp://{}", server.listen_addr()).parse().unwrap();
    let ready = server.notify_ready();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async {
        let _ = stopped.await;
    }));
    ready.await.unwrap();
    let (client, _) =
        GatewayClient::pair(&endpoint, grant.code, "storage review", ClientKind::Macos)
            .await
            .unwrap();
    let (sender, mut events) = client.into_parts();
    wait_gateway_ready(&mut events).await;
    let mut first = None;
    for id in ["connected", "cleanup-open", "cleanup-reopen"] {
        sender
            .send(ClientMessage::GetStorageUsage {
                request_id: id.into(),
            })
            .await
            .unwrap();
        let usage = loop {
            match next_gateway_message(&mut events).await {
                ServerMessage::StorageUsage { request_id, usage } if request_id == id => {
                    break usage;
                }
                ServerMessage::Rejected {
                    request_id,
                    message,
                    ..
                } if request_id == id => panic!("storage request rejected: {message}"),
                _ => {}
            }
        };
        if let Some(previous) = &first {
            assert_eq!(&usage, previous);
        } else {
            first = Some(usage);
            // A second disk walk would see this change; immediate requests share
            // the result rather than defeating the bounded measurement cache.
            fs::write(root.path().join("state/later-file"), b"later").unwrap();
        }
    }
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn storage_requests_share_an_in_flight_measurement() {
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let (first, second) = tokio::join!(
        server.host.storage_usage_request(),
        server.host.storage_usage_request(),
    );
    assert_eq!(first.unwrap(), second.unwrap());
    server.host.shutdown().await;
}

#[tokio::test]
async fn storage_request_refreshes_after_the_completed_measurement_expires() {
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let first = server.host.storage_usage_request().await.unwrap();
    fs::write(root.path().join("state/later-file"), b"later").unwrap();
    assert_eq!(server.host.storage_usage_request().await.unwrap(), first);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let refreshed = server.host.storage_usage_request().await.unwrap();
    assert!(refreshed.gateway_total.bytes >= first.gateway_total.bytes + 5);
    server.host.shutdown().await;
}

#[tokio::test]
async fn storage_request_measures_freed_files_immediately_after_purge() {
    use mobius::backend::session_files::{SessionFileOrigin, SessionFileSelection};
    let root = tempfile::tempdir().unwrap();
    let state_dir = root.path().join("state");
    let (server, _) = configured_test_server(state_dir.clone()).await;
    let mut checkpoint = Checkpoint::empty("storage-purge");
    checkpoint.session_context.owner_id = server.bots.bots().unwrap().remove(0).id;
    SqliteCheckpoint::new(state_dir.join("checkpoints.sqlite3"))
        .unwrap()
        .save(&checkpoint, &[], None)
        .await
        .unwrap();
    let files = server.host.session_file_store().await;
    files
        .publish_artifact(
            &checkpoint.session_id,
            "result.txt".into(),
            "text/plain".into(),
            b"result",
        )
        .await
        .unwrap();
    let first = server.host.storage_usage_request().await.unwrap();
    assert_eq!(first.used_bytes, 6);
    server
        .host
        .delete_sessions(
            &[checkpoint.session_id],
            SessionFileSelection::Origins(vec![SessionFileOrigin::Artifact]),
        )
        .await
        .unwrap();
    let refreshed = server.host.storage_usage_request().await.unwrap();
    assert_eq!(refreshed.used_bytes, 0);
    assert_eq!(refreshed.sessions[0].artifacts.files, 0);
    assert_eq!(refreshed.session_count, first.session_count);
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
async fn telemetry_mutation_requires_authenticated_local_operator_identity_on_wire() {
    let root = tempfile::tempdir().unwrap();
    let (mut server, grant) = configured_test_server(root.path().join("state")).await;
    let operator = server.auth.provision_local_client().unwrap();
    let paired = server
        .auth
        .pair(&grant.code, "local gateway operator")
        .unwrap();
    let endpoint: Endpoint = format!("tcp://{}", server.config.listen).parse().unwrap();
    let ready = server.notify_ready();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async {
        let _ = stopped.await;
    }));
    ready.await.unwrap();
    for (token, authorized) in [(paired.token, false), (operator.token, true)] {
        let client = GatewayClient::connect(&endpoint, &token, ClientKind::GatewayDashboard)
            .await
            .unwrap();
        let (sender, mut events) = client.into_parts();
        wait_gateway_ready(&mut events).await;
        sender
            .send(ClientMessage::ConfigureTelemetry {
                request_id: "policy".into(),
                expected_revision: 0,
                sinks: vec![],
                preserve_auth: vec![],
            })
            .await
            .unwrap();
        loop {
            match next_gateway_message(&mut events).await {
                ServerMessage::Rejected {
                    request_id: id,
                    code,
                    fatal,
                    ..
                } if id == "policy" => {
                    assert!(!authorized);
                    assert_eq!(code, "operator_required");
                    assert!(!fatal);
                    break;
                }
                ServerMessage::Telemetry {
                    request_id,
                    revision,
                    ..
                } if request_id == "policy" => {
                    assert!(authorized);
                    assert_eq!(revision, 1);
                    break;
                }
                _ => {}
            }
        }
    }
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn public_telemetry_reports_hide_url_and_custom_header_credentials() {
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let mut endpoint = sink("https://collector.example/secret-path?key=secret-query".into());
    endpoint
        .headers
        .insert("x-api-key".into(), "secret-header".into());
    let original = endpoint.clone();
    server
        .host
        .configure_telemetry(0, vec![endpoint], &[])
        .await
        .unwrap();
    let (_, reports) = server.host.telemetry_report().await.unwrap();
    assert_eq!(reports[0].sink.url, "https://collector.example");
    assert!(reports[0].sink.headers.is_empty());
    assert_eq!(server.host.telemetry.config().unwrap().sinks[0], original);
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
