use tokio::io::duplex;

use super::*;

#[tokio::test]
async fn framed_json_round_trip_preserves_the_versioned_message() {
    let expected = ClientFrame::new(ClientMessage::Authenticate {
        token: "secret".into(),
        client_kind: ClientKind::Cli,
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

#[test]
fn session_file_deletion_round_trips() {
    let expected = ClientFrame::new(ClientMessage::DeleteSessionFile {
        request_id: "request-delete".into(),
        session_id: "session-a".into(),
        file_id: "file-a".into(),
    });

    let encoded = serde_json::to_value(&expected).expect("encode file deletion");
    let decoded: ClientFrame =
        serde_json::from_value(encoded.clone()).expect("decode file deletion");

    assert_eq!(encoded["type"], "delete_session_file");
    assert_eq!(decoded, expected);
}

#[test]
fn session_file_list_carries_the_file_origin() {
    let frame = ServerFrame::new(ServerMessage::SessionFiles {
        request_id: "request-list".into(),
        session_id: "session-a".into(),
        files: vec![SessionFileRecord {
            origin: mobius::protocol::SessionFileOrigin::Agent,
            file: SessionFileReference {
                id: "file-a".into(),
                name: "report.txt".into(),
                size: 6,
                media_type: "text/plain".into(),
            },
        }],
    });

    let encoded = serde_json::to_value(frame).expect("encode session files");

    assert_eq!(encoded["files"][0]["origin"], "agent");
}

#[test]
fn swarm_message_records_use_text_as_the_payload_name() {
    let record = SwarmMessageRecord {
        id: "message-a".into(),
        sequence: 1,
        author_bot_id: "bot-a".into(),
        author_handle: "reviewer".into(),
        source_session_id: "session-a".into(),
        text: "Review this".into(),
        created_at_ms: 1,
        in_reply_to_message_id: None,
        reply_depth: 0,
    };

    let encoded = serde_json::to_value(record).expect("encode swarm message");

    assert_eq!(
        encoded,
        serde_json::json!({
            "id": "message-a",
            "sequence": 1,
            "author_bot_id": "bot-a",
            "author_handle": "reviewer",
            "source_session_id": "session-a",
            "text": "Review this",
            "created_at_ms": 1,
            "in_reply_to_message_id": null,
            "reply_depth": 0,
        })
    );
}

#[tokio::test]
async fn websocket_bridge_rejects_text_messages() {
    let incoming = futures_util::stream::iter([Ok(Message::Text("{}".into()))]);
    let (writer, _reader) = duplex(64);

    let error = websocket_to_framed(incoming, writer)
        .await
        .expect_err("text message must fail");

    assert!(error.to_string().contains("must be binary"));
}

#[tokio::test(start_paused = true)]
async fn idle_framed_websocket_sends_a_ping() {
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::protocol::Role;

    let (server_socket, client_socket) = duplex(1024);
    let (server, mut client) = tokio::join!(
        WebSocketStream::from_raw_socket(server_socket, Role::Server, None),
        WebSocketStream::from_raw_socket(client_socket, Role::Client, None),
    );
    let (outgoing, _incoming) = server.split();
    let (_framed_writer, framed_reader) = duplex(64);
    let bridge = tokio::spawn(framed_to_websocket(framed_reader, outgoing));
    tokio::task::yield_now().await;

    tokio::time::advance(WEBSOCKET_KEEPALIVE_INTERVAL).await;
    let message = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .expect("WebSocket keepalive timeout")
        .expect("WebSocket closed before keepalive")
        .expect("read WebSocket keepalive");

    assert!(matches!(message, Message::Ping(payload) if payload.is_empty()));
    bridge.abort();
}

#[test]
fn connection_handshakes_have_no_session_replay_cursor() {
    let frames = [
        ClientMessage::Authenticate {
            token: "secret".into(),
            client_kind: ClientKind::Cli,
        },
        ClientMessage::Pair {
            code: "pairing-code".into(),
            client_label: "client".into(),
            client_kind: ClientKind::Macos,
        },
    ];

    let has_cursor = frames
        .into_iter()
        .map(ClientFrame::new)
        .map(|frame| serde_json::to_value(frame).expect("encode handshake"))
        .any(|value| value.get("last_sequence").is_some());

    assert!(!has_cursor);
}

#[test]
fn saved_provider_credential_has_no_impossible_status_flag() {
    let frame = ServerFrame::new(ServerMessage::ProviderCredentialSaved {
        instance: "kimi".into(),
        request_id: "credential-1".into(),
        provider: "openai_socket".into(),
    });
    let encoded = serde_json::to_value(frame).expect("encode credential response");

    assert_eq!(encoded["type"], "provider_credential_saved");
    assert!(encoded.get("configured").is_none());
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
fn client_frame_round_trip_preserves_a_unified_message() {
    let expected = ClientFrame::new(ClientMessage::Submit {
        session_id: "session-a".into(),
        submission: Submission {
            id: "submission-a".into(),
            op: Op::Message {
                message: mobius::protocol::MessageSubmission {
                    author: mobius::protocol::MessageAuthor::User,
                    text: "follow up".into(),
                    attachments: Vec::new(),
                    reply: None,
                    requested_delivery: Some(mobius::protocol::ActiveMessageDelivery::Queue),
                    target_turn_id: Some("turn-a".into()),
                },
            },
        },
    });

    let encoded = serde_json::to_value(&expected).expect("encode nested submission");
    let actual: ClientFrame =
        serde_json::from_value(encoded.clone()).expect("decode nested submission");

    assert_eq!(
        encoded["submission"]["op"]["message"]["requested_delivery"],
        "queue"
    );
    assert_eq!(actual, expected);
}

#[test]
fn rendered_preview_round_trip_preserves_page_metadata_and_continuation() {
    let expected = RenderedPreview {
        id: "/root/reviewer".into(),
        title: "reviewer".into(),
        subtitle: "Last 1 turn".into(),
        page_id: "/root/reviewer:before-51".into(),
        update: FrontendPreviewUpdate::Prepend,
        events: vec![RenderedEvent {
            recorded_at_ms: 42,
            event: EventMsg::ContextCompacted,
            blocks: Vec::new(),
        }],
        next: Some(Op::CapabilityCommand {
            capability: "subagents".into(),
            command: "subagents".into(),
            arguments: r#"{"path":"/root/reviewer","before_sequence":2}"#.into(),
            input: None,
            target: None,
        }),
    };

    let encoded = serde_json::to_value(&expected).expect("encode rendered preview");
    let actual: RenderedPreview =
        serde_json::from_value(encoded.clone()).expect("decode rendered preview");

    assert_eq!(encoded["update"], "prepend");
    assert_eq!(encoded["page_id"], "/root/reviewer:before-51");
    assert_eq!(encoded["events"][0]["recorded_at_ms"], 42);
    assert_eq!(actual, expected);
}

#[test]
fn protocol_v10_rejects_an_untargeted_capability_shape() {
    let frame = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "type": "submit",
        "session_id": "session-a",
        "submission": {
            "id": "submission-a",
            "op": {
                "type": "capability_command",
                "capability": "sessions",
                "command": "fork",
                "arguments": "",
                "input": null
            }
        }
    });

    let error = serde_json::from_value::<ClientFrame>(frame)
        .expect_err("v10 capability commands require an explicit target field");

    assert!(error.to_string().contains("missing field `target`"));
}

#[test]
fn session_creation_requires_a_gateway_workspace_and_bot() {
    let frame = ClientFrame::new(ClientMessage::CreateSession {
        request_id: "request-a".into(),
        workspace: PathBuf::from("/srv/mobius/project"),
        bot_id: "bot-a".into(),
    });

    let encoded = serde_json::to_value(frame).expect("encode session creation");

    assert_eq!(
        encoded,
        serde_json::json!({
            "version": PROTOCOL_VERSION,
            "type": "create_session",
            "request_id": "request-a",
            "workspace": "/srv/mobius/project",
            "bot_id": "bot-a"
        })
    );
}

#[test]
fn swarm_management_frames_keep_request_correlation_out_of_broadcasts() {
    let create = ClientFrame::new(ClientMessage::CreateSwarm {
        request_id: "request-swarm".into(),
        title: "Review team".into(),
        leader_bot_id: "leader".into(),
        member_bot_ids: vec!["leader".into(), "reviewer".into()],
    });
    let broadcast = ServerFrame::new(ServerMessage::Swarms {
        request_id: None,
        swarms: Vec::new(),
    });

    assert_eq!(
        serde_json::to_value(create).expect("encode swarm creation"),
        serde_json::json!({
            "version": PROTOCOL_VERSION,
            "type": "create_swarm",
            "request_id": "request-swarm",
            "title": "Review team",
            "leader_bot_id": "leader",
            "member_bot_ids": ["leader", "reviewer"]
        })
    );
    assert!(
        serde_json::to_value(broadcast)
            .expect("encode swarm broadcast")
            .get("request_id")
            .is_none()
    );
}

#[test]
fn swarm_attention_frames_carry_the_pending_board_message() {
    let frame = ServerFrame::new(ServerMessage::SwarmAttentions {
        attentions: vec![SwarmAttention {
            swarm_id: "swarm-a".into(),
            swarm_title: "Review team".into(),
            message_id: "message-a".into(),
            bot_id: "bot-a".into(),
            text: "Choose the release scope".into(),
        }],
    });

    assert_eq!(
        serde_json::to_value(frame).expect("encode Swarm attention"),
        serde_json::json!({
            "version": PROTOCOL_VERSION,
            "type": "swarm_attentions",
            "attentions": [{
                "swarm_id": "swarm-a",
                "swarm_title": "Review team",
                "message_id": "message-a",
                "bot_id": "bot-a",
                "text": "Choose the release scope"
            }]
        })
    );
}

#[test]
fn swarm_chat_and_hidden_bot_session_frames_are_gateway_scoped() {
    let post = ClientFrame::new(ClientMessage::PostSwarmMessage {
        request_id: "request-post".into(),
        swarm_id: "swarm-a".into(),
        text: "@reviewer check this".into(),
    });
    let list = ClientFrame::new(ClientMessage::ListBotSessions {
        request_id: "request-hidden".into(),
        bot_id: "bot-a".into(),
    });
    let response = ServerFrame::new(ServerMessage::BotSessions {
        request_id: "request-hidden".into(),
        bot_id: "bot-a".into(),
        sessions: Vec::new(),
    });

    assert_eq!(
        serde_json::to_value(post).expect("encode human swarm post"),
        serde_json::json!({
            "version": PROTOCOL_VERSION,
            "type": "post_swarm_message",
            "request_id": "request-post",
            "swarm_id": "swarm-a",
            "text": "@reviewer check this"
        })
    );
    assert_eq!(
        serde_json::to_value(list).expect("encode hidden Bot sessions request")["type"],
        "list_bot_sessions"
    );
    assert_eq!(
        serde_json::to_value(response).expect("encode hidden Bot sessions response"),
        serde_json::json!({
            "version": PROTOCOL_VERSION,
            "type": "bot_sessions",
            "request_id": "request-hidden",
            "bot_id": "bot-a",
            "sessions": []
        })
    );
}

#[test]
fn workspace_directory_creation_uses_a_gateway_host_parent_and_name() {
    let frame = ClientFrame::new(ClientMessage::CreateWorkspaceDirectory {
        request_id: "request-directory".into(),
        parent: PathBuf::from("/srv/mobius"),
        name: "project".into(),
    });

    let encoded = serde_json::to_value(frame).expect("encode workspace directory creation");

    assert_eq!(
        encoded,
        serde_json::json!({
            "version": PROTOCOL_VERSION,
            "type": "create_workspace_directory",
            "request_id": "request-directory",
            "parent": "/srv/mobius",
            "name": "project"
        })
    );
}

#[test]
fn provider_registration_is_gateway_scoped() {
    let frame = ClientFrame::new(ClientMessage::RegisterProvider {
        request_id: "request-provider".into(),
        config: ProviderConfig {
            instance: "kimi".into(),
            provider: "kimi".into(),
            model: "kimi-k3".into(),
            base_url: None,
            endpoint_auth: crate::wire::ProviderEndpointAuth::ProviderDefault,
            reasoning_effort: Some("max".into()),
            web_search: HostedWebSearch::Off,
        },
        label: "Kimi".into(),
        tint: Default::default(),
        model_ids: Vec::new(),
        reasoning_efforts: Vec::new(),
    });

    let encoded = serde_json::to_value(frame).expect("encode provider registration");

    assert_eq!(encoded["type"], "register_provider");
    assert_eq!(encoded["config"]["provider"], "kimi");
    assert_eq!(encoded["config"]["endpoint_auth"], "provider_default");
    assert_eq!(encoded["model_ids"], serde_json::json!([]));
    assert_eq!(encoded["reasoning_efforts"], serde_json::json!([]));
    assert!(encoded.get("session_id").is_none());
}

#[test]
fn provider_removal_is_gateway_scoped() {
    let encoded = serde_json::to_value(ClientFrame::new(ClientMessage::RemoveProvider {
        request_id: "request-provider".into(),
        instance: "kimi-work".into(),
    }))
    .expect("encode provider removal");

    assert_eq!(
        encoded,
        serde_json::json!({
            "version": PROTOCOL_VERSION,
            "type": "remove_provider",
            "request_id": "request-provider",
            "instance": "kimi-work"
        })
    );
}

#[test]
fn provider_registration_requires_a_reasoning_catalog() {
    let frame = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "type": "register_provider",
        "request_id": "request-provider",
        "config": {
            "instance": "openrouter",
            "provider": "openrouter",
            "model": "openai/gpt-5",
            "endpoint_auth": "provider_default",
            "reasoning_effort": "high",
            "web_search": "off"
        },
        "label": "OpenRouter",
        "tint": "blue",
        "model_ids": ["openai/gpt-5"]
    });

    let error = serde_json::from_value::<ClientFrame>(frame)
        .expect_err("provider registration requires reasoning efforts");

    assert!(
        error
            .to_string()
            .contains("missing field `reasoning_efforts`")
    );
}

