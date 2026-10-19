use super::*;
use crate::wire::{
    BotAction, BotSubscription, HookBinding, HookKind, HookSource, RoutineSchedule,
    RoutineScheduleKind,
};

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
    for delivery in bots.pending_actions(Utc::now().timestamp(), 100).unwrap() {
        bots.action_accepted(&delivery.id).unwrap();
    }
    let activity = gateway.runtime_activity().await.unwrap();
    assert!(activity.idle);
    assert!(gateway.begin_mutation().await.is_ok());
    let revision = gateway.runtime_activity().await.unwrap().activity_revision;
    assert_eq!(
        revision,
        gateway.runtime_activity().await.unwrap().activity_revision
    );

    let (commands, mut receiver) = mpsc::channel(1);
    let (events, _) = broadcast::channel(1);
    gateway.state.lock().await.sessions.insert(
        "activity-probe".into(),
        HostHandle {
            inner: Arc::new(HostInner {
                session_id: "activity-probe".into(),
                commands,
                events,
                alive: Arc::new(AtomicBool::new(true)),
                terminated: Arc::new(AtomicBool::new(true)),
                termination: Arc::new(tokio::sync::Notify::new()),
                session_mutations: Arc::new(RwLock::new(())),
                realtime_voice: Arc::new(Mutex::new(())),
            }),
        },
    );
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
