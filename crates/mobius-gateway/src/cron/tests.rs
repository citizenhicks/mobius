use chrono::Utc;

use super::*;

fn store() -> (tempfile::TempDir, CronStore) {
    let root = tempfile::tempdir().expect("temp dir");
    let state = root.path().join("state");
    std::fs::create_dir(&state).expect("state");
    (root, CronStore::open(&state).expect("cron store"))
}

fn cron(expression: &str) -> CronSchedule {
    cron_in(expression, "UTC")
}

fn cron_in(expression: &str, time_zone: &str) -> CronSchedule {
    CronSchedule {
        kind: CronScheduleKind::Cron,
        at: None,
        every_seconds: None,
        expression: Some(expression.into()),
        time_zone: Some(time_zone.into()),
    }
}

fn once(at: i64) -> CronSchedule {
    CronSchedule {
        kind: CronScheduleKind::Once,
        at: Some(at),
        every_seconds: None,
        expression: None,
        time_zone: None,
    }
}

fn interval(every_seconds: u64) -> CronSchedule {
    CronSchedule {
        kind: CronScheduleKind::Interval,
        at: None,
        every_seconds: Some(every_seconds),
        expression: None,
        time_zone: None,
    }
}

fn add_task(store: &CronStore, source: &str, task: &str, schedule: CronSchedule) -> StoredCronTask {
    store
        .add_for_test(source, task, schedule, None)
        .expect("scheduled task")
}

fn take_due(store: &CronStore, now: i64) -> Vec<String> {
    let due = store.take_due(now).expect("due tasks");
    let ids = due.iter().map(|(id, _)| id.clone()).collect();
    for (_, run) in due {
        store
            .finish_run(run, CronRunStatus::Succeeded, None)
            .expect("finish due run");
    }
    ids
}

#[test]
fn global_records_expose_structured_schedule_and_task_contents() {
    let (_root, store) = store();
    let stored = add_task(&store, "session-a", "do work", cron("0 9 * * *"));
    let records = store.records(1_000).expect("frontend records");

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, stored.id);
    assert_eq!(records[0].source_session_id, "session-a");
    assert_eq!(records[0].task, "do work");
    assert_eq!(records[0].schedule, cron("0 9 * * *"));
    assert!(records[0].enabled);
    assert!(!records[0].finished);
}

#[cfg(unix)]
#[test]
fn managed_task_cannot_be_replaced_with_an_outside_symlink() {
    let (root, store) = store();
    let task = add_task(&store, "session-a", "inside", cron("0 9 * * *"));
    let outside = root.path().join("outside.md");
    std::fs::write(&outside, "outside").expect("outside task");
    std::fs::remove_file(&task.task).expect("remove task");
    std::os::unix::fs::symlink(&outside, &task.task).expect("replace with symlink");

    assert!(store.task_input(&task.id).is_err());
}

#[test]
fn missing_task_contents_fail_closed() {
    let (_root, store) = store();
    let task = add_task(&store, "session-a", "inside", cron("0 9 * * *"));
    std::fs::remove_file(&task.task).expect("remove task input");

    assert!(store.records(1_000).is_err());
}

#[test]
fn tasks_and_history_persist_with_global_ownership() {
    let (root, store) = store();
    let task = add_task(&store, "session-a", "do work", cron("0 9 * * MON"));
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
    assert_eq!(reopened.list().expect("tasks"), std::slice::from_ref(&task));
    let runs = reopened.history(None).expect("history");
    assert_eq!(runs.len(), 1);
    assert_eq!(task.session_id, "session-a");
    assert_eq!(runs[0].source_session_id, "session-a");
    assert_eq!(runs[0].session_id.as_deref(), Some("execution-session"));
}

#[test]
fn update_rewrites_task_contents_and_metadata() {
    let (_root, store) = store();
    let task = add_task(&store, "session-a", "old task", cron("0 9 * * *"));
    let run = match store.begin_run(&task.id).expect("begin run") {
        BeginRun::Started(run) => run,
        BeginRun::Skipped => panic!("first run must start"),
    };
    store
        .finish_run(run, CronRunStatus::Succeeded, None)
        .expect("finish run");
    let updated = store
        .reschedule(
            &task.id,
            "session-b",
            "new task",
            interval(60),
            Some(2_000),
            false,
        )
        .expect("update task");

    assert_eq!(
        store.task_input(&task.id).expect("updated input").1,
        "new task"
    );
    assert_eq!(updated.session_id, "session-b");
    assert_eq!(updated.schedule, interval(60));
    assert_eq!(updated.ends_at, Some(2_000));
    assert!(!updated.enabled);
    assert_eq!(
        store.history(Some(&task.id)).expect("history")[0].source_session_id,
        "session-a"
    );
}

#[test]
fn delete_removes_unopenable_run_history() {
    let (_root, store) = store();
    let task = add_task(&store, "session-a", "task", cron("0 9 * * *"));
    let run = match store.begin_run(&task.id).expect("begin run") {
        BeginRun::Started(run) => run,
        BeginRun::Skipped => panic!("first run must start"),
    };
    store
        .finish_run(run, CronRunStatus::Succeeded, None)
        .expect("finish run");

    store.delete(&task.id).expect("delete task");

    assert!(store.history(None).expect("history").is_empty());
}

#[test]
fn delete_session_removes_only_its_global_records() {
    let (_root, store) = store();
    let deleted = add_task(&store, "session-a", "task a", cron("0 9 * * *"));
    let retained = add_task(&store, "session-b", "task b", cron("0 10 * * *"));
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
        .expect("delete source session");
    assert_eq!(store.list().expect("remaining tasks"), [retained]);
    assert!(
        store
            .history(None)
            .expect("remaining history")
            .iter()
            .all(|run| run.source_session_id != "session-a")
    );
    assert!(!deleted.task.exists());
}

