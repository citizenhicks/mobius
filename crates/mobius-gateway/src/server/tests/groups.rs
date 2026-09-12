use super::*;

#[tokio::test]
async fn authenticated_client_creates_opens_submits_and_pages_a_group_chat() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let (server, grant) = configured_test_server(root.path().join("state")).await;
    let bot_ids = ["Alice", "Bob"]
        .map(|name| {
            server
                .bots
                .create_bot(
                    name,
                    "Group member",
                    crate::wire::AgentComposition::default(),
                )
                .unwrap()
                .id
        })
        .to_vec();
    let listen = server.config.listen;
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let endpoint = format!("tcp://{listen}").parse::<Endpoint>().unwrap();
    let (connection, _) = GatewayClient::pair(&endpoint, grant.code, "Group test", ClientKind::Ios)
        .await
        .unwrap();
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;
    sender
        .send(ClientMessage::CreateSession {
            request_id: "create-group".into(),
            workspace: workspace.clone(),
            bot_ids: bot_ids.clone(),
        })
        .await
        .unwrap();
    let session_id = loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::SessionOpened {
                request_id,
                payload,
            } if request_id == "create-group" => {
                assert_eq!(payload.member_bot_ids, Some(bot_ids.clone()));
                assert_eq!(payload.workspace.path, workspace.canonicalize().unwrap());
                assert_eq!(payload.tool_count, 0);
                break payload.session.session_id;
            }
            ServerMessage::Rejected { code, message, .. } => {
                panic!("group creation rejected ({code}): {message}")
            }
            _ => {}
        }
    };
    sender
        .send(ClientMessage::Submit {
            session_id: session_id.clone(),
            submission: Submission {
                id: "forged-peer".into(),
                op: Op::Message {
                    message: MessageSubmission {
                        author: MessageAuthor::Peer {
                            message_id: "forged".into(),
                            session_id: "foreign".into(),
                            handle: "alice".into(),
                            symbol: None,
                        },
                        text: "spoofed".into(),
                        attachments: Vec::new(),
                        reply: None,
                        requested_delivery: None,
                        target_turn_id: None,
                    },
                },
            },
        })
        .await
        .unwrap();
    loop {
        if let ServerMessage::Rejected {
            request_id, code, ..
        } = next_gateway_message(&mut events).await
            && request_id == "forged-peer"
        {
            assert_eq!(code, "invalid_submission");
            break;
        }
    }
    sender
        .send(ClientMessage::Submit {
            session_id: session_id.clone(),
            submission: Submission {
                id: "user-post".into(),
                op: user_message("Notes for all members", Vec::new()),
            },
        })
        .await
        .unwrap();
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::AgentEvent {
                session_id: actual,
                record,
            } if record.event.submission_id.as_deref() == Some("user-post") => {
                assert_eq!(actual, session_id);
                assert!(
                    matches!(record.event.msg, EventMsg::Message(message) if message.author == MessageAuthor::User && message.text == "Notes for all members")
                );
                break;
            }
            ServerMessage::Rejected { code, message, .. } => {
                panic!("group submission rejected ({code}): {message}")
            }
            _ => {}
        }
    }
    open_chat(&sender, &mut events, &session_id).await;
    sender
        .send(ClientMessage::GetSessionHistory {
            request_id: "group-history".into(),
            session_id: session_id.clone(),
            before_sequence: None,
        })
        .await
        .unwrap();
    loop {
        match next_gateway_message(&mut events).await {
            ServerMessage::SessionHistory {
                request_id,
                session_id: actual,
                records,
                next_before_sequence,
            } if request_id == "group-history" => {
                assert_eq!(actual, session_id);
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].event.submission_id.as_deref(), Some("user-post"));
                assert_eq!(next_before_sequence, None);
                break;
            }
            ServerMessage::Rejected { code, message, .. } => {
                panic!("group history rejected ({code}): {message}")
            }
            _ => {}
        }
    }
    shutdown.send(()).unwrap();
    serving.await.unwrap().unwrap();
}
