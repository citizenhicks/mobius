use super::*;
use crate::bots::StoredRoutineBinding;
use crate::wire::{RoutineAction, RoutineBinding, RoutineSchedule, RoutineScheduleKind};

fn fixture() -> (tempfile::TempDir, BotStorage) {
    let root = tempfile::tempdir().expect("root");
    let (storage, _) = BotStorage::open(&root.path().join("bots.sqlite3")).expect("storage");
    (root, storage)
}

fn source() -> HookSource {
    HookSource::Session {
        session_id: "session-a".into(),
    }
}

fn event(id: &str, data: HookData) -> HookEvent {
    HookEvent {
        id: id.into(),
        source: source(),
        cause_id: None,
        ancestry: Vec::new(),
        bot_id: "bot-a".into(),
        occurred_at: 100,
        data,
    }
}

fn started(id: &str) -> HookEvent {
    event(
        id,
        HookData::SessionTurnStarted {
            session_id: "session-a".into(),
            turn_id: id.into(),
        },
    )
}

fn report(id: &str, source: HookSource, kind: HookKind) -> BotSubscription {
    BotSubscription {
        bot_id: "bot-a".into(),
        binding: HookBinding {
            id: id.into(),
            on: event_selector(source, kind),
            action: BotAction::Report {
                instruction: "Tell me the outcome.".into(),
            },
        },
        enabled: true,
    }
}

#[test]
fn facts_and_actions_keep_insertion_order_and_durable_receipts() {
    let (root, storage) = fixture();
    storage
        .set_subscription(
            &report("report", source(), HookKind::SessionTurnStarted),
            0,
            100,
        )
        .expect("binding");
    for id in ["z-first", "a-second"] {
        assert!(storage.record_hook(&started(id)).expect("fact"));
    }
    assert_eq!(
        storage
            .unpublished_events(10)
            .expect("facts")
            .iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        ["z-first", "a-second"]
    );
    let pending = storage.pending_actions(100, 10).expect("actions");
    assert_eq!(
        pending
            .iter()
            .map(|action| action.event.id.as_str())
            .collect::<Vec<_>>(),
        ["z-first", "a-second"]
    );
    let ids = pending
        .iter()
        .map(|action| action.id.clone())
        .collect::<Vec<_>>();
    storage.action_accepted(&ids[0]).expect("receipt");
    storage.event_published("z-first").expect("published");
    storage
        .action_failed(&ids[1], "offline", Some(105))
        .expect("retry");
    drop(storage);
    let (storage, _) = BotStorage::open(&root.path().join("bots.sqlite3")).expect("reopen");
    assert!(!storage.record_hook(&started("z-first")).expect("duplicate"));
    assert!(!storage.action_pending(&ids[0]).expect("accepted"));
    assert!(
        storage
            .pending_actions(104, 10)
            .expect("not due")
            .is_empty()
    );
    assert_eq!(storage.pending_actions(105, 10).expect("due")[0].id, ids[1]);
    assert_eq!(
        storage.unpublished_events(10).expect("unpublished")[0].id,
        "a-second"
    );
    let mut conflict = started("a-second");
    conflict.occurred_at += 1;
    assert!(storage.record_hook(&conflict).is_err());
}

#[test]
fn catalog_mutation_and_non_start_action_acknowledgment_share_one_transaction() {
    let (_root, storage) = fixture();
    storage
        .set_subscription(
            &report("report", source(), HookKind::SessionTurnStarted),
            0,
            0,
        )
        .expect("binding");
    storage.record_hook(&started("input")).expect("input");
    let pending = storage.pending_actions(100, 10).expect("action").remove(0);
    let mut bad = started("bad");
    bad.ancestry = vec!["duplicate".into(); 2];
    assert!(
        storage
            .save_catalog_with_hooks(
                "{\"changed\":true}",
                &[],
                &[bad],
                &[],
                100,
                Some(&pending.id)
            )
            .is_err()
    );
    assert!(
        storage
            .action_pending(&pending.id)
            .expect("ack rolled back")
    );
    assert!(
        storage
            .load_catalog()
            .expect("catalog rolled back")
            .is_none()
    );
    storage
        .save_catalog_with_hooks(
            "{\"changed\":true}",
            &[],
            &[started("committed")],
            &[],
            100,
            Some(&pending.id),
        )
        .expect("catalog, event and ack");
    assert!(!storage.action_pending(&pending.id).expect("accepted"));
    assert!(storage.hook_event("committed").expect("fact").is_some());
}