#[test]
fn provider_config_rejects_api_key_environment_overrides() {
    let encoded = serde_json::json!({
        "provider": "kimi",
        "model": "kimi-k3",
        "api_key_env": "CUSTOM_KIMI_API_KEY"
    });

    let error = serde_json::from_value::<ProviderConfig>(encoded)
        .expect_err("provider environment overrides must be rejected");

    assert!(error.to_string().contains("unknown field `api_key_env`"));
}

#[test]
fn opening_a_session_owns_its_replay_cursor() {
    let frame = ClientFrame::new(ClientMessage::OpenSession {
        request_id: "request-open".into(),
        session_id: "session-a".into(),
        last_sequence: Some(7),
    });

    let encoded = serde_json::to_value(frame).expect("encode session open");

    assert_eq!(encoded["last_sequence"], 7);
    assert!(encoded.get("replay_epoch").is_none());
}

#[test]
fn routine_management_is_bot_owned_and_structured() {
    let frames = [
        ClientMessage::ListRoutines {
            request_id: "list".into(),
            bot_id: Some("bot-a".into()),
        },
        ClientMessage::UpdateRoutine {
            request_id: "reschedule".into(),
            id: "routine-a".into(),
            bot_id: "bot-a".into(),
            workspace: PathBuf::from("/srv/mobius/project"),
            instructions: "Review pull requests".into(),
            schedule: RoutineSchedule {
                kind: RoutineScheduleKind::Cron,
                at: None,
                every_seconds: None,
                expression: Some("0 9 * * *".into()),
                time_zone: Some("UTC".into()),
            },
            ends_at: None,
            enabled: true,
        },
        ClientMessage::DeleteRoutine {
            request_id: "delete".into(),
            id: "routine-a".into(),
        },
        ClientMessage::RunRoutine {
            request_id: "run".into(),
            id: "routine-a".into(),
        },
        ClientMessage::ListRoutineHistory {
            request_id: "history".into(),
            id: None,
        },
    ];

    let encoded = frames
        .into_iter()
        .map(ClientFrame::new)
        .map(|frame| serde_json::to_value(frame).expect("encode routine operation"))
        .collect::<Vec<_>>();

    assert!(
        encoded
            .iter()
            .all(|value| value.get("session_id").is_none())
    );
    assert_eq!(encoded[1]["schedule"]["kind"], "cron");
    assert_eq!(encoded[1]["schedule"]["expression"], "0 9 * * *");
}

