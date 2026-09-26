use super::*;

async fn activity(sender: &GatewaySender, events: &mut GatewayEvents) -> (bool, usize, String) {
    sender
        .send(ClientMessage::GetRuntimeActivity {
            request_id: "activity".into(),
        })
        .await
        .unwrap();
    loop {
        if let ServerMessage::RuntimeActivity {
            idle,
            connected_clients,
            activity_revision,
            ..
        } = next_gateway_message(events).await
        {
            return (idle, connected_clients, activity_revision);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn runtime_start_quiesced_holds_due_routines_before_first_poll() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let (mut server, _) = configured_test_server(root.path().join("state")).await;
    let bots = Arc::clone(&server.bots);
    let bot = bots
        .create_bot("staged", "Staged", crate::wire::AgentComposition::default())
        .unwrap();
    let routine = bots
        .create_routine(
            &bot.id,
            &workspace,
            "due work",
            crate::wire::RoutineSchedule {
                kind: crate::wire::RoutineScheduleKind::Once,
                at: Some(Utc::now().timestamp() - 1),
                every_seconds: None,
                expression: None,
                time_zone: None,
            },
            None,
        )
        .unwrap();
    server.start_quiesced().await.unwrap();
    let host = server.host.clone();
    let ready = server.notify_ready();
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    ready.await.unwrap();
    tokio::time::advance(ROUTINE_TICK).await;
    tokio::task::yield_now().await;
    assert!(bots.routine(&routine.id).unwrap().next_run_at.is_some());
    assert!(!bots.has_running_routines().unwrap());
    assert!(
        host.ready().await.is_ok(),
        "dashboard ready stays available"
    );
    host.cancel_idle_shutdown().await;
    tokio::time::advance(ROUTINE_TICK).await;
    tokio::task::yield_now().await;
    assert!(bots.routine(&routine.id).unwrap().next_run_at.is_none());
    shutdown.send(()).unwrap();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn runtime_shutdown_excludes_dashboard_observers_and_gates_new_native_connections() {
    let root = tempfile::tempdir().unwrap();
    let (server, grant) =
        GatewayServer::bootstrap(root.path().join("state"), "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
    let endpoint = format!("tcp://{}", server.config.listen)
        .parse::<Endpoint>()
        .unwrap();
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async move {
        let _ = signal.await;
    }));
    let (dashboard, identity) = GatewayClient::pair(
        &endpoint,
        grant.code,
        "controller",
        ClientKind::GatewayDashboard,
    )
    .await
    .unwrap();
    let (sender, mut events) = dashboard.into_parts();
    wait_gateway_ready(&mut events).await;
    let before = activity(&sender, &mut events).await;
    assert!(before.0);
    assert_eq!(before.1, 0);
    assert_eq!(before, activity(&sender, &mut events).await);
    let native = GatewayClient::connect(&endpoint, identity.token.clone(), ClientKind::Cli)
        .await
        .unwrap();
    let during = activity(&sender, &mut events).await;
    assert_eq!(during.1, 1);
    assert_ne!(before.2, during.2);
    drop(native);
    let disconnected = loop {
        let value = activity(&sender, &mut events).await;
        if value.1 == 0 {
            break value;
        }
        tokio::task::yield_now().await;
    };
    assert_ne!(during.2, disconnected.2);
    sender
        .send(ClientMessage::PrepareIdleShutdown {
            request_id: "prepare".into(),
            expected_activity_revision: disconnected.2.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(
        next_gateway_message(&mut events).await,
        ServerMessage::IdleShutdownPrepared { prepared: true, .. }
    ));
    assert!(
        GatewayClient::connect(&endpoint, identity.token.clone(), ClientKind::Cli)
            .await
            .is_err()
    );
    let dashboard = GatewayClient::connect(&endpoint, identity.token, ClientKind::GatewayDashboard)
        .await
        .unwrap();
    let (controller, mut control_events) = dashboard.into_parts();
    wait_gateway_ready(&mut control_events).await;
    assert_eq!(
        activity(&controller, &mut control_events).await.2,
        disconnected.2
    );
    controller
        .send(ClientMessage::CancelIdleShutdown {
            request_id: "cancel".into(),
        })
        .await
        .unwrap();
    assert!(matches!(
        next_gateway_message(&mut control_events).await,
        ServerMessage::Accepted { .. }
    ));
    assert_eq!(
        activity(&controller, &mut control_events).await.2,
        disconnected.2
    );
    shutdown.send(()).unwrap();
    serving.await.unwrap().unwrap();
}