#[test]
fn projection_atomically_advances_cursor_with_the_fact_and_matching_actions() {
    let (_root, storage) = fixture();
    storage
        .set_subscription(
            &report("report", source(), HookKind::SessionTurnStarted),
            5,
            0,
        )
        .expect("binding");
    assert!(
        storage
            .project_session(&started("before"), 5)
            .expect("fact before binding")
    );
    assert!(storage.pending_actions(100, 10).expect("before").is_empty());
    assert!(
        storage
            .project_session(&started("after"), 6)
            .expect("after binding")
    );
    assert!(
        !storage
            .project_session(&started("replay"), 6)
            .expect("cursor replay")
    );
    assert_eq!(storage.pending_actions(100, 10).expect("actions").len(), 1);
    let mut invalid = started("bad");
    invalid.ancestry = vec!["same".into(), "same".into()];
    assert!(storage.project_session(&invalid, 7).is_err());
    assert_eq!(
        storage
            .source_cursor("session-a")
            .expect("rolled back cursor"),
        6
    );
    assert!(
        storage
            .hook_event("bad")
            .expect("rolled back fact")
            .is_none()
    );
    assert!(
        storage
            .advance_source_cursor("session-a", "bot-b", 7)
            .is_err()
    );
    storage
        .advance_source_cursor("session-a", "bot-a", 9)
        .expect("ignored records advance");
    storage
        .advance_source_cursor("session-a", "bot-a", 8)
        .expect("monotonic");
    assert_eq!(storage.source_cursor("session-a").expect("cursor"), 9);
}

#[test]
fn editing_or_disabling_binding_revokes_pending_authorization_but_keeps_receipts() {
    let (_root, storage) = fixture();
    let mut subscription = report("report", source(), HookKind::SessionTurnStarted);
    storage
        .set_subscription(&subscription, 0, 0)
        .expect("binding");
    storage.record_hook(&started("first")).expect("first");
    storage.record_hook(&started("second")).expect("second");
    let pending = storage.pending_actions(100, 10).expect("actions");
    storage.action_accepted(&pending[0].id).expect("accepted");
    subscription.binding.action = BotAction::Report {
        instruction: "A different authorization.".into(),
    };
    storage
        .set_subscription(&subscription, 0, 100)
        .expect("edit action");
    assert!(!storage.action_pending(&pending[1].id).expect("revoked"));
    assert!(
        !storage
            .record_hook(&started("first"))
            .expect("accepted duplicate")
    );
    storage.record_hook(&started("third")).expect("third");
    let third = storage
        .pending_actions(100, 10)
        .expect("new action")
        .remove(0);
    subscription.binding.on = event_selector(source(), HookKind::SessionDeleted);
    storage
        .set_subscription(&subscription, 0, 100)
        .expect("edit selector");
    assert!(
        !storage
            .action_pending(&third.id)
            .expect("old selector revoked")
    );
    storage
        .record_hook(&event(
            "deleted",
            HookData::SessionDeleted {
                session_id: "session-a".into(),
            },
        ))
        .expect("deleted");
    subscription.enabled = false;
    storage
        .set_subscription(&subscription, 0, 100)
        .expect("disable");
    assert!(
        storage
            .pending_actions(100, 10)
            .expect("disabled")
            .is_empty()
    );
    assert!(
        !storage
            .has_monitored_sessions()
            .expect("inactive monitoring")
    );
}

