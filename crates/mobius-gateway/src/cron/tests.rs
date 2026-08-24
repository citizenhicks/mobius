use chrono::{LocalResult, NaiveDate};

use super::*;

fn store() -> (tempfile::TempDir, CronStore) {
    let root = tempfile::tempdir().expect("temp dir");
    let state = root.path().join("state");
    std::fs::create_dir(&state).expect("state");
    let store = CronStore::open(&state).expect("cron store");
    (root, store)
}

fn add_task(store: &CronStore, source_session_id: &str, task: &str, schedule: &str) -> CronTask {
    store
        .begin_setup(source_session_id, Some(task))
        .expect("begin setup");
    store
        .add_managed(source_session_id, task, schedule)
        .expect("add managed task")
}

#[cfg(unix)]
#[test]
fn managed_task_cannot_be_replaced_with_an_outside_symlink() {
    let (root, store) = store();
    let task = add_task(&store, "session-a", "inside", "0 9 * * *");
    let outside = root.path().join("outside.md");
    std::fs::write(&outside, "outside").expect("outside task");
    std::fs::remove_file(&task.task).expect("remove task");
    std::os::unix::fs::symlink(&outside, &task.task).expect("replace with symlink");

    let error = store
        .task_input(&task.id)
        .expect_err("replacement symlink must fail");

    assert!(error.to_string().contains("private gateway task directory"));
}

