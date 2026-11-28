use super::*;

#[test]
fn idle_policy_rejects_invalid_thresholds_and_clock_overflow() {
    for (after, minimum) in [
        (Some(Duration::ZERO), 1),
        (Some(Duration::MAX), 1),
        (Some(Duration::from_secs(1)), 0),
        (None, -1),
    ] {
        assert!(
            Compaction::default()
                .idle_compaction(after, minimum)
                .is_err()
        );
    }
    assert!(Compaction::default().idle_compaction(None, 1).is_ok());
}

#[test]
fn checkpoint_target_is_a_share_of_the_window() {
    let policy = Compaction::default();
    assert_eq!(policy.checkpoint_tokens(400_000), 80_000);
    let small = Compaction::from_settings(|id| {
        (id == "checkpoint_percent").then_some(&FrontendSettingValue::Integer(10))
    })
    .expect("valid settings");
    assert_eq!(small.checkpoint_tokens(128_000), 12_800);
}

#[tokio::test]
async fn an_armed_idle_timer_survives_other_subscribers_and_disarms_before_its_attempt() {
    let directory = tempfile::tempdir().expect("directory");
    let runtime = super::super::tests::lifecycle_runtime(directory.path());
    let after = Duration::from_secs(240);
    let policy = Compaction::default()
        .idle_compaction(Some(after), 100)
        .expect("idle compaction");
    policy.counts().expect("counts").insert(
        runtime.session_id.share(),
        SessionCount {
            checkpoints: Arc::clone(&runtime.checkpoints),
            count: 0,
            pending_epoch: None,
            observed: 0,
            idle_armed: false,
        },
    );
    let session = &runtime.session_id;
    for (observed, idle_turn, armed) in [(99, false, false), (100, false, true), (100, true, true)]
    {
        policy.observe(session, observed).expect("observe");
        policy.arm_idle(session, idle_turn).expect("arm");
        assert_eq!(policy.idle_after(session), armed.then_some(after));
        assert_eq!(
            policy.has_background_work(session).await.expect("work"),
            armed
        );
    }
    assert!(policy.disarm_idle(session).unwrap());
    policy.arm_idle(session, true).unwrap();
    assert_eq!(policy.idle_after(session), None);
}

#[tokio::test]
async fn idle_chats_keep_their_count_and_unknown_chats_report_none() {
    let directory = tempfile::tempdir().expect("directory");
    let runtime = super::super::tests::lifecycle_runtime(directory.path());
    runtime
        .checkpoints
        .save_state(
            &runtime.session_id,
            STATE_KEY,
            &serde_json::json!({ "count": 3 }),
        )
        .await
        .expect("saved count");
    let policy = Arc::new(Compaction::default());
    let stack =
        super::super::MiddlewareStack::new(vec![Arc::<Compaction>::clone(&policy)]).expect("stack");
    assert_eq!(policy.frontend("session").count, None);
    for source in [
        super::super::SessionStartSource::Startup,
        super::super::SessionStartSource::Resume,
    ] {
        stack
            .session_start(
                &runtime,
                &[],
                source,
                &mut Vec::new(),
                &mut Default::default(),
            )
            .await
            .expect("session start");
        assert_eq!(policy.frontend("session").count, Some(3));
        stack.session_end(&runtime).await.expect("session end");
        assert_eq!(policy.frontend("session").count, Some(3));
    }
}

#[tokio::test]
async fn count_settlement_retries_storage_failures_without_counting_rollbacks_twice() {
    use crate::backend::checkpoint::Checkpoint;
    use crate::backend::checkpoint::sqlite::SqliteCheckpoint;

    for fail_load in [true, false] {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("checkpoints.sqlite3");
        let checkpoints = Arc::new(SqliteCheckpoint::new(&path).expect("store"));
        let mut checkpoint = Checkpoint::for_test_owner("session", "test-bot");
        checkpoint.context_epoch = 1;
        checkpoints
            .save(checkpoint.clone(), Vec::new(), None)
            .await
            .expect("save");
        let policy = Compaction::default();
        policy.counts().expect("counts").insert(
            checkpoint.session_id.share(),
            SessionCount {
                checkpoints: Arc::<SqliteCheckpoint>::clone(&checkpoints),
                count: 3,
                pending_epoch: None,
                observed: 0,
                idle_armed: false,
            },
        );
        policy.stage(&checkpoint.session_id, 1).expect("stage");
        let database = rusqlite::Connection::open(path).expect("database");
        database
            .execute_batch(if fail_load {
                "UPDATE sessions SET context_epoch = 'corrupt' WHERE session_id = 'session'"
            } else {
                "CREATE TRIGGER fail_count BEFORE INSERT ON middleware_state
                     BEGIN SELECT RAISE(ABORT, 'forced state save failure'); END;"
            })
            .expect("inject failure");

        assert!(
            policy.settle(&checkpoint.session_id).await.is_err(),
            "storage failure must surface as an error from settle"
        );
        {
            let counts = policy.counts().expect("counts");
            assert_eq!(counts["session"].count, 3);
            assert_eq!(counts["session"].pending_epoch, Some(1));
        }
        if fail_load {
            database
                .execute(
                    "UPDATE sessions SET context_epoch = 1 WHERE session_id = 'session'",
                    [],
                )
                .expect("restore context_epoch");
        } else {
            database
                .execute_batch("DROP TRIGGER fail_count")
                .expect("restore writes");
        }
        policy
            .settle(&checkpoint.session_id)
            .await
            .expect("retry settlement");
        policy
            .settle(&checkpoint.session_id)
            .await
            .expect("already settled");
        assert_eq!(policy.frontend("session").count, Some(4));
        assert_eq!(
            checkpoints
                .load_state("session", STATE_KEY)
                .await
                .expect("state"),
            Some(serde_json::json!({ "count": 4 }))
        );
        policy
            .stage(&checkpoint.session_id, 2)
            .expect("stage rolled-back epoch");
        policy
            .settle(&checkpoint.session_id)
            .await
            .expect("discard rollback");
        let counts = policy.counts().expect("counts");
        assert_eq!(counts["session"].count, 4);
        assert_eq!(counts["session"].pending_epoch, None);
    }
}