fn timer(id: &str) -> RoutineBinding {
    HookBinding {
        id: id.into(),
        on: HookSelector::Schedule {
            schedule: RoutineSchedule {
                kind: RoutineScheduleKind::Interval,
                every_seconds: Some(60),
                at: None,
                expression: None,
                time_zone: None,
            },
            ends_at: None,
        },
        action: RoutineAction::Start,
    }
}
fn routine(id: &str, bindings: Vec<RoutineBinding>, enabled: bool) -> StoredRoutine {
    StoredRoutine {
        id: id.into(),
        bot_id: "bot-a".into(),
        workspace: "/srv/project".into(),
        instructions: "/srv/instructions".into(),
        bindings: bindings
            .into_iter()
            .map(|definition| StoredRoutineBinding {
                definition,
                next_due_at: Some(100),
                last_matched_minute: None,
            })
            .collect(),
        enabled,
    }
}

#[test]
fn each_timer_matches_only_its_own_routine_binding_and_paused_routines_match_resume_hooks() {
    let (_root, storage) = fixture();
    let resume = HookBinding {
        id: "resume".into(),
        on: event_selector(source(), HookKind::SessionTurnStarted),
        action: RoutineAction::Resume,
    };
    let routines = [
        routine(
            "routine-a",
            vec![timer("timer-a"), timer("timer-b"), resume],
            false,
        ),
        routine("routine-b", vec![timer("timer-c")], true),
    ];
    storage
        .save_catalog_with_hooks("{}", &routines, &[], &[], 0, None)
        .expect("compiled bindings");
    let due = HookEvent {
        id: "due".into(),
        source: HookSource::Schedule {
            routine_id: "routine-a".into(),
            binding_id: "timer-b".into(),
        },
        data: HookData::ScheduleDue {
            binding_id: "timer-b".into(),
        },
        ..started("due")
    };
    storage.record_hook(&due).expect("due");
    let pending = storage.pending_actions(100, 10).expect("only exact timer");
    assert_eq!(pending.len(), 1);
    assert!(
        matches!(&pending[0].action,BotAction::Routine{command} if command.routine_id=="routine-a"&&command.action==RoutineAction::Start)
    );
    storage.action_accepted(&pending[0].id).expect("ack");
    storage
        .record_hook(&started("resume-cause"))
        .expect("paused routine event");
    let pending = storage.pending_actions(100, 10).expect("resume");
    assert_eq!(pending.len(), 1);
    assert!(
        matches!(&pending[0].action,BotAction::Routine{command} if command.routine_id=="routine-a"&&command.action==RoutineAction::Resume)
    );
}

#[test]
fn closing_session_retains_final_actions_and_replaying_closure_does_not_cancel_them() {
    let (_root, storage) = fixture();
    storage
        .set_subscription(&report("old", source(), HookKind::SessionTurnStarted), 0, 0)
        .expect("old");
    storage
        .set_subscription(
            &report("final", source(), HookKind::SessionOwnerChanged),
            0,
            0,
        )
        .expect("final");
    storage.project_session(&started("work"), 1).expect("work");
    let old = storage
        .pending_actions(100, 10)
        .expect("old action")
        .remove(0);
    let closure = event(
        "owner-change",
        HookData::SessionOwnerChanged {
            session_id: "session-a".into(),
            previous_bot_id: "bot-a".into(),
        },
    );
    storage
        .close_session_sources(std::slice::from_ref(&closure), "{}")
        .expect("close");
    assert!(!storage.action_pending(&old.id).expect("old cancelled"));
    let final_action = storage
        .pending_actions(100, 10)
        .expect("final action")
        .remove(0);
    assert_eq!(final_action.event.id, closure.id);
    assert!(
        storage
            .subscriptions("bot-a")
            .expect("removed bindings")
            .is_empty()
    );
    assert_eq!(
        storage.source_cursor("session-a").expect("removed cursor"),
        0
    );
    storage
        .advance_source_cursor("session-a", "bot-b", 2)
        .expect("new owner cursor");
    storage
        .close_session_sources(std::slice::from_ref(&closure), "{\"replayed\":true}")
        .expect("replay");
    assert!(
        storage
            .action_pending(&final_action.id)
            .expect("final action survives replay")
    );
    assert_eq!(
        storage
            .source_cursor("session-a")
            .expect("new cursor survives replay"),
        2
    );
    assert_eq!(
        storage.load_catalog().expect("catalog").as_deref(),
        Some("{\"replayed\":true}")
    );
}