#[test]
fn routine_run_preview_is_correlated_and_cursored() {
    let request = ClientFrame::new(ClientMessage::GetRoutineRunPreview {
        request_id: "preview".into(),
        id: "run-a".into(),
        before_sequence: Some(8),
    });
    let encoded = serde_json::to_value(request).expect("encode preview request");

    assert_eq!(encoded["type"], "get_routine_run_preview");
    assert_eq!(encoded["id"], "run-a");
    assert_eq!(encoded["before_sequence"], 8);
}

#[test]
fn routine_run_deletion_is_correlated() {
    let request = ClientFrame::new(ClientMessage::DeleteRoutineRun {
        request_id: "delete-run".into(),
        id: "run-a".into(),
    });
    let encoded = serde_json::to_value(request).expect("encode run deletion");

    assert_eq!(encoded["type"], "delete_routine_run");
    assert_eq!(encoded["request_id"], "delete-run");
    assert_eq!(encoded["id"], "run-a");
}

#[test]
fn session_actions_have_flat_authenticated_frames() {
    let rename = serde_json::to_value(ClientFrame::new(ClientMessage::RenameSession {
        request_id: "request-a".into(),
        session_id: "session-a".into(),
        title: "Renamed chat".into(),
    }))
    .expect("encode rename");
    let pin = serde_json::to_value(ClientFrame::new(ClientMessage::SetSessionPinned {
        request_id: "request-c".into(),
        session_id: "session-a".into(),
        pinned: true,
    }))
    .expect("encode pin");
    let delete = serde_json::to_value(ClientFrame::new(ClientMessage::DeleteSessions {
        request_id: "request-d".into(),
        session_ids: vec!["session-a".into(), "session-b".into()],
    }))
    .expect("encode delete");

    assert_eq!(rename["type"], "rename_session");
    assert_eq!(rename["title"], "Renamed chat");
    assert_eq!(pin["type"], "set_session_pinned");
    assert_eq!(pin["pinned"], true);
    assert_eq!(delete["type"], "delete_sessions");
    assert_eq!(
        delete["session_ids"],
        serde_json::json!(["session-a", "session-b"])
    );
}