#[test]
fn tasks_and_history_persist_with_source_and_owner_only_permissions() {
    let (root, store) = store();
    let task = add_task(&store, "session-a", "do work", "0 9 * * MON");
    assert_eq!(store.task_input(&task.id).expect("read task").1, "do work");
    let run = match store.begin_run(&task.id).expect("begin run") {
        BeginRun::Started(run) => run,
        BeginRun::Skipped => panic!("first run must start"),
    };
    store
        .attach_execution_session(&run, "execution-session")
        .expect("attach execution session");
    store
        .finish_run(run, CronRunStatus::Succeeded, None)
        .expect("finish run");
    drop(store);

    let reopened = CronStore::open(&root.path().join("state")).expect("reopen");
    let runs = reopened.history("session-a", None).expect("source history");

    assert_eq!(
        reopened.list("session-a").expect("source tasks"),
        vec![task.clone()]
    );
    assert!(reopened.list("session-b").expect("other tasks").is_empty());
    assert_eq!(runs.len(), 1);
    assert_eq!(task.session_id, "session-a");
    assert_eq!(runs[0].source_session_id, "session-a");
    assert_eq!(runs[0].session_id.as_deref(), Some("execution-session"));
    #[cfg(unix)]
    {
        assert_eq!(
            std::fs::metadata(root.path().join("state").join(STATE_FILE))
                .expect("state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(root.path().join("state").join(TASKS_DIR))
                .expect("task directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&task.task)
                .expect("task metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn setup_authority_is_concurrent_and_consumed_per_session() {
    let (_root, store) = store();
    store
        .begin_setup("session-a", Some("task a"))
        .expect("begin a");
    store
        .begin_setup("session-b", Some("task b"))
        .expect("begin b");
    store.cancel_setup("unrelated-session");

    let task_a = store
        .add_managed("session-a", "task a", "0 9 * * *")
        .expect("schedule a");
    let task_b = store
        .add_managed("session-b", "task b", "0 10 * * *")
        .expect("schedule b");

    assert!(
        store
            .add_managed("session-a", "second a", "0 11 * * *")
            .is_err(),
        "successful creation must consume only its setup authority"
    );
    assert_eq!(store.list("session-a").expect("tasks a"), vec![task_a]);
    assert_eq!(store.list("session-b").expect("tasks b"), vec![task_b]);
}

#[test]
fn task_operations_are_scoped_to_the_source_session() {
    let (_root, store) = store();
    let task_a = add_task(&store, "session-a", "task a", "0 9 * * *");
    let task_b = add_task(&store, "session-b", "task b", "0 10 * * *");
    let prefix_len = task_a
        .id
        .bytes()
        .zip(task_b.id.bytes())
        .position(|(left, right)| left != right)
        .expect("unique task IDs must differ")
        + 1;
    let foreign_prefix = &task_b.id[..prefix_len];

    assert!(store.task("session-a", foreign_prefix).is_err());
    assert!(
        store
            .reschedule("session-a", foreign_prefix, "0 11 * * *")
            .is_err()
    );
    assert!(store.delete("session-a", foreign_prefix).is_err());
    assert_eq!(
        store
            .reschedule("session-a", &task_a.id, "0 12 * * *")
            .expect("reschedule own task")
            .schedule,
        "0 12 * * *"
    );
    for task in [&task_a, &task_b] {
        let run = match store.begin_run(&task.id).expect("begin run") {
            BeginRun::Started(run) => run,
            BeginRun::Skipped => panic!("run must start"),
        };
        store
            .finish_run(run, CronRunStatus::Succeeded, None)
            .expect("finish run");
    }

    assert_eq!(
        store.history("session-a", None).expect("history a").len(),
        1
    );
    assert_eq!(
        store.history("session-b", None).expect("history b").len(),
        1
    );
    assert!(store.history("session-a", Some(foreign_prefix)).is_err());
    store
        .delete("session-a", &task_a.id)
        .expect("delete own task");
    assert_eq!(
        store
            .history("session-a", Some(&task_a.id))
            .expect("deleted task history")
            .len(),
        1
    );
}

#[test]
fn delete_session_removes_only_its_schedules_files_and_history() {
    let (root, store) = store();
    let deleted = add_task(&store, "session-a", "task a", "0 9 * * *");
    let retained = add_task(&store, "session-b", "task b", "0 10 * * *");
    for task in [&deleted, &retained] {
        let run = match store.begin_run(&task.id).expect("begin run") {
            BeginRun::Started(run) => run,
            BeginRun::Skipped => panic!("run must start"),
        };
        store
            .finish_run(run, CronRunStatus::Succeeded, None)
            .expect("finish run");
    }

    store
        .delete_session("session-a")
        .expect("delete session cron data");

    assert!(store.list("session-a").expect("deleted tasks").is_empty());
    assert!(
        store
            .history("session-a", None)
            .expect("deleted history")
            .is_empty()
    );
    assert!(!deleted.task.exists());
    assert_eq!(store.list("session-b").expect("retained tasks"), [retained]);
    drop(store);
    let reopened = CronStore::open(&root.path().join("state")).expect("reopen");
    assert!(
        reopened
            .list("session-a")
            .expect("reopened tasks")
            .is_empty()
    );
    assert_eq!(
        reopened
            .history("session-b", None)
            .expect("retained history")
            .len(),
        1
    );
}

#[test]
fn delete_session_rejects_a_running_schedule() {
    let (_root, store) = store();
    let task = add_task(&store, "session-a", "task a", "0 9 * * *");
    let run = match store.begin_run(&task.id).expect("begin run") {
        BeginRun::Started(run) => run,
        BeginRun::Skipped => panic!("run must start"),
    };

    assert!(store.delete_session("session-a").is_err());
    assert_eq!(store.list("session-a").expect("retained task"), [task]);

    store
        .finish_run(run, CronRunStatus::Succeeded, None)
        .expect("finish run");
}

#[test]
fn finish_run_unlocks_a_duplicated_file_handle() {
    let (_root, store) = store();
    let task = add_task(&store, "session-a", "task a", "0 9 * * *");
    let run = match store.begin_run(&task.id).expect("begin run") {
        BeginRun::Started(run) => run,
        BeginRun::Skipped => panic!("run must start"),
    };
    let duplicate = run._lock.try_clone().expect("duplicate task lock");

    store
        .finish_run(run, CronRunStatus::Succeeded, None)
        .expect("finish run");
    store
        .delete_session("session-a")
        .expect("completed run must release every task lock");

    drop(duplicate);
}

#[test]
fn delete_session_rejects_an_active_setup() {
    let (_root, store) = store();
    store
        .begin_setup("session-a", Some("task a"))
        .expect("begin setup");

    assert!(store.delete_session("session-a").is_err());

    store.cancel_setup("session-a");
    store
        .delete_session("session-a")
        .expect("delete idle session cron data");
}

#[test]
fn missing_managed_file_does_not_block_schedule_deletion() {
    let (_root, store) = store();
    let task = add_task(&store, "session-a", "do work", "0 9 * * *");
    std::fs::remove_file(&task.task).expect("remove managed file");

    store
        .delete("session-a", &task.id)
        .expect("delete broken schedule");

    assert!(store.list("session-a").expect("list").is_empty());
}

#[test]
fn cancelling_setup_is_scoped_to_its_session() {
    let (_root, store) = store();
    store.begin_setup("session-a", None).expect("begin a");
    store.begin_setup("session-b", None).expect("begin b");
    store.cancel_setup("session-a");

    assert!(
        store
            .add_managed("session-a", "task a", "0 9 * * *")
            .is_err()
    );
    assert!(
        store
            .add_managed("session-b", "task b", "0 10 * * *")
            .is_ok()
    );
}

#[test]
fn ordinary_chat_cannot_create_a_scheduled_task() {
    let (_root, store) = store();
    store
        .begin_setup("setup-chat", None)
        .expect("begin setup in another chat");

    let error = store
        .add_managed("ordinary-chat", "Review open pull requests", "0 9 * * 1")
        .expect_err("setup authority is required");

    assert!(error.to_string().contains("active scheduling setup"));
    assert!(store.list("ordinary-chat").expect("list").is_empty());
}

#[test]
fn due_matching_is_global_across_source_sessions() {
    let (_root, store) = store();
    let task_a = add_task(&store, "session-a", "task a", "30 8 * * 1");
    let task_b = add_task(&store, "session-b", "task b", "30 8 * * 1");
    let local = match Local.from_local_datetime(
        &NaiveDate::from_ymd_opt(2026, 8, 3)
            .expect("date")
            .and_hms_opt(8, 30, 0)
            .expect("time"),
    ) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(first, _) => first,
        LocalResult::None => panic!("local test timestamp must exist"),
    };

    let due = store
        .due_at_minute(local.timestamp().div_euclid(60))
        .expect("due tasks");

    assert_eq!(due, vec![task_a, task_b]);
}

#[test]
fn overlap_is_skipped_and_recorded() {
    let (_root, store) = store();
    let task = add_task(&store, "session-a", "do work", "* * * * *");
    let active = match store.begin_run(&task.id).expect("begin run") {
        BeginRun::Started(run) => run,
        BeginRun::Skipped => panic!("first run must start"),
    };

    let skipped = match store.begin_run(&task.id).expect("overlap result") {
        BeginRun::Skipped => store
            .history("session-a", Some(&task.id))
            .expect("history")
            .into_iter()
            .next()
            .expect("skipped run"),
        BeginRun::Started(_) => panic!("overlap must not start"),
    };

    assert_eq!(skipped.status, CronRunStatus::Skipped);
    assert_eq!(skipped.source_session_id, "session-a");
    store
        .finish_run(active, CronRunStatus::Succeeded, None)
        .expect("finish run");
}

#[test]
fn history_trimming_preserves_running_entries() {
    let running = CronRun {
        id: "running".into(),
        task_id: "task".into(),
        source_session_id: "source".into(),
        started_at: 0,
        finished_at: None,
        status: CronRunStatus::Running,
        session_id: None,
        message: None,
    };
    let mut state = CronState::default();
    state.runs.push(running.clone());
    for index in 1..MAX_RUNS {
        state.runs.push(CronRun {
            id: index.to_string(),
            task_id: "task".into(),
            source_session_id: "source".into(),
            started_at: 0,
            finished_at: Some(0),
            status: CronRunStatus::Succeeded,
            session_id: None,
            message: None,
        });
    }

    append_run(
        &mut state,
        CronRun {
            id: "new".into(),
            task_id: "task".into(),
            source_session_id: "source".into(),
            started_at: 1,
            finished_at: Some(1),
            status: CronRunStatus::Succeeded,
            session_id: None,
            message: None,
        },
    )
    .expect("append run");

    assert_eq!(state.runs.len(), MAX_RUNS);
    assert!(state.runs.contains(&running));
}

#[test]
fn persisted_tasks_must_stay_in_the_private_task_directory() {
    let (root, store) = store();
    let mut state = CronState::default();
    state.tasks.push(CronTask {
        id: Uuid::new_v4().to_string(),
        session_id: "session-a".into(),
        task: root.path().join("outside.md"),
        schedule: "0 9 * * *".into(),
    });

    let error =
        validate_state(&state, &store.tasks_dir).expect_err("outside persisted task must fail");

    assert!(error.to_string().contains("private gateway task directory"));
}

#[test]
fn previous_state_version_is_rejected_without_compatibility() {
    let root = tempfile::tempdir().expect("temp dir");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&state_dir).expect("state");
    let state = CronState {
        version: STATE_VERSION - 1,
        tasks: Vec::new(),
        runs: Vec::new(),
    };
    std::fs::write(
        state_dir.join(STATE_FILE),
        serde_json::to_vec(&state).expect("encode old state"),
    )
    .expect("write old state");

    let error = match CronStore::open(&state_dir) {
        Ok(_) => panic!("old state must fail"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("unsupported cron state version"));
}

#[test]
fn malformed_or_out_of_range_schedule_is_rejected() {
    assert!(validate_schedule("0 9 * *").is_err());
    assert!(validate_schedule("75 9 * * *").is_err());
    assert!(validate_schedule("0 9 * * MON").is_ok());
}
