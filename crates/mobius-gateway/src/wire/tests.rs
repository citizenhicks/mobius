use tokio::io::duplex;

use super::*;

#[tokio::test(start_paused = true)]
async fn stalled_frame_write_times_out_after_partial_output() {
    let (mut writer, mut reader) = duplex(4);
    let error = write_frame(&mut writer, &"payload")
        .await
        .expect_err("stalled write");
    assert!(matches!(error, Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut));
    drop(writer);
    let mut partial = Vec::new();
    reader
        .read_to_end(&mut partial)
        .await
        .expect("partial bytes");
    assert_eq!(partial.len(), 4, "only the frame prefix reached the peer");
}

#[tokio::test]
async fn shared_frame_writes_preserve_the_wire_payload_for_every_subscriber() {
    let frame = ServerFrame::new(ServerMessage::Error {
        code: "test".into(),
        message: "shared payload".repeat(100),
        fatal: false,
    });
    let shared = SharedFrame::encoded(frame.clone()).expect("encode frame");
    let (events, mut first) = tokio::sync::broadcast::channel(1);
    let mut second = events.subscribe();
    events.send(shared.clone()).expect("broadcast");
    let original = shared.clone();
    for delivered in [
        first.recv().await.unwrap(),
        second.recv().await.unwrap(),
        shared,
    ] {
        assert!(std::ptr::eq(&*delivered, &*original));
        let mut bytes = Vec::new();
        delivered
            .write(&mut bytes)
            .await
            .expect("write cached bytes");
        let mut reader = FrameReader::new(bytes.as_slice());
        assert_eq!(
            read_frame::<ServerFrame>(&mut reader).await.unwrap(),
            Some(frame.clone())
        );
    }
}

#[tokio::test]
async fn framed_json_round_trip_preserves_the_versioned_message() {
    let expected = ClientFrame::new(ClientMessage::Authenticate {
        token: "secret".into(),
        client_kind: ClientKind::Cli,
        catalog: Default::default(),
    });
    let (mut writer, reader) = duplex(1024);
    let mut reader = FrameReader::new(reader);

    write_frame(&mut writer, &expected)
        .await
        .expect("write frame");
    let actual: ClientFrame = read_frame(&mut reader)
        .await
        .expect("read frame")
        .expect("frame");

    assert_eq!(actual, expected);
}

#[test]
fn client_frames_reject_unknown_fields() {
    let error = serde_json::from_value::<ClientFrame>(serde_json::json!({
        "version": PROTOCOL_VERSION,
        "type": "list_sessions",
        "request_id": "request-a",
        "unexpected": true,
    }))
    .expect_err("reject unknown client field");

    assert!(error.to_string().contains("unknown field `unexpected`"));
}

#[test]
fn client_request_ids_are_bounded_without_echoing_rejected_values() {
    for request_id in [
        Value::String(String::new()),
        Value::String("x".repeat(257)),
        Value::String("é".repeat(129)),
        Value::Null,
    ] {
        let error = serde_json::from_value::<ClientFrame>(serde_json::json!({
            "version": PROTOCOL_VERSION,
            "type": "list_sessions",
            "request_id": request_id,
        }))
        .expect_err("reject invalid request ID");
        assert_eq!(error.to_string(), "request ID must be 1–256 bytes");
    }
}

#[test]
fn session_file_bytes_use_standard_base64_and_round_trip() {
    let expected = ClientFrame::new(ClientMessage::UploadSessionFileChunk {
        request_id: "request-upload".into(),
        session_id: "session-a".into(),
        upload_id: "upload-a".into(),
        offset: 0,
        data: vec![0, 0xff],
    });

    let encoded = serde_json::to_value(&expected).expect("encode upload chunk");
    let decoded: ClientFrame =
        serde_json::from_value(encoded.clone()).expect("decode upload chunk");

    assert_eq!(encoded["type"], "upload_session_file_chunk");
    assert_eq!(encoded["data"], "AP8=");
    assert_eq!(decoded, expected);
}

#[tokio::test]
async fn websocket_diagnostics_preserve_safe_metadata_not_peer_details() {
    use tokio_tungstenite::tungstenite::{error::UrlError, http::Response};

    let details = "private-peer-data";
    for (error, expected) in [
        (
            WebSocketError::Url(UrlError::UnableToConnect(details.into())),
            "WebSocket URL failure",
        ),
        (
            WebSocketError::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                details,
            )),
            "WebSocket I/O failure: ConnectionReset",
        ),
        (
            WebSocketError::Http(Box::new(
                Response::builder()
                    .status(429)
                    .header("x-peer-details", details)
                    .body(Some(details.as_bytes().to_vec()))
                    .expect("HTTP error response"),
            )),
            "WebSocket handshake failed: HTTP 429",
        ),
    ] {
        let error = websocket_error(error);
        assert_eq!(
            error.to_string(),
            format!("gateway protocol error: {expected}")
        );
        assert!(!format!("{error:?}").contains(details));
    }
}

#[test]
fn ssh_identity_wire_record_has_no_key_or_path_material() {
    let frame = ServerFrame::new(ServerMessage::SshIdentities {
        request_id: "ssh-list".into(),
        identities: vec![SshIdentityRecord {
            label: "id_ed25519".into(),
            algorithm: "ssh-ed25519".into(),
            fingerprint: "SHA256:safe-summary".into(),
        }],
    });
    let encoded = serde_json::to_value(frame).expect("encode SSH identities");

    assert_eq!(encoded["type"], "ssh_identities");
    assert!(encoded["identities"][0].get("private_key").is_none());
    assert!(encoded["identities"][0].get("public_key").is_none());
    assert!(encoded["identities"][0].get("path").is_none());
    assert!(encoded["identities"][0].get("comment").is_none());
}