#[test]
fn git_diff_query_has_a_correlated_unified_diff_response() {
    let request = serde_json::to_value(ClientFrame::new(ClientMessage::GetGitDiff {
        request_id: "request-diff".into(),
        session_id: "session-a".into(),
        scope: GitDiffScope::Unstaged,
    }))
    .expect("encode Git diff request");
    let response = serde_json::to_value(ServerFrame::new(ServerMessage::GitDiff {
        request_id: "request-diff".into(),
        session_id: "session-a".into(),
        scope: GitDiffScope::Unstaged,
        diff: "diff --git a/file b/file\n".into(),
    }))
    .expect("encode Git diff response");

    assert_eq!(
        (request["type"].as_str(), response["type"].as_str()),
        (Some("get_git_diff"), Some("git_diff"))
    );
    assert_eq!(response["request_id"], "request-diff");
    assert_eq!(response["session_id"], "session-a");
    assert_eq!(response["diff"], "diff --git a/file b/file\n");
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
fn workspace_file_query_carries_its_scope() {
    let request = serde_json::to_value(ClientFrame::new(ClientMessage::ListWorkspaceFiles {
        request_id: "request-files".into(),
        session_id: "session-a".into(),
        scope: WorkspaceFileScope::Modified,
    }))
    .expect("encode workspace file request");
    let response = serde_json::to_value(ServerFrame::new(ServerMessage::WorkspaceFiles {
        request_id: "request-files".into(),
        session_id: "session-a".into(),
        files: vec![WorkspaceFileRecord {
            path: "src/main.rs".into(),
            size: 12,
        }],
        truncated: true,
    }))
    .expect("encode workspace file response");

    assert_eq!(request["scope"], "modified");
    assert_eq!(response["truncated"], true);
}

#[test]
fn workspace_file_write_carries_utf8_content() {
    let request = serde_json::to_value(ClientFrame::new(ClientMessage::WriteWorkspaceFile {
        request_id: "write-file".into(),
        session_id: "session-a".into(),
        path: ".env".into(),
        content: "TOKEN=secret\n".into(),
    }))
    .expect("encode workspace file write");

    assert_eq!(
        request,
        serde_json::json!({
            "version": PROTOCOL_VERSION,
            "type": "write_workspace_file",
            "request_id": "write-file",
            "session_id": "session-a",
            "path": ".env",
            "content": "TOKEN=secret\n"
        })
    );
}

#[test]
fn git_branch_switch_is_an_explicit_session_request() {
    let request = serde_json::to_value(ClientFrame::new(ClientMessage::SwitchGitBranch {
        request_id: "request-branch".into(),
        session_id: "session-a".into(),
        branch: "feature/ui".into(),
    }))
    .expect("encode Git branch switch");

    assert_eq!(
        request,
        serde_json::json!({
            "version": PROTOCOL_VERSION,
            "type": "switch_git_branch",
            "request_id": "request-branch",
            "session_id": "session-a",
            "branch": "feature/ui"
        })
    );
}

#[test]
fn agent_events_identify_their_session() {
    let frame = ServerFrame::new(ServerMessage::AgentEvent {
        session_id: "session-a".into(),
        record: RecordedEvent {
            sequence: 1,
            recorded_at_ms: 1,
            event: Event {
                submission_id: None,
                msg: EventMsg::ContextCompacted,
            },
            stream_metrics: Vec::new(),
            blocks: Vec::new(),
            preview: None,
        },
    });

    let encoded = serde_json::to_value(frame).expect("encode agent event");

    assert_eq!(encoded["session_id"], "session-a");
    assert_eq!(encoded["record"]["sequence"], 1);
}

#[test]
fn agent_events_preserve_every_web_search_query() {
    let frame = ServerFrame::new(ServerMessage::AgentEvent {
        session_id: "session-a".into(),
        record: RecordedEvent {
            sequence: 2,
            recorded_at_ms: 1,
            event: Event {
                submission_id: None,
                msg: EventMsg::WebSearchEnd(mobius::protocol::WebSearchEndEvent {
                    session_id: "session-a".into(),
                    turn_id: "turn-a".into(),
                    model_step_id: "step-a".into(),
                    call_id: "search-a".into(),
                    action: mobius::protocol::WebSearchAction::Search {
                        queries: vec!["möbius framework".into(), "möbius gateway".into()],
                    },
                }),
            },
            stream_metrics: Vec::new(),
            blocks: Vec::new(),
            preview: None,
        },
    });

    let encoded = serde_json::to_value(frame).expect("encode agent event");

    assert_eq!(
        encoded["record"]["event"]["msg"]["action"]["queries"],
        serde_json::json!(["möbius framework", "möbius gateway"])
    );
}

#[test]
fn session_record_exposes_only_frontend_catalog_fields() {
    let record = SessionRecord {
        session_id: "session-a".into(),
        session_context: mobius::protocol::SessionContext {
            bot_id: "bot-a".into(),
            ..mobius::protocol::SessionContext::default()
        },
        parent_session_id: None,
        parent_sequence: None,
        sequence: 3,
        first_user_message: Some("hello".into()),
        execution_stats: mobius::backend::checkpoint::ExecutionStats::default(),
        title: Some("Greeting".into()),
        pinned: true,
        activity: SessionActivity {
            state: SessionActivityState::Running,
            turn_id: Some("turn-a".into()),
            approval_request_id: None,
            started_at: Some(2),
            last_outcome: None,
            message: None,
        },
        created_at: 1,
        updated_at: 2,
    };

    let encoded = serde_json::to_value(record).expect("encode session record");

    assert_eq!(encoded["session_id"], "session-a");
    assert_eq!(encoded["title"], "Greeting");
    assert_eq!(encoded["pinned"], true);
    assert_eq!(encoded["activity"]["state"], "running");
    assert_eq!(encoded["activity"]["turn_id"], "turn-a");
    assert!(encoded.get("catalog_visible").is_none());
}

#[test]
fn session_record_requires_activity() {
    let encoded = serde_json::json!({
        "session_id": "session-a",
        "session_context": {},
        "parent_session_id": null,
        "parent_sequence": null,
        "sequence": 3,
        "first_user_message": null,
        "execution_stats": {
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
            }
        },
        "created_at": 1,
        "updated_at": 2,
        "title": null,
        "pinned": false
    });

    assert!(serde_json::from_value::<SessionRecord>(encoded).is_err());
}

