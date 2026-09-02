use super::*;

fn gateway(root: &tempfile::TempDir) -> GatewayHost {
    let listen = "127.0.0.1:8741".parse().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(root.path().join("state"), listen, None).expect("config");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    GatewayHost::start(store, config, credentials, bots).expect("gateway")
}

#[tokio::test]
async fn gateway_manages_bot_swarms_and_broadcasts_the_catalog() {
    let root = tempfile::tempdir().expect("root");
    let gateway = gateway(&root);
    let leader = ensure_test_bot(&gateway).await.expect("leader Bot");
    let (reviewer, tester) = {
        let state = gateway.state.lock().await;
        let config = leader.config.config.clone();
        (
            state
                .bots
                .create_bot("reviewer", "Reviewer", config.clone())
                .expect("reviewer Bot"),
            state
                .bots
                .create_bot("tester", "Tester", config)
                .expect("tester Bot"),
        )
    };
    let mut events = gateway.subscribe();

    let created = gateway
        .create_swarm(
            "Review team".into(),
            leader.id.clone(),
            vec![reviewer.id.clone()],
        )
        .await
        .expect("create swarm");
    let swarm_id = created[0].id.clone();
    assert_eq!(created[0].leader_bot_id, leader.id);
    assert_eq!(created[0].members.len(), 2);
    assert!(matches!(
        events.recv().await.expect("swarm broadcast").message,
        ServerMessage::Swarms {
            request_id: None,
            ..
        }
    ));

    let scratchpad = gateway
        .submit_scratchpad(
            &crate::wire::ScratchpadScope::Swarm {
                id: swarm_id.clone(),
            },
            Op::CapabilityCommand {
                capability: "scratchpad".into(),
                command: "scratchpad".into(),
                arguments: "add".into(),
                input: Some("Release target is Friday".into()),
                target: None,
            },
        )
        .await
        .expect("add swarm scratchpad note");
    assert!(matches!(
        &scratchpad.widgets[0].content,
        Some(mobius::protocol::FrontendWidgetContent::ActionList { items, .. })
            if items[0].text == "Release target is Friday"
    ));

    let renamed = gateway
        .rename_swarm(&swarm_id, "Release team".into())
        .await
        .expect("rename swarm");
    assert_eq!(renamed[0].title, "Release team");

    let joined = gateway
        .add_swarm_member(&swarm_id, tester.id.clone())
        .await
        .expect("add member");
    assert_eq!(joined[0].members.len(), 3);

    let left = gateway
        .leave_swarm(&swarm_id, &tester.id)
        .await
        .expect("leave swarm");
    assert_eq!(left[0].members.len(), 2);

    assert!(
        gateway
            .disband_swarm(&swarm_id)
            .await
            .expect("disband")
            .is_empty()
    );
    let scratchpad = gateway.state.lock().await.scratchpad.clone();
    let cleared = scratchpad
        .swarm_contribution(&swarm_id)
        .await
        .expect("cleared swarm scratchpad");
    assert!(matches!(
        &cleared.widgets[0].content,
        Some(mobius::protocol::FrontendWidgetContent::ActionList { items, .. }) if items.is_empty()
    ));
}

#[tokio::test]
async fn gateway_rejects_unknown_or_already_grouped_bots() {
    let root = tempfile::tempdir().expect("root");
    let gateway = gateway(&root);
    let leader = ensure_test_bot(&gateway).await.expect("leader Bot");
    let reviewer = {
        let state = gateway.state.lock().await;
        state
            .bots
            .create_bot("reviewer", "Reviewer", leader.config.config.clone())
            .expect("reviewer Bot")
    };
    gateway
        .create_swarm("First".into(), leader.id.clone(), vec![reviewer.id.clone()])
        .await
        .expect("first swarm");

    let unknown = gateway
        .create_swarm(
            "Unknown".into(),
            leader.id.clone(),
            vec![Uuid::new_v4().to_string()],
        )
        .await
        .expect_err("unknown Bot");
    assert_eq!(unknown.code, "invalid_bot");

    let grouped = gateway
        .create_swarm("Second".into(), leader.id.clone(), vec![reviewer.id])
        .await
        .expect_err("Bots may belong to one swarm");
    assert_eq!(grouped.code, "invalid_swarm");
}

#[tokio::test]
async fn gateway_leave_releases_host_state_while_waiting_for_delivery_acceptance() {
    let root = tempfile::tempdir().expect("root");
    let gateway = gateway(&root);
    let leader = ensure_test_bot(&gateway).await.expect("leader Bot");
    let reviewer = {
        let state = gateway.state.lock().await;
        state
            .bots
            .create_bot("reviewer", "Reviewer", leader.config.config.clone())
            .expect("reviewer Bot")
    };
    let swarm_id = gateway
        .create_swarm(
            "Review team".into(),
            leader.id.clone(),
            vec![reviewer.id.clone()],
        )
        .await
        .expect("create swarm")[0]
        .id
        .clone();
    let swarm = Arc::clone(&gateway.state.lock().await.swarm);
    swarm
        .post(
            &leader.id,
            &Uuid::new_v4().to_string(),
            format!("@{} please review", reviewer.handle),
            None,
        )
        .await
        .expect("post");
    let claim = swarm
        .claim_next_delivery(&reviewer.id)
        .await
        .expect("claim delivery")
        .expect("pending delivery");
    let leaving = gateway.leave_swarm(&swarm_id, &reviewer.id);
    tokio::pin!(leaving);
    tokio::select! {
        biased;
        result = &mut leaving => panic!("leave settled before queue acceptance: {result:?}"),
        () = std::future::ready(()) => {}
    }
    tokio::select! {
        biased;
        state = gateway.state.lock() => drop(state),
        () = std::future::ready(()) => {
            panic!("leave held gateway state while waiting for delivery acceptance");
        }
    }

    claim
        .accept(std::future::ready(()))
        .await
        .expect("accept delivery")
        .expect("eligible delivery");
    leaving.await.expect("leave after acceptance");
}