#[tokio::test]
async fn framed_reader_retains_a_partial_prefix_when_cancelled() {
    let first = ClientFrame::new(ClientMessage::ListRoutines {
        request_id: "request-a".into(),
        bot_id: None,
    });
    let second = ClientFrame::new(ClientMessage::GetProfile {
        request_id: "request-b".into(),
        include_provider_usage: false,
    });
    let encode = |frame: &ClientFrame| {
        let payload = serde_json::to_vec(frame).expect("encode frame");
        let mut encoded = u32::try_from(payload.len())
            .expect("frame length")
            .to_be_bytes()
            .to_vec();
        encoded.extend_from_slice(&payload);
        encoded
    };
    let mut encoded = encode(&first);
    encoded.extend_from_slice(&encode(&second));
    let (mut writer, reader) = duplex(4096);
    let mut reader = FrameReader::new(reader);
    writer
        .write_all(&encoded[..1])
        .await
        .expect("write partial prefix");

    {
        let pending = read_frame::<ClientFrame>(&mut reader);
        tokio::pin!(pending);
        tokio::select! {
            biased;
            result = &mut pending => panic!("partial frame completed: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
    }
    writer.write_all(&encoded[1..]).await.expect("finish frame");
    let actual_first = read_frame::<ClientFrame>(&mut reader)
        .await
        .expect("read resumed frame")
        .expect("frame");
    let actual_second = read_frame::<ClientFrame>(&mut reader)
        .await
        .expect("read buffered frame")
        .expect("frame");

    assert_eq!([actual_first, actual_second], [first, second]);
}

#[test]
fn git_credential_status_returns_only_safe_metadata() {
    let request = serde_json::to_value(ClientFrame::new(ClientMessage::ApproveGitCredential {
        request_id: "request-credential".into(),
        target: "https://git.example.com".into(),
        username: "octo".into(),
        token: "secret".into(),
    }))
    .expect("encode Git credential approval");
    let response = serde_json::to_value(ServerFrame::new(ServerMessage::GitCredentialStatus {
        request_id: "request-credential".into(),
        available: true,
        username: Some("octo".into()),
    }))
    .expect("encode Git credential status");

    assert_eq!(request["type"], "approve_git_credential");
    assert_eq!(request["token"], "secret");
    assert_eq!(response["type"], "git_credential_status");
    assert_eq!(response["available"], true);
    assert_eq!(response["username"], "octo");
    assert!(response.get("target").is_none());
    assert!(response.get("token").is_none());
}

#[test]
fn server_frame_decodes_session_opened_with_a_widget_action_tag() {
    let encoded = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "type": "session_opened",
        "request_id": "request-open",
        "payload": {
            "active_turn_ids": [],
            "pending_approvals": [],
            "latest_sequence": 4,
            "attached_folders": [],
            "next_before_sequence": 2,
            "workspace": { "id": "workspace-a", "path": "/workspace" },
            "git": null,
            "session": {
                "session_id": "session-a",
                "context": {
                    "owner_id": "bot-a"
                },
                "model": {
                    "route": "default",
                    "model": "model-a",
                    "reasoning_effort": null,
                    "model_context_window": null
                }
            },
            "contributions": [{
                "capability": "subagents",
                "accepts_file_attachments": false,
                "commands": [],
                "widgets": [{
                    "id": "subagents",
                    "slot": "header",
                    "text": "subagents",
                    "tone": "neutral",
                    "symbol": null,
                    "icon_only": false,
                    "progress": null,
                    "content": null,
                    "action": {
                        "type": "capability_command",
                        "capability": "subagents",
                        "command": "subagents",
                        "arguments": "",
                        "input": null,
                        "target": null
                    }
                }],
                "references": []
            }],
            "widgets": [],
            "context_limit_tokens": 250000,
            "active_message_delivery": "steer",
            "run_stats": {
                "run_count": 0,
                "failed_run_count": 0,
                "aborted_run_count": 0,
                "model_calls": 0,
                "tool_calls": 0,
                "failed_tool_calls": 0,
                "elapsed_ms": 0,
                "usage": {
                    "input_tokens": 0,
                    "cached_input_tokens": 0,
                    "cache_write_input_tokens": 0,
                    "output_tokens": 0,
                    "reasoning_output_tokens": 0,
                    "total_tokens": 0
                },
                "active": null
            }
        }
    });

    let frame: ServerFrame = serde_json::from_value(encoded).expect("decode nested session ready");
    let ServerMessage::SessionOpened {
        request_id,
        payload,
    } = frame.message
    else {
        panic!("expected session-opened frame");
    };

    assert_eq!(
        (
            request_id.as_str(),
            payload.session.session_id.as_str(),
            payload.next_before_sequence,
            payload.context_limit_tokens,
            payload.contributions[0].widgets[0].action.is_some(),
        ),
        ("request-open", "session-a", Some(2), Some(250_000), true)
    );
}

#[tokio::test]
async fn read_frame_rejects_an_oversized_declared_payload() {
    let (mut writer, reader) = duplex(8);
    let mut reader = FrameReader::new(reader);
    let oversized = u32::try_from(MAX_FRAME_BYTES + 1).expect("frame limit fits u32");
    writer
        .write_all(&oversized.to_be_bytes())
        .await
        .expect("write prefix");

    let error = read_frame::<ClientFrame>(&mut reader)
        .await
        .expect_err("oversized frame must fail");

    assert!(matches!(error, Error::Protocol(_)), "{error}");
}

#[test]
fn validate_version_requires_the_exact_protocol() {
    for version in [PROTOCOL_VERSION - 1, PROTOCOL_VERSION + 1] {
        let error = validate_version(version).expect_err("incompatible version must fail");
        assert!(matches!(error, Error::Protocol(_)), "{error}");
    }
}