#[test]
fn causal_feedback_is_suppressed_and_terminal_facts_at_the_bound_remain_publishable() {
    let (_root, storage) = fixture();
    storage
        .set_subscription(
            &report("report", source(), HookKind::SessionTurnStarted),
            0,
            0,
        )
        .expect("binding");
    let first = started("first");
    storage.record_hook(&first).expect("first");
    let next = caused_event(
        "second".into(),
        "bot-a".into(),
        source(),
        started("second").data,
        100,
        Some(&first),
    )
    .expect("caused");
    storage.record_hook(&next).expect("feedback fact");
    assert_eq!(
        storage
            .pending_actions(100, 10)
            .expect("loop suppressed")
            .len(),
        1
    );
    let mut boundary = started("boundary");
    boundary.ancestry = (0..MAX_HOOK_ANCESTRY)
        .map(|i| format!("ancestor-{i}"))
        .collect();
    let terminal = caused_event(
        "terminal".into(),
        "bot-a".into(),
        source(),
        started("terminal").data,
        100,
        Some(&boundary),
    )
    .expect("terminal fact at bound");
    assert_eq!(terminal.ancestry.len(), MAX_HOOK_ANCESTRY);
    assert!(storage.record_hook(&terminal).expect("terminal is stored"));
    assert!(
        storage
            .unpublished_events(10)
            .expect("publishable")
            .iter()
            .any(|event| event.id == "terminal")
    );
    assert_eq!(
        storage
            .pending_actions(100, 10)
            .expect("no new actions at bound")
            .len(),
        1
    );
}

#[test]
fn filters_and_explicit_bot_source_cross_bot_binding_share_one_outbox() {
    let (_root, storage) = fixture();
    let mut subscription = report(
        "filtered",
        HookSource::Routine {
            routine_id: "routine-a".into(),
        },
        HookKind::RunFinished,
    );
    if let HookSelector::Event {
        routine_outcome, ..
    } = &mut subscription.binding.on
    {
        *routine_outcome = Some(RoutineRunStatus::Succeeded);
    }
    storage
        .set_subscription(&subscription, 0, 0)
        .expect("filter");
    for (id, status) in [
        ("failed", RoutineRunStatus::Failed),
        ("succeeded", RoutineRunStatus::Succeeded),
    ] {
        storage
            .record_hook(&HookEvent {
                source: HookSource::Routine {
                    routine_id: "routine-a".into(),
                },
                data: HookData::RunFinished {
                    routine_id: "routine-a".into(),
                    run_id: id.into(),
                    status,
                    session_id: None,
                    reason: None,
                },
                ..started(id)
            })
            .expect("result");
    }
    assert_eq!(storage.pending_actions(100, 10).expect("filtered").len(), 1);
    let mut cross = report(
        "cross",
        HookSource::Bot {
            bot_id: "bot-a".into(),
        },
        HookKind::CustomReceived,
    );
    cross.bot_id = "bot-b".into();
    if let HookSelector::Event { custom_name, .. } = &mut cross.binding.on {
        *custom_name = Some("alert".into());
    }
    storage
        .set_subscription(&cross, 0, 0)
        .expect("explicit bot subscription");
    let mut private = report("private", source(), HookKind::SessionTurnStarted);
    private.bot_id = "bot-b".into();
    storage
        .set_subscription(&private, 0, 0)
        .expect("private selector");
    storage
        .record_hook(&started("private-event"))
        .expect("private event");
    for (id, name) in [("ignored", "other"), ("cross-event", "alert")] {
        storage
            .record_hook(&HookEvent {
                source: HookSource::Bot {
                    bot_id: "bot-a".into(),
                },
                data: HookData::CustomReceived {
                    name: name.into(),
                    data: serde_json::json!({"ok":true}),
                },
                ..started(id)
            })
            .expect("custom");
    }
    let pending = storage.pending_actions(100, 10).expect("shared outbox");
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[1].bot_id, "bot-b");
    assert_eq!(pending[1].event.id, "cross-event");
}

