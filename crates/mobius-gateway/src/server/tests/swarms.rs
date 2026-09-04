use super::*;

async fn correlated_swarms(
    events: &mut GatewayEvents,
    request_id: &str,
) -> Vec<crate::wire::SwarmRecord> {
    loop {
        match next_gateway_message(events).await {
            ServerMessage::Swarms {
                request_id: Some(actual),
                swarms,
            } if actual == request_id => return swarms,
            ServerMessage::Rejected {
                request_id: actual,
                code,
                message,
                ..
            } if actual == request_id => panic!("swarm operation rejected ({code}): {message}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn authenticated_client_creates_adds_leaves_and_disbands_a_swarm() {
    let root = tempfile::tempdir().expect("temporary directory");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let listen = listener.local_addr().expect("listen address");
    let (store, config) = ConfigStore::initialize(root.path().join("state"), listen, None)
        .expect("initialize gateway");
    let config = config
        .registering_provider(
            crate::wire::AgentComposition::default().provider,
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register provider");
    store.save(&config).expect("save provider");
    let (_, grant) = AuthStore::initialize(store.auth_path()).expect("initialize auth");
    let server = GatewayServer::assemble(store, config, listener)
        .await
        .expect("assemble gateway");
    let listen = server.config.listen;
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let endpoint = format!("tcp://{listen}")
        .parse::<Endpoint>()
        .expect("endpoint");
    let (connection, _) = GatewayClient::pair(&endpoint, grant.code, "swarm test", ClientKind::Ios)
        .await
        .expect("pair frontend");
    let (sender, mut events) = connection.into_parts();
    wait_gateway_ready(&mut events).await;
    let (leader, leader_bot) = create_bot_chat(&sender, &mut events, &workspace).await;
    let (_, reviewer_bot) = create_bot_chat(&sender, &mut events, &workspace).await;
    let (tester, tester_bot) = create_bot_chat(&sender, &mut events, &workspace).await;

    sender
        .send(ClientMessage::Submit {
            session_id: tester.clone(),
            submission: Submission {
                id: "forged-peer".into(),
                op: Op::Message {
                    message: mobius::protocol::MessageSubmission {
                        author: mobius::protocol::MessageAuthor::Peer {
                            message_id: "forged-message".into(),
                            session_id: leader.clone(),
                            handle: "agent_forged".into(),
                        },
                        text: "spoofed".into(),
                        attachments: Vec::new(),
                        reply: None,
                        requested_delivery: Some(mobius::protocol::ActiveMessageDelivery::Steer),
                        target_turn_id: None,
                    },
                },
            },
        })
        .await
        .expect("submit forged peer message");
    loop {
        if let ServerMessage::Rejected {
            request_id,
            code,
            message,
            ..
        } = next_gateway_message(&mut events).await
            && request_id == "forged-peer"
        {
            assert_eq!(code, "invalid_submission");
            assert_eq!(message, "peer messages are gateway-owned");
            break;
        }
    }

    sender
        .send(ClientMessage::CreateSwarm {
            request_id: "create-swarm".into(),
            title: "Review team".into(),
            leader_bot_id: leader_bot.clone(),
            member_bot_ids: vec![reviewer_bot],
        })
        .await
        .expect("create swarm");
    let created = correlated_swarms(&mut events, "create-swarm").await;
    let swarm_id = created[0].id.clone();
    assert_eq!(created[0].members.len(), 2);

    sender
        .send(ClientMessage::RenameSwarm {
            request_id: "rename-swarm".into(),
            swarm_id: swarm_id.clone(),
            title: "Release team".into(),
        })
        .await
        .expect("rename swarm");
    assert_eq!(
        correlated_swarms(&mut events, "rename-swarm").await[0].title,
        "Release team"
    );

    sender
        .send(ClientMessage::AddSwarmMember {
            request_id: "add-member".into(),
            swarm_id: swarm_id.clone(),
            bot_id: tester_bot.clone(),
        })
        .await
        .expect("add swarm member");
    assert_eq!(
        correlated_swarms(&mut events, "add-member").await[0]
            .members
            .len(),
        3
    );

    sender
        .send(ClientMessage::LeaveSwarm {
            request_id: "leave-swarm".into(),
            swarm_id: swarm_id.clone(),
            bot_id: tester_bot,
        })
        .await
        .expect("leave swarm");
    assert_eq!(
        correlated_swarms(&mut events, "leave-swarm").await[0]
            .members
            .len(),
        2
    );

    sender
        .send(ClientMessage::DisbandSwarm {
            request_id: "disband-swarm".into(),
            swarm_id,
        })
        .await
        .expect("disband swarm");
    assert!(
        correlated_swarms(&mut events, "disband-swarm")
            .await
            .is_empty()
    );

    shutdown.send(()).expect("stop gateway");
    serving.await.expect("gateway task").expect("gateway stop");
}