#[test]
fn directory_listing_request_uses_a_gateway_host_path() {
    let frame = ClientFrame::new(ClientMessage::ListDirectories {
        request_id: "request-b".into(),
        path: PathBuf::from("/srv/mobius"),
        include_files: true,
    });

    let encoded = serde_json::to_value(frame).expect("encode directory listing");

    assert_eq!(
        encoded,
        serde_json::json!({
            "version": PROTOCOL_VERSION,
            "type": "list_directories",
            "request_id": "request-b",
            "path": "/srv/mobius",
            "include_files": true
        })
    );
}

#[test]
fn extension_lifecycle_requests_are_gateway_scoped() {
    let frames = [
        ClientMessage::InstallExtension {
            request_id: "install".into(),
            source: "https://github.com/DietrichGebert/ponytail".into(),
            reference: Some("main".into()),
            subdirectory: None,
        },
        ClientMessage::UpdateExtension {
            request_id: "update".into(),
            id: "plugin:ponytail".into(),
        },
        ClientMessage::UninstallExtension {
            request_id: "uninstall".into(),
            id: "plugin:ponytail".into(),
        },
        ClientMessage::TrustExtensionHooks {
            request_id: "trust".into(),
            id: "plugin:ponytail".into(),
            expected_digest: "a".repeat(64),
        },
        ClientMessage::RevokeExtensionHooksTrust {
            request_id: "revoke-trust".into(),
            id: "plugin:ponytail".into(),
            expected_digest: "a".repeat(64),
        },
    ];

    for frame in frames.map(ClientFrame::new) {
        let encoded = serde_json::to_value(frame).expect("encode lifecycle request");
        assert!(encoded.get("session_id").is_none());
    }
}

