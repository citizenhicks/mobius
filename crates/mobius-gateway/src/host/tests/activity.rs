use super::*;
use crate::wire::{
    BotAction, BotSubscription, HookBinding, HookKind, HookSource, RoutineSchedule,
    RoutineScheduleKind,
};

#[tokio::test]
async fn runtime_activity_observes_hidden_execution_and_short_completed_work() {
    let (root, gateway, bot) = super::bots::gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let host = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let mut checkpoint = checkpoints.load(host.session_id()).await.unwrap().unwrap();
    checkpoint.catalog_visible = false;
    checkpoint.sequence += 1;
    checkpoint.active_execution = Some(ActiveExecution {
        author: mobius::protocol::MessageAuthor::User,
        submission_id: "hidden-work".into(),
        turn_id: "hidden-turn".into(),
        started_at_ms: 1,
        model_calls: 0,
        tool_calls: 0,
        failed_tool_calls: 0,
        usage: TokenUsage::default(),
        next_model_step: 0,
        stop_hook_active: false,
        phase: mobius::backend::checkpoint::ExecutionPhase::Model,
    });
    checkpoints.save(&checkpoint, &[], None).await.unwrap();
    assert!(!gateway.runtime_activity().await.unwrap().idle);
    checkpoint.active_execution = None;
    checkpoint.sequence += 1;
    checkpoints.save(&checkpoint, &[], None).await.unwrap();
    let before = gateway.runtime_activity().await.unwrap();
    assert!(before.idle);
    let host = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let mut changes = gateway.activity_changes();
    let mut independent_changes = gateway.activity_changes();
    host.submit(Submission {
        id: Uuid::new_v4().to_string(),
        op: Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::User,
                text: "A short local turn".into(),
                attachments: vec![],
                reply: None,
                requested_delivery: None,
                target_turn_id: None,
            },
        },
    })
    .await
    .unwrap();
    host.wait_idle().await;
    assert!(changes.has_changed().unwrap());
    let observed_revision = *changes.borrow_and_update();
    assert!(independent_changes.has_changed().unwrap());
    assert_eq!(observed_revision, *independent_changes.borrow_and_update());
    let after = gateway.runtime_activity().await.unwrap();
    assert!(after.idle);
    assert_ne!(before.activity_revision, after.activity_revision);
    assert_eq!(
        after.activity_revision,
        gateway.runtime_activity().await.unwrap().activity_revision
    );
    gateway.shutdown().await;
}

#[tokio::test]
async fn committed_bot_changes_wake_activity_without_renewing_execution_revision() {
    let (_root, gateway, _bot) = super::bots::gateway_with_bot().await;
    let bots = Arc::clone(&gateway.state.lock().await.bots);
    let mut changes = gateway.activity_changes();
    let revision = *changes.borrow_and_update();
    let before = gateway.runtime_activity().await.unwrap();
    assert!(before.idle);

    bots.create_bot("Another Bot", "A configuration edit.", Default::default())
        .unwrap();

    assert!(changes.has_changed().unwrap());
    assert_eq!(*changes.borrow_and_update(), revision);
    let after = gateway.runtime_activity().await.unwrap();
    assert!(after.idle);
    assert_eq!(before.activity_revision, after.activity_revision);
    gateway.shutdown().await;
}

#[test]
fn activity_revision_wraps_and_retains_marks_without_subscribers() {
    let activity = WorkActivity {
        instance: Uuid::new_v4(),
        revision: watch::Sender::new(u64::MAX),
    };
    activity.mark();
    assert_eq!(*activity.revision.subscribe().borrow(), 0);
}

#[tokio::test]
async fn due_unpolled_routines_hold_activity_but_future_schedules_do_not() {
    let (root, gateway, bot) = super::bots::gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let bots = Arc::clone(&gateway.state.lock().await.bots);
    let now = Utc::now().timestamp();
    for offset in [3600, -1] {
        bots.create_routine(
            &bot.id,
            &timer_definition(
                &workspace,
                "Scheduled work",
                RoutineSchedule {
                    kind: RoutineScheduleKind::Once,
                    at: Some(now + offset),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
                None,
            ),
            None,
        )
        .unwrap();
        assert_eq!(gateway.runtime_activity().await.unwrap().idle, offset > 0);
    }
    bots.poll_due(Utc::now().timestamp()).unwrap();
    assert!(
        !bots
            .next_routine_at(Utc::now().timestamp())
            .unwrap()
            .is_some_and(|at| at.timestamp() <= Utc::now().timestamp())
    );
    assert!(!gateway.runtime_activity().await.unwrap().idle);
    for delivery in bots.pending_actions(Utc::now().timestamp(), 100).unwrap() {
        bots.action_accepted(&delivery.id).unwrap();
    }
    assert!(gateway.runtime_activity().await.unwrap().idle);
    gateway.shutdown().await;
}

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
            bot_id: bot.id.clone(),
            enabled: true,
            binding: HookBinding {
                id: "requested-report".into(),
                on: crate::bots::event_selector(
                    HookSource::Routine {
                        routine_id: routine.id.clone(),
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
                gateway_sandbox: std::sync::Weak::new(),
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

#[tokio::test]
async fn applied_telemetry_publication_keeps_runtime_and_visible_config_aligned() {
    for rollback_fails in [false, true] {
        let (root, gateway, _) = super::bots::gateway_with_bot().await;
        let state_dir = root.path().join("state");
        let config_path = state_dir.join("gateway.toml");
        let database = rusqlite::Connection::open(state_dir.join("bots.sqlite3")).unwrap();
        if rollback_fails {
            database.execute_batch("CREATE TRIGGER reject_telemetry_cursor BEFORE INSERT ON telemetry_cursors BEGIN SELECT RAISE(ABORT, 'test cursor failure'); END;").unwrap();
        }
        let sink: crate::telemetry::TelemetrySink = serde_json::from_value(serde_json::json!({
            "id": "applied", "url": "https://telemetry.example.test/events", "every_seconds": 300,
            "events": ["routine_created"], "enabled": false
        }))
        .unwrap();
        crate::publication::fail_next_directory_sync(&config_path);
        if rollback_fails {
            crate::publication::fail_next_replacement(&config_path);
        }
        let error = gateway
            .configure_telemetry(0, vec![sink], &[])
            .await
            .unwrap_err();
        assert!(error.message.contains("applied") || error.message.contains("remains changed"));
        let (_, reopened) = ConfigStore::open(state_dir).unwrap();
        let state = gateway.state.lock().await;
        assert_eq!(*state.config().unwrap(), reopened);
        assert_eq!(reopened.telemetry.revision, 1);
        assert_eq!(gateway.telemetry.config().unwrap().sinks[0].id, "applied");
        let count: i64 = database
            .query_row(
                "SELECT COUNT(*) FROM telemetry_cursors WHERE sink_id='applied'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, if rollback_fails { 0 } else { 1 });
        if rollback_fails {
            assert!(
                state
                    .bots
                    .telemetry_count(&reopened.telemetry.sinks[0])
                    .is_err()
            );
            assert!(error.message.contains("cursor synchronization failed"));
        }
        drop(state);
        gateway.shutdown().await;
    }
}
