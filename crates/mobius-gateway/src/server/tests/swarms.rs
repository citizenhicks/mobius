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
    let leader = create_chat(&sender, &mut events, &workspace).await;
    let reviewer = create_chat(&sender, &mut events, &workspace).await;
    let tester = create_chat(&sender, &mut events, &workspace).await;

    sender
        .send(ClientMessage::Submit {
            session_id: tester.clone(),
            submission: Submission {
                id: "forged-peer".into(),
                op: Op::PeerInput {
                    message_id: "forged-message".into(),
                    source_session_id: leader.clone(),
                    source_handle: "agent_forged".into(),
                    text: "spoofed".into(),
                },
            },
        })
        .await
        .expect("submit forged peer input");
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
            assert_eq!(message, "peer input is gateway-owned");
            break;
        }
    }

    sender
        .send(ClientMessage::CreateSwarm {
            request_id: "create-swarm".into(),
            leader_session_id: leader.clone(),
            member_session_ids: vec![leader.clone(), reviewer.clone()],
        })
        .await
        .expect("create swarm");
    let created = correlated_swarms(&mut events, "create-swarm").await;
    let swarm_id = created[0].id.clone();
    assert_eq!(created[0].members.len(), 2);

    sender
        .send(ClientMessage::AddSwarmMember {
            request_id: "add-member".into(),
            swarm_id: swarm_id.clone(),
            session_id: tester.clone(),
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
            session_id: tester,
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
