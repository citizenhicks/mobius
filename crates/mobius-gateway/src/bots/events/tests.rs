use super::*;

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
        (6, Some("bot-a".into()))
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
    assert_eq!(
        storage.source_cursor("session-a").expect("cursor"),
        (9, Some("bot-a".into()))
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
        (0, None)
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
        (2, Some("bot-b".into()))
    );
    assert_eq!(
        storage.load_catalog().expect("catalog").as_deref(),
        Some("{\"replayed\":true}")
    );
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