#[test]
fn authenticated_webhook_acceptance_is_durable_idempotent_and_revocable() {
    let (root, storage) = fixture();
    let now = chrono::Utc::now().timestamp();
    let record = storage
        .create_webhook("bot-a", "outage", "Tell me about the outage.", [7; 32])
        .expect("source");
    let delivery = WebhookDelivery {
        source_id: &record.id,
        token_hash: [7; 32],
        delivery_id: "delivery-1",
        body_digest: [3; 32],
        body: "{\"host\":\"server-a\"}",
        timestamp: now,
    };
    let mut unauthorized = delivery;
    unauthorized.token_hash = [9; 32];
    unauthorized.body = "invalid json";
    unauthorized.delivery_id = "";
    assert!(matches!(
        storage.accept_webhook(&unauthorized, now),
        Err(Error::Unauthorized)
    ));
    let mut stale = delivery;
    stale.timestamp = now - 301;
    assert!(matches!(
        storage.accept_webhook(&stale, now),
        Err(Error::Unauthorized)
    ));
    assert!(storage.accept_webhook(&delivery, now).expect("accepted"));
    let pending = storage.pending_actions(now, 10).expect("pending");
    assert_eq!(pending.len(), 1);
    assert!(
        matches!(&pending[0].event.data,HookData::CustomReceived{name,data} if name=="outage"&&data["host"]=="server-a")
    );
    drop(storage);
    let (storage, _) = BotStorage::open(&root.path().join("bots.sqlite3")).expect("reopen");
    assert!(
        !storage
            .accept_webhook(&delivery, now)
            .expect("retry receipt")
    );
    let mut conflict = delivery;
    conflict.body_digest = [4; 32];
    assert!(storage.accept_webhook(&conflict, now).is_err());
    storage
        .configure_webhook("bot-a", &record.id, false, None)
        .expect("disable");
    assert!(
        !storage
            .action_pending(&pending[0].id)
            .expect("pending cancelled")
    );
    assert!(
        storage
            .subscriptions("bot-a")
            .expect("consumers disabled")
            .iter()
            .all(|sub| !sub.enabled)
    );
    assert!(matches!(
        storage.accept_webhook(&delivery, now),
        Err(Error::Unauthorized)
    ));
    storage
        .configure_webhook("bot-a", &record.id, true, Some([8; 32]))
        .expect("rotate and enable");
    assert!(matches!(
        storage.accept_webhook(&delivery, now),
        Err(Error::Unauthorized)
    ));
    let mut rotated = delivery;
    rotated.token_hash = [8; 32];
    rotated.delivery_id = "delivery-2";
    assert!(storage.accept_webhook(&rotated, now).expect("new token"));
    storage.delete_webhook("bot-a", &record.id).expect("delete");
    assert!(
        storage
            .pending_actions(now, 10)
            .expect("source removed")
            .is_empty()
    );
    assert!(matches!(
        storage.accept_webhook(&rotated, now),
        Err(Error::Unauthorized)
    ));
}

fn running() -> RoutineRun {
    RoutineRun {
        id: uuid::Uuid::new_v4().to_string(),
        routine_id: "routine-a".into(),
        bot_id: "bot-a".into(),
        started_at: 100,
        finished_at: None,
        status: RoutineRunStatus::Running,
        session_id: Some(uuid::Uuid::new_v4().to_string()),
        message: None,
    }
}