#[test]
fn once_and_interval_schedules_advance_without_replay_storms() {
    let (_root, store) = store();
    let now = 1_000;
    let once_task = add_task(&store, "session-a", "once", once(now - 1));
    let interval_task = add_task(&store, "session-b", "interval", interval(60));
    {
        let mut state = store.lock_state().expect("state");
        state
            .tasks
            .iter_mut()
            .find(|task| task.id == interval_task.id)
            .expect("interval task")
            .next_run_at = Some(now - 1);
    }

    let due = take_due(&store, now);
    assert_eq!(due, [once_task.id.clone(), interval_task.id.clone()]);
    assert!(take_due(&store, now).is_empty());
    assert!(store.records(now).expect("records")[0].finished);
    assert_eq!(
        store.task(&interval_task.id).expect("interval").next_run_at,
        Some(1_059)
    );
}

#[test]
fn bounded_interval_runs_its_last_due_occurrence() {
    let (_root, store) = store();
    let task = store
        .add_for_test("session-a", "bounded", interval(60), Some(1_000))
        .expect("bounded task");
    store
        .lock_state()
        .expect("state")
        .tasks
        .iter_mut()
        .find(|stored| stored.id == task.id)
        .expect("stored task")
        .next_run_at = Some(1_000);

    let due = take_due(&store, 1_007);

    assert_eq!(due.len(), 1);
    assert!(store.records(1_007).expect("records")[0].finished);
    assert!(!store.has_active_tasks(1_007).expect("active tasks"));
}

#[test]
fn cron_matching_is_global_and_deduplicated_by_local_minute() {
    let (_root, store) = store();
    let now = Utc::now();
    let expression = format!("{} {} * * *", now.minute(), now.hour());
    let first = add_task(&store, "session-a", "first", cron(&expression));
    let second = add_task(&store, "session-b", "second", cron(&expression));
    let timestamp = now.with_second(7).expect("scheduler phase").timestamp();

    let due = take_due(&store, timestamp);
    assert_eq!(due, [first.id, second.id]);
    assert!(take_due(&store, timestamp).is_empty());
}

#[test]
fn cron_next_occurrence_uses_iana_timezone_across_dst() {
    let (_root, store) = store();
    let task = add_task(
        &store,
        "session-a",
        "dst",
        cron_in("30 1 * * *", "America/New_York"),
    );
    let now = Utc
        .with_ymd_and_hms(2024, 3, 10, 7, 0, 0)
        .single()
        .expect("timestamp")
        .timestamp();
    let expected = Utc
        .with_ymd_and_hms(2024, 3, 11, 5, 30, 0)
        .single()
        .expect("timestamp")
        .timestamp();

    assert_eq!(task.next_run_at(now), Some(expected));
}

#[test]
fn overlap_is_skipped_and_recorded() {
    let (_root, store) = store();
    let task = add_task(&store, "session-a", "do work", cron("* * * * *"));
    let active = match store.begin_run(&task.id).expect("begin run") {
        BeginRun::Started(run) => run,
        BeginRun::Skipped => panic!("first run must start"),
    };
    assert!(matches!(
        store.begin_run(&task.id).expect("overlap"),
        BeginRun::Skipped
    ));
    assert_eq!(
        store.history(Some(&task.id)).expect("history")[0].status,
        CronRunStatus::Skipped
    );
    store
        .finish_run(active, CronRunStatus::Succeeded, None)
        .expect("finish run");
}

#[test]
fn due_overlap_is_skipped_and_recorded() {
    let (_root, store) = store();
    let now = 1_000;
    let task = add_task(&store, "session-a", "do work", once(now - 1));
    let active = match store.begin_run(&task.id).expect("begin run") {
        BeginRun::Started(run) => run,
        BeginRun::Skipped => panic!("first run must start"),
    };

    assert!(store.take_due(now).expect("due tasks").is_empty());
    let history = store.history(Some(&task.id)).expect("history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].status, CronRunStatus::Skipped);
    assert!(history[0].finished_at.is_some());
    assert_eq!(history[1].status, CronRunStatus::Running);

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
            id: Uuid::new_v4().to_string(),
            task_id: "task".into(),
            source_session_id: "source".into(),
            started_at: index as i64,
            finished_at: Some(index as i64),
            status: CronRunStatus::Succeeded,
            session_id: None,
            message: None,
        });
    }
    append_run(
        &mut state,
        CronRun {
            id: Uuid::new_v4().to_string(),
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
    state.tasks.push(StoredCronTask {
        id: Uuid::new_v4().to_string(),
        session_id: "session-a".into(),
        task: root.path().join("outside.md"),
        schedule: cron("0 9 * * *"),
        ends_at: None,
        enabled: true,
        next_run_at: Some(1),
        last_scheduled_minute: None,
    });
    assert!(
        validate_state(&state, &store.tasks_dir)
            .expect_err("outside persisted task must fail")
            .to_string()
            .contains("private gateway task directory")
    );
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
    assert!(validate_schedule(&cron("0 9 * *"), None).is_err());
    assert!(validate_schedule(&cron("75 9 * * *"), None).is_err());
    assert!(validate_schedule(&cron("0 9 * * MON"), None).is_ok());
    assert!(validate_schedule(&interval(59), None).is_err());
    assert!(validate_schedule(&once(1), Some(0)).is_err());
    assert!(validate_schedule(&once(2), Some(1)).is_err());
}
