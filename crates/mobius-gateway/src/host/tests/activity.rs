use super::*;
use crate::wire::{
    BotAction, BotSubscription, HookBinding, HookKind, HookSource, RoutineSchedule,
    RoutineScheduleKind,
};
use mobius::SessionId;

#[tokio::test]
async fn runtime_activity_tracks_reservations_and_pending_deliveries() {
    let (root, gateway, bot) = super::bots::gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let _host = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let bots = Arc::clone(&gateway.state.lock().await.bots);
    let routine = bots
        .create_routine(
            &bot.id,
            &timer_definition(
                &workspace,
                "Future work",
                RoutineSchedule {
                    kind: RoutineScheduleKind::Once,
                    at: Some(Utc::now().timestamp() + 3600),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
                None,
            ),
            crate::config::CanonicalWorkspace::for_test(&workspace),
            None,
        )
        .unwrap();
    assert!(
        gateway
            .runtime_activity()
            .await
            .unwrap()
            .next_routine_at
            .is_some()
    );
    assert!(
        gateway.runtime_activity().await.unwrap().idle,
        "future schedules are not work"
    );
    gateway
        .set_bot_subscription(BotSubscription {
            bot_id: Arc::clone(&bot.id),
            enabled: true,
            binding: HookBinding {
                id: "requested-report".into(),
                on: crate::bots::event_selector(
                    HookSource::Routine {
                        routine_id: Arc::clone(&routine.id),
                    },
                    HookKind::RunFinished,
                ),
                action: BotAction::Report {
                    instruction: "Tell me when this work finishes.".into(),
                },
            },
        })
        .await
        .unwrap();
    let BeginRun::Started(run) = bots.begin_run(&routine.id).unwrap() else {
        panic!("run reserved")
    };
    assert!(
        !gateway.runtime_activity().await.unwrap().idle,
        "reserved work has no session yet"
    );
    bots.finish_run(run, RoutineRunStatus::Succeeded, None)
        .unwrap();
    assert!(
        !gateway.runtime_activity().await.unwrap().idle,
        "a durable report is pending admission"
    );
    for delivery in bots
        .storage
        .pending_actions(Utc::now().timestamp(), 100)
        .unwrap()
    {
        bots.storage.action_accepted(&delivery.id).unwrap();
    }
    let activity = gateway.runtime_activity().await.unwrap();
    assert!(activity.idle);
    assert!(gateway.begin_mutation().await.is_ok());
    let revision = gateway.runtime_activity().await.unwrap().activity_revision;
    assert_eq!(
        revision,
        gateway.runtime_activity().await.unwrap().activity_revision
    );

    let (handle, mut receiver) = probe_handle("activity-probe", 1, true);
    gateway
        .state
        .lock()
        .await
        .sessions
        .insert(SessionId::new("activity-probe").unwrap(), handle);
    let (activity, ()) = tokio::join!(gateway.runtime_activity(), async {
        let Some(HostCommand::RuntimeIsIdle { reply }) = receiver.recv().await else {
            panic!("activity query reaches the resident actor");
        };
        let BeginRun::Started(run) = bots.begin_run(&routine.id).unwrap() else {
            panic!("run reserved during the idle query");
        };
        bots.finish_run(run, RoutineRunStatus::Succeeded, None)
            .unwrap();
        reply.send(Ok(true)).unwrap();
    });
    let activity = activity.unwrap();
    assert_eq!(activity.activity_revision, revision);
    assert!(
        !activity.idle,
        "a delivery committed during actor inspection is not idle"
    );
    gateway.state.lock().await.sessions.remove("activity-probe");
    gateway.shutdown().await;
}

#[tokio::test]
async fn telemetry_update_rolls_back_only_changed_fields_on_publication_failure() {
    let (root, gateway, _) = super::bots::gateway_with_bot().await;
    let state_dir = root.path().join("state");
    let mut sink: crate::telemetry::TelemetrySink = serde_json::from_value(serde_json::json!({
        "id": "original", "url": "https://telemetry.example.test/events", "every_seconds": 300,
        "events": ["routine_created"], "enabled": false
    }))
    .unwrap();
    gateway
        .configure_telemetry(0, vec![sink.clone()], &[])
        .await
        .unwrap();
    let before = {
        let state = gateway.state.lock().await;
        let config = state.config().unwrap();
        serde_json::to_value(&*config).unwrap()
    };
    let database = rusqlite::Connection::open(state_dir.join("bots.sqlite3")).unwrap();
    let cursor: i64 = database
        .query_row(
            "SELECT after_rowid FROM telemetry_cursors WHERE sink_id='original'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    database.execute_batch("CREATE TRIGGER reject_telemetry_cursor BEFORE INSERT ON telemetry_cursors BEGIN SELECT RAISE(ABORT, 'test cursor failure'); END;").unwrap();
    sink.id = "replacement".into();
    assert!(
        gateway
            .configure_telemetry(1, vec![sink.clone()], &[])
            .await
            .is_err()
    );
    {
        let state = gateway.state.lock().await;
        let config = state.config().unwrap();
        assert_eq!(serde_json::to_value(&*config).unwrap(), before);
    }
    let (_, restored) = ConfigStore::open(state_dir.clone()).unwrap();
    assert_eq!(serde_json::to_value(restored).unwrap(), before);
    assert_eq!(
        database
            .query_row(
                "SELECT after_rowid FROM telemetry_cursors WHERE sink_id='original'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        cursor
    );
    assert_eq!(gateway.telemetry.config().unwrap().sinks[0].id, "original");
    database
        .execute_batch("DROP TRIGGER reject_telemetry_cursor;")
        .unwrap();
    let config_path = state_dir.join("gateway.toml");
    let backup = state_dir.join("gateway.toml.test-backup");
    std::fs::rename(&config_path, &backup).unwrap();
    std::fs::create_dir(&config_path).unwrap();
    assert!(
        gateway
            .configure_telemetry(1, vec![sink], &[])
            .await
            .is_err()
    );
    {
        let state = gateway.state.lock().await;
        let config = state.config().unwrap();
        assert_eq!(serde_json::to_value(&*config).unwrap(), before);
    }
    std::fs::remove_dir(&config_path).unwrap();
    std::fs::rename(&backup, &config_path).unwrap();
    gateway.shutdown().await;
}

#[tokio::test]
async fn telemetry_reports_resumed_session_activity_without_duplicate_network_calls() {
    use crate::telemetry::{Telemetry, TelemetrySink, Trigger};
    use crate::wire::{HookData, HookEvent};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let (root, gateway, bot) = super::bots::gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let session = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let (checkpoints, activities) = {
        let state = gateway.state.lock().await;
        (
            Arc::clone(&state.checkpoints),
            Arc::clone(&state.activities),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sink: TelemetrySink = serde_json::from_value(serde_json::json!({
        "id":"activity", "url":format!("http://{}",listener.local_addr().unwrap()),
        "every_seconds":60,"sections":["activity"],"events":["session_turn_started","session_approval"]
    }))
    .unwrap();
    gateway
        .configure_telemetry(0, vec![sink], &[])
        .await
        .unwrap();
    let receiver = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header = std::str::from_utf8(&request[..end]).unwrap();
                    let length: usize = header
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        bodies.push(
                            serde_json::from_slice::<serde_json::Value>(
                                &request[end + 4..end + 4 + length],
                            )
                            .unwrap(),
                        );
                        break;
                    }
                }
                assert!(request.len() < 8192);
            }
            socket
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        }
        bodies
    });
    for state in [
        SessionActivityState::AwaitingApproval,
        SessionActivityState::Running,
    ] {
        if state == SessionActivityState::AwaitingApproval {
            let bots = Arc::clone(&gateway.state.lock().await.bots);
            bots.storage
                .record_hook(&HookEvent {
                    id: "approval".into(),
                    bot_id: Arc::clone(&bot.id),
                    occurred_at: Utc::now().timestamp(),
                    source: HookSource::Session {
                        session_id: session.session_id().share(),
                    },
                    cause_id: None,
                    ancestry: vec![],
                    data: HookData::SessionApproval {
                        session_id: session.session_id().share(),
                        turn_id: "turn".into(),
                        request_id: "approval".into(),
                    },
                })
                .unwrap();
        }
        let activity = SessionActivity {
            state,
            turn_id: Some("turn".into()),
            approval_request_id: (state == SessionActivityState::AwaitingApproval)
                .then(|| "approval".into()),
            started_at: Some(10),
            ..SessionActivity::default()
        };
        update_session_activity(&checkpoints, &activities, session.session_id(), activity)
            .await
            .unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        Telemetry::tick_at(
            &gateway,
            0,
            Trigger::Events,
            &mut tasks,
            Utc::now().timestamp(),
        )
        .await
        .unwrap();
        while tasks.join_next().await.is_some() {}
        Telemetry::tick_at(
            &gateway,
            0,
            Trigger::Events,
            &mut tasks,
            Utc::now().timestamp(),
        )
        .await
        .unwrap();
        assert!(
            tasks.is_empty(),
            "an unchanged catalog must not send another snapshot"
        );
    }
    let bodies = tokio::time::timeout(std::time::Duration::from_secs(5), receiver)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bodies[0]["events"][0]["id"], "approval");
    for (body, state) in bodies.iter().zip(["awaiting_approval", "running"]) {
        assert_eq!(
            body["reason"],
            if state == "awaiting_approval" {
                "events"
            } else {
                "interval"
            }
        );
        assert_eq!(body["session_activity"]["complete"], true);
        assert_eq!(body["session_activity"]["sessions"][0]["state"], state);
    }
    gateway.shutdown().await;
}
