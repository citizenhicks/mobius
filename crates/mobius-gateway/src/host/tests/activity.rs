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
    gateway.shutdown().await;
}
