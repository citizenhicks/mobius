use super::*;
use crate::wire::{RoutineSchedule, RoutineScheduleKind};

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
async fn runtime_idle_shutdown_checks_reservations_revision_and_work_admission() {
    let (root, gateway, bot) = super::bots::gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let host = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let bots = Arc::clone(&gateway.state.lock().await.bots);
    let routine = bots
        .create_routine(
            &bot.id,
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
    let BeginRun::Started(run) = bots.begin_run(&routine.id).unwrap() else {
        panic!("run reserved")
    };
    assert!(
        !gateway.runtime_activity().await.unwrap().idle,
        "reserved work has no session yet"
    );
    bots.finish_run(run, RoutineRunStatus::Succeeded, None)
        .unwrap();
    let activity = gateway.runtime_activity().await.unwrap();
    assert!(
        !gateway
            .prepare_idle_shutdown("stale", || Ok(0))
            .await
            .unwrap()
    );
    assert!(
        !gateway
            .prepare_idle_shutdown(&activity.activity_revision, || Ok(1))
            .await
            .unwrap()
    );
    let in_flight = gateway.begin_mutation().await.unwrap();
    assert!(
        !gateway
            .prepare_idle_shutdown(&activity.activity_revision, || Ok(0))
            .await
            .unwrap()
    );
    drop(in_flight);
    assert!(
        gateway
            .prepare_idle_shutdown(&activity.activity_revision, || Ok(0))
            .await
            .unwrap()
    );
    assert!(
        gateway.begin_mutation().await.is_err(),
        "routine polling and connection admission are closed"
    );
    let rejected = host
        .submit(Submission {
            id: "racing-submit".into(),
            op: Op::Interrupt {
                turn_id: "turn".into(),
            },
        })
        .await;
    assert_eq!(rejected.unwrap_err().code, "gateway_busy");
    assert!(
        gateway.ready().await.is_ok(),
        "dashboard must reconnect to cancel"
    );
    gateway.cancel_idle_shutdown().await;
    assert!(gateway.begin_mutation().await.is_ok());
    let revision = gateway.runtime_activity().await.unwrap().activity_revision;
    gateway.cancel_idle_shutdown().await;
    assert_eq!(
        revision,
        gateway.runtime_activity().await.unwrap().activity_revision
    );
    gateway.shutdown().await;
}