#[test]
fn hook_start_receipt_survives_run_history_deletion_and_rolls_back_with_reservation() {
    let (root, storage) = fixture();
    let mut subscription = report("start", source(), HookKind::SessionTurnStarted);
    subscription.binding.action = BotAction::Routine {
        command: RoutineCommand {
            routine_id: "routine-a".into(),
            action: RoutineAction::Start,
        },
    };
    storage
        .set_subscription(&subscription, 0, 100)
        .expect("start binding");
    let cause = started("start-cause");
    assert!(storage.record_hook(&cause).expect("cause"));
    let action = storage.pending_actions(100, 10).expect("action").remove(0);
    let run = running();
    storage.connection.lock().expect("connection").execute_batch("CREATE TRIGGER fail_reservation BEFORE INSERT ON hook_events WHEN NEW.id LIKE '%running' BEGIN SELECT RAISE(ABORT, 'test reservation rollback'); END;").expect("failure seam");
    assert!(
        storage
            .insert_run_with_cause(&run, &action.id, Some(&cause))
            .is_err()
    );
    assert!(storage.history(None).expect("no reservation").is_empty());
    assert!(
        storage
            .command_run(&action.id)
            .expect("no receipt")
            .is_none()
    );
    assert!(storage.action_pending(&action.id).expect("retry remains"));
    storage
        .connection
        .lock()
        .expect("connection")
        .execute_batch("DROP TRIGGER fail_reservation;")
        .expect("restore");
    storage
        .insert_run_with_cause(&run, &action.id, Some(&cause))
        .expect("reservation and acceptance");
    assert!(!storage.action_pending(&action.id).expect("accepted"));
    storage
        .finish_run(&run.id, RoutineRunStatus::Succeeded, 101, None)
        .expect("finish");
    storage.delete_run(&run.id).expect("delete history");
    drop(storage);
    let (storage, _) = BotStorage::open(&root.path().join("bots.sqlite3")).expect("reopen");
    assert!(
        storage
            .command_run(&action.id)
            .expect("history removed")
            .is_none()
    );
    assert!(!storage.record_hook(&cause).expect("cause deduplicated"));
    assert!(
        !storage
            .action_pending(&action.id)
            .expect("accepted receipt retained")
    );
    assert!(
        storage
            .pending_actions(102, 10)
            .expect("no replay")
            .is_empty()
    );
}

#[test]
fn run_terminal_state_and_command_receipt_commit_with_hooks_or_roll_back_together() {
    let (_root, storage) = fixture();
    storage
        .set_subscription(
            &report(
                "result",
                HookSource::Routine {
                    routine_id: "routine-a".into(),
                },
                HookKind::RunFinished,
            ),
            0,
            0,
        )
        .expect("binding");
    let run = running();
    let cause = started("start-cause");
    storage
        .insert_run_with_cause(&run, "start-command", Some(&cause))
        .expect("reserve and receipt");
    assert_eq!(
        storage
            .command_run("start-command")
            .expect("receipt")
            .expect("run")
            .id,
        run.id
    );
    assert!(
        storage
            .pending_actions(100, 10)
            .expect("running is not reported")
            .is_empty()
    );
    storage.connection.lock().expect("connection").execute_batch("CREATE TRIGGER fail_terminal BEFORE INSERT ON hook_events WHEN NEW.id LIKE '%succeeded' BEGIN SELECT RAISE(ABORT, 'test atomic rollback'); END;").expect("failure seam");
    assert!(
        storage
            .finish_run(&run.id, RoutineRunStatus::Succeeded, 101, None)
            .is_err()
    );
    assert_eq!(
        storage.history(None).expect("history")[0].status,
        RoutineRunStatus::Running
    );
    storage
        .connection
        .lock()
        .expect("connection")
        .execute_batch("DROP TRIGGER fail_terminal;")
        .expect("restore");
    let stop = started("stop-cause");
    storage
        .request_run_stop(&run.id, "stop-command", Some(&stop))
        .expect("durable stop intent");
    storage
        .request_run_stop(&run.id, "stop-command", Some(&stop))
        .expect("stop retry");
    assert!(
        storage
            .run_cancel_requested(&run.id)
            .expect("cancel intent")
    );
    let finished = storage
        .finish_run(
            &run.id,
            RoutineRunStatus::Cancelled,
            102,
            Some("cancelled".into()),
        )
        .expect("terminal");
    assert_eq!(
        storage
            .finish_run(
                &run.id,
                RoutineRunStatus::Cancelled,
                103,
                Some("cancelled".into())
            )
            .expect("idempotent terminal"),
        finished
    );
    assert!(
        storage
            .finish_run(&run.id, RoutineRunStatus::Failed, 104, None)
            .is_err()
    );
    let pending = storage.pending_actions(102, 10).expect("terminal report");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].event.cause_id.as_deref(), Some("stop-cause"));
    assert!(matches!(
        pending[0].event.data,
        HookData::RunFinished {
            status: RoutineRunStatus::Cancelled,
            ..
        }
    ));
}