#[test]
fn gateway_ready_contains_no_selected_session() {
    let session_file_limits = mobius::middleware::session_files::session_file_limits();
    let frame = ServerFrame::new(ServerMessage::Ready {
        payload: ReadyPayload {
            machine_name: "snowwhite.local".into(),
            bots: Vec::new(),
            sessions: Vec::new(),
            background_approvals: Vec::new(),
            swarm_attentions: Vec::new(),
            swarms: Vec::new(),
            providers: Vec::new(),
            provider_instances: Vec::new(),
            bot_defaults: Some(VersionedAgentConfig {
                revision: 1,
                config: AgentComposition::default(),
            }),
            models: Vec::new(),
            model_providers: BTreeMap::new(),
            middleware_features: Vec::new(),
            extensions: Vec::new(),
            contributions: vec![FrontendContribution {
                capability: "extensions".into(),
                references: vec![mobius::protocol::FrontendReference {
                    trigger: '$',
                    value: "review".into(),
                    description: "Review code.".into(),
                }],
                ..FrontendContribution::default()
            }],
            max_active_sessions: 32,
            session_file_limits,
        },
    });

    let encoded = serde_json::to_value(frame).expect("encode gateway ready");
    let encoded_limits: mobius::protocol::SessionFileLimits =
        serde_json::from_value(encoded["payload"]["session_file_limits"].clone())
            .expect("decode session file limits");

    assert_eq!(
        (
            encoded["payload"]["max_active_sessions"].as_u64(),
            encoded["payload"]["machine_name"].as_str(),
            encoded["payload"]["contributions"][0]["references"][0]["value"].as_str(),
            encoded["payload"].get("session"),
            encoded["payload"].get("workspace"),
        ),
        (
            Some(32),
            Some("snowwhite.local"),
            Some("review"),
            None,
            None
        )
    );
    assert_eq!(encoded_limits, session_file_limits);
}

#[test]
fn server_frame_decodes_session_opened_with_a_widget_action_tag() {
    let encoded = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "type": "session_opened",
        "request_id": "request-open",
        "payload": {
            "latest_sequence": 4,
            "next_before_sequence": 2,
            "workspace": { "id": "workspace-a", "path": "/workspace" },
            "git": null,
            "session": {
                "session_id": "session-a",
                "context": {
                    "bot_id": "bot-a"
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
            "tool_count": 3,
            "compaction_count": 2,
            "context_limit_tokens": 250000,
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
            payload.compaction_count,
            payload.context_limit_tokens,
            payload.contributions[0].widgets[0].action.is_some(),
        ),
        ("request-open", "session-a", Some(2), 2, Some(250_000), true)
    );
}

#[test]
fn replay_completion_is_correlated_to_the_open_request() {
    let frame = ServerFrame::new(ServerMessage::SessionReplayComplete {
        request_id: "request-open".into(),
        session_id: "session-a".into(),
    });

    let encoded = serde_json::to_value(frame).expect("encode replay completion");

    assert_eq!(encoded["type"], "session_replay_complete");
    assert_eq!(encoded["request_id"], "request-open");
    assert_eq!(encoded["session_id"], "session-a");
}

#[test]
fn session_history_page_has_a_correlated_cursor() {
    let request = serde_json::to_value(ClientFrame::new(ClientMessage::GetSessionHistory {
        request_id: "request-history".into(),
        session_id: "session-a".into(),
        before_sequence: Some(9),
    }))
    .expect("encode history request");
    let response = serde_json::to_value(ServerFrame::new(ServerMessage::SessionHistory {
        request_id: "request-history".into(),
        session_id: "session-a".into(),
        records: vec![RecordedEvent {
            sequence: 8,
            recorded_at_ms: 1,
            event: Event {
                submission_id: None,
                msg: EventMsg::ContextCompacted,
            },
            stream_metrics: Vec::new(),
            blocks: Vec::new(),
            preview: None,
        }],
        next_before_sequence: Some(4),
    }))
    .expect("encode history response");

    assert_eq!(
        (
            request["type"].as_str(),
            request["before_sequence"].as_u64(),
            response["type"].as_str(),
            response["next_before_sequence"].as_u64(),
            response["records"][0]["event"]["msg"]["type"].as_str(),
        ),
        (
            Some("get_session_history"),
            Some(9),
            Some("session_history"),
            Some(4),
            Some("context_compacted"),
        )
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

#[tokio::test]
async fn write_frame_accepts_payloads_above_the_previous_twenty_mebibyte_limit() {
    let mut writer = tokio::io::sink();
    let payload = "x".repeat(20 * 1024 * 1024 + 1);

    write_frame(&mut writer, &payload)
        .await
        .expect("50 MiB envelope accepts a larger payload");
}

#[test]
fn validate_version_requires_the_exact_protocol() {
    for version in [PROTOCOL_VERSION - 1, PROTOCOL_VERSION + 1] {
        let error = validate_version(version).expect_err("incompatible version must fail");
        assert!(matches!(error, Error::Protocol(_)), "{error}");
    }
}

#[test]
fn scratchpad_messages_carry_their_management_scope() {
    let operation = Op::CapabilityCommand {
        capability: "scratchpad".into(),
        command: "scratchpad".into(),
        arguments: "refresh".into(),
        input: None,
        target: None,
    };
    let request = serde_json::to_value(ClientFrame::new(ClientMessage::SubmitScratchpad {
        request_id: "scratchpad-1".into(),
        scope: ScratchpadScope::Swarm {
            id: "f517e178-38e3-4f2c-89ec-f787860964ea".into(),
        },
        operation,
    }))
    .expect("encode scratchpad request");

    assert_eq!(request["type"], "submit_scratchpad");
    assert_eq!(request["scope"]["type"], "swarm");
    assert!(request.get("session_id").is_none());

    let response = ServerFrame::new(ServerMessage::ScratchpadChanged {
        request_id: "scratchpad-1".into(),
        scope: ScratchpadScope::Global,
        contribution: FrontendContribution {
            capability: "scratchpad".into(),
            ..FrontendContribution::default()
        },
    });
    let decoded: ServerFrame = serde_json::from_value(
        serde_json::to_value(&response).expect("encode global scratchpad response"),
    )
    .expect("decode global scratchpad response");
    assert_eq!(decoded, response);
}

#[test]
fn daily_usage_carries_its_provider_on_the_wire() {
    let usage = DailyUsage {
        unix_day: 42,
        provider: "openai_socket".into(),
        usage: TokenUsage {
            total_tokens: 11,
            ..TokenUsage::default()
        },
    };

    let encoded = serde_json::to_value(&usage).expect("encode daily usage");
    let decoded: DailyUsage = serde_json::from_value(encoded.clone()).expect("decode usage");

    assert_eq!(encoded["provider"], "openai_socket");
    assert_eq!(decoded, usage);
}
