use std::sync::Arc;

use super::*;

fn unseeded_fixture() -> (tempfile::TempDir, BotStore, PathBuf) {
    let root = tempfile::tempdir().expect("root");
    let state = root.path().join("state");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&state).expect("state");
    std::fs::create_dir(&workspace).expect("workspace");
    let store = BotStore::open(&state).expect("Bot store");
    (root, store, workspace)
}

fn fixture() -> (tempfile::TempDir, BotStore, PathBuf) {
    let fixture = unseeded_fixture();
    fixture
        .1
        .seed_default(&VersionedAgentConfig {
            revision: 1,
            config: AgentComposition::default(),
        })
        .expect("seed default Bot")
        .expect("fresh default Bot");
    fixture
}

fn create_bot(store: &BotStore, handle: &str) -> BotRecord {
    store
        .create_bot(handle, "Own test work.", AgentComposition::default())
        .expect("Bot")
}

fn once(at: i64) -> RoutineSchedule {
    RoutineSchedule {
        kind: RoutineScheduleKind::Once,
        at: Some(at),
        every_seconds: None,
        expression: None,
        time_zone: None,
    }
}

fn cron(expression: &str, time_zone: &str) -> RoutineSchedule {
    RoutineSchedule {
        kind: RoutineScheduleKind::Cron,
        at: None,
        every_seconds: None,
        expression: Some(expression.into()),
        time_zone: Some(time_zone.into()),
    }
}

fn interval(every_seconds: u64) -> RoutineSchedule {
    RoutineSchedule {
        kind: RoutineScheduleKind::Interval,
        at: None,
        every_seconds: Some(every_seconds),
        expression: None,
        time_zone: None,
    }
}

fn finish_due(store: &BotStore, now: i64) -> Vec<String> {
    let due = store.take_due(now).expect("due routines");
    let ids = due.iter().map(|(id, _)| id.clone()).collect();
    for (_, run) in due {
        store
            .finish_run(run, RoutineRunStatus::Succeeded, None)
            .expect("finish due run");
    }
    ids
}

#[test]
fn fresh_state_seeds_one_ordinary_immutable_mobius_bot() {
    let (root, store, _) = unseeded_fixture();
    assert!(!store.path.exists());
    let defaults = VersionedAgentConfig {
        revision: 7,
        config: AgentComposition::default(),
    };

    let bot = store
        .seed_default(&defaults)
        .expect("seed default")
        .expect("fresh seed");

    assert_eq!(
        (
            bot.handle.as_str(),
            bot.name.as_str(),
            bot.description.as_str(),
            bot.tint,
            bot.config.revision,
        ),
        (
            "mobius",
            "Mobius",
            MOBIUS_DESCRIPTION,
            ProviderTint::Blue,
            1,
        )
    );
    assert_eq!(bot.config.config, defaults.config);
    assert!(
        store
            .prepare_bot_deletion(&bot.id, bot.config.revision)
            .expect_err("built-in Bot is immutable")
            .to_string()
            .contains("cannot be deleted")
    );
    let reopened = BotStore::open(&root.path().join("state")).expect("reopen Bots");
    assert!(
        reopened
            .seed_default(&defaults)
            .expect("repeat seed")
            .is_none()
    );
    assert_eq!(reopened.bots().expect("Bots"), [bot]);
}

#[test]
fn bot_deletion_removes_owned_routines_history_and_scripts() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "retired");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "retire owned state",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");
    let BeginRun::Started(run) = store.begin_run(&routine.id).expect("begin run") else {
        panic!("routine must start");
    };
    store
        .finish_run(run, RoutineRunStatus::Succeeded, None)
        .expect("finish run");

    let deletion = store
        .prepare_bot_deletion(&bot.id, bot.config.revision)
        .expect("prepare Bot deletion");
    store.delete_bot(deletion).expect("delete Bot");

    assert!(store.bot(&bot.id).is_err());
    assert!(store.routine(&routine.id).is_err());
    assert!(store.history(None).expect("history").is_empty());
    assert!(!routine.instructions.exists());
}

#[test]
fn bot_deletion_refuses_to_orphan_instruction_files() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "blocked_cleanup");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "keep owned state",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");
    std::fs::remove_file(&routine.instructions).expect("remove instruction file");
    std::fs::create_dir(&routine.instructions).expect("replace file with directory");
    let error = store
        .prepare_bot_deletion(&bot.id, bot.config.revision)
        .expect_err("invalid instructions must fail preflight");

    assert!(error.to_string().contains("instructions must remain"));
    assert_eq!(store.bot(&bot.id).expect("Bot remains"), bot);
    assert_eq!(
        store.routine(&routine.id).expect("routine remains"),
        routine
    );
}

#[test]
fn bot_deletion_rejects_a_running_routine_before_mutation() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "busy");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "stay active",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");
    let BeginRun::Started(run) = store.begin_run(&routine.id).expect("begin run") else {
        panic!("routine must start");
    };

    let error = store
        .prepare_bot_deletion(&bot.id, bot.config.revision)
        .expect_err("running routine must reject deletion");

    assert!(error.to_string().contains("currently running"));
    assert_eq!(store.bot(&bot.id).expect("Bot remains"), bot);
    assert_eq!(
        store.routine(&routine.id).expect("routine remains"),
        routine
    );
    store
        .finish_run(run, RoutineRunStatus::Failed, Some("test cleanup".into()))
        .expect("finish run");
}

#[tokio::test]
async fn routine_creation_cannot_commit_after_its_bot_is_deleted() {
    let (_root, store, workspace) = fixture();
    let store = Arc::new(store);
    let bot = create_bot(&store, "retiring");
    let deletion = store
        .prepare_bot_deletion(&bot.id, bot.config.revision)
        .expect("prepare Bot deletion");
    let creating = tokio::task::spawn_blocking({
        let store = Arc::clone(&store);
        let bot_id = bot.id.clone();
        move || {
            store.create_routine(
                &bot_id,
                &workspace,
                "must not outlive its Bot",
                once(Utc::now().timestamp() + 60),
                None,
            )
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if std::fs::read_dir(&store.routines_dir)
                .expect("routine directory")
                .any(|entry| entry.expect("routine entry").path().is_file())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("routine creation reaches the serialized state update");
    assert!(!creating.is_finished());

    store.delete_bot(deletion).expect("delete Bot");
    let error = creating
        .await
        .expect("routine task")
        .expect_err("deleted Bot cannot gain a routine");

    assert!(error.to_string().contains("unknown Bot"));
    assert!(store.history(None).expect("history").is_empty());
    assert!(
        std::fs::read_dir(&store.routines_dir)
            .expect("routine directory")
            .next()
            .is_none()
    );
}

#[test]
fn bot_profile_update_preserves_identity_and_persists_exact_revision() {
    let (root, store, _) = fixture();
    let created = create_bot(&store, "reviewer");
    let mut config = created.config.config.clone();
    config.system_prompt = "Review carefully".into();

    let updated = store
        .update_bot(
            &created.id,
            1,
            "Code reviewer",
            "Review code carefully.",
            ProviderTint::Purple,
            config,
        )
        .expect("update Bot");

    assert_eq!(updated.id, created.id);
    assert_eq!(updated.handle, "reviewer");
    assert_eq!(updated.name, "Code reviewer");
    assert_eq!(updated.config.revision, 2);
    let reopened = BotStore::open(&root.path().join("state")).expect("reopen");
    assert_eq!(reopened.bot(&created.id).expect("stored Bot"), updated);
}

#[test]
fn bot_handles_are_derived_unique_and_immutable() {
    let (_root, store, _) = fixture();
    let created = create_bot(&store, "builder");
    let duplicate = store
        .create_bot("builder", "Own other work.", AgentComposition::default())
        .expect("second Bot");
    let reserved = store
        .create_bot("User", "Human-facing work.", AgentComposition::default())
        .expect("reserved handle is suffixed");
    assert_eq!(duplicate.handle, "builder-2");
    assert_eq!(reserved.handle, "user-2");

    let updated = store
        .update_bot(
            &created.id,
            created.config.revision,
            "Renamed",
            "Own renamed work.",
            ProviderTint::Teal,
            created.config.config,
        )
        .expect("rename Bot");
    assert_eq!(updated.handle, "builder");
}

#[test]
fn running_routine_reserves_its_fresh_session_before_execution() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "operator");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "prepare report",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");

    let BeginRun::Started(active) = store.begin_run(&routine.id).expect("begin") else {
        panic!("first run must start");
    };
    let reserved = active.session_id().to_owned();
    let running = store.history(Some(&routine.id)).expect("history");
    assert_eq!(running[0].status, RoutineRunStatus::Running);
    assert_eq!(running[0].session_id.as_deref(), Some(reserved.as_str()));
    assert!(matches!(
        store.begin_run(&routine.id).expect("overlap"),
        BeginRun::Skipped
    ));
    store
        .finish_run(active, RoutineRunStatus::Succeeded, None)
        .expect("finish");
}

#[test]
fn active_routine_run_cannot_be_deleted() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "operator");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "prepare report",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");
    let BeginRun::Started(active) = store.begin_run(&routine.id).expect("begin") else {
        panic!("run must start");
    };
    let run = store.history(Some(&routine.id)).expect("history")[0].clone();

    let error = store
        .delete_run(&run.id)
        .expect_err("active run must remain");

    assert!(error.to_string().contains("currently running"));
    store
        .finish_run(active, RoutineRunStatus::Succeeded, None)
        .expect("finish");
}

#[test]
fn unrelated_run_finishes_while_bot_deletion_recovery_is_pending() {
    let (_root, store, workspace) = fixture();
    let deleting = create_bot(&store, "deleting");
    let worker = create_bot(&store, "worker");
    let routine = store
        .create_routine(
            &worker.id,
            &workspace,
            "finish existing work",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");
    let BeginRun::Started(active) = store.begin_run(&routine.id).expect("begin") else {
        panic!("run must start");
    };
    let mut deletion = store
        .prepare_bot_deletion(&deleting.id, deleting.config.revision)
        .expect("prepare deletion");
    store
        .record_bot_deletion(&mut deletion, &[], &[], None)
        .expect("record recovery intent");
    drop(deletion);

    let run = store
        .finish_run(active, RoutineRunStatus::Succeeded, None)
        .expect("finish unrelated run");

    assert_eq!(run.status, RoutineRunStatus::Succeeded);
    assert_eq!(
        store
            .pending_bot_deletion()
            .expect("pending deletion")
            .map(|pending| pending.bot_id),
        Some(deleting.id)
    );
}

#[test]
fn due_routines_idle_while_bot_deletion_recovery_is_pending() {
    let (_root, store, workspace) = fixture();
    let deleting = create_bot(&store, "deleting");
    let worker = create_bot(&store, "worker");
    let now = Utc::now().timestamp();
    store
        .create_routine(&worker.id, &workspace, "wait for recovery", once(now), None)
        .expect("routine");
    let mut deletion = store
        .prepare_bot_deletion(&deleting.id, deleting.config.revision)
        .expect("prepare deletion");
    store
        .record_bot_deletion(&mut deletion, &[], &[], None)
        .expect("record recovery intent");
    drop(deletion);

    assert!(
        store
            .take_due(now)
            .expect("scheduler remains idle")
            .is_empty()
    );
}

#[test]
fn routine_start_reloads_an_update_that_wins_before_its_lock() {
    let (_root, store, workspace) = fixture();
    let original_bot = create_bot(&store, "original");
    let updated_bot = create_bot(&store, "updated");
    let routine = store
        .create_routine(
            &original_bot.id,
            &workspace,
            "original instructions",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");

    let BeginRun::Started(active) = store
        .begin_run_inner(&routine.id, || {
            store
                .update_routine(
                    &routine.id,
                    &updated_bot.id,
                    &workspace,
                    "updated instructions",
                    routine.schedule.clone(),
                    None,
                    true,
                )
                .expect("interleaved update");
        })
        .expect("begin updated routine")
    else {
        panic!("updated routine must start");
    };

    let run = store.run(&active.run_id).expect("running invocation");
    assert_eq!(run.bot_id, updated_bot.id);
    store
        .finish_run(active, RoutineRunStatus::Succeeded, None)
        .expect("finish invocation");
}

#[test]
fn routine_start_rejects_a_delete_that_wins_before_its_lock() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "deleted");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "delete before start",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");

    let error = store
        .begin_run_inner(&routine.id, || {
            let deletion = store
                .prepare_routine_deletion(&routine.id)
                .expect("prepare interleaved delete");
            store.delete_routine(deletion).expect("interleaved delete");
        })
        .err()
        .expect("deleted routine must not start");

    assert!(error.to_string().contains("unknown routine"));
    assert!(store.history(None).expect("history").is_empty());
}

#[test]
fn routine_start_does_not_record_an_update_lock_as_an_overlap() {
    let (_root, store, workspace) = fixture();
    let original_bot = create_bot(&store, "locked_original");
    let updated_bot = create_bot(&store, "locked_updated");
    let routine = store
        .create_routine(
            &original_bot.id,
            &workspace,
            "update while locked",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");
    let held_lock = std::cell::RefCell::new(None);

    let error = store
        .begin_run_inner(&routine.id, || {
            let lock = store
                .try_routine_lock(&routine.id)
                .expect("routine lock")
                .expect("uncontended routine lock");
            store
                .update(|state| {
                    let index = resolve_routine(&state.routines, &routine.id)?;
                    state.routines[index].bot_id.clone_from(&updated_bot.id);
                    Ok(())
                })
                .expect("interleaved update");
            held_lock.replace(Some(lock));
        })
        .err()
        .expect("mutation lock must not become an overlap run");

    assert!(error.to_string().contains("currently being modified"));
    assert_eq!(
        store.routine(&routine.id).expect("routine").bot_id,
        updated_bot.id
    );
    assert!(store.history(None).expect("history").is_empty());
}

#[test]
fn routine_start_does_not_orphan_history_behind_a_delete_lock() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "locked_delete");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "delete while locked",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");
    let held_lock = std::cell::RefCell::new(None);

    let error = store
        .begin_run_inner(&routine.id, || {
            let lock = store
                .try_routine_lock(&routine.id)
                .expect("routine lock")
                .expect("uncontended routine lock");
            store
                .update(|state| {
                    let index = resolve_routine(&state.routines, &routine.id)?;
                    state.routines.remove(index);
                    Ok(())
                })
                .expect("interleaved delete");
            held_lock.replace(Some(lock));
        })
        .err()
        .expect("deleted routine must not append history");

    assert!(error.to_string().contains("unknown routine"));
    assert!(store.history(None).expect("history").is_empty());
}

#[test]
fn due_routines_are_bot_owned_and_deduplicated_by_local_minute() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "daily");
    let now = Utc::now().timestamp();
    let local = Utc.timestamp_opt(now, 0).single().expect("time");
    let expression = format!("{} {} * * *", local.minute(), local.hour());
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "daily report",
            cron(&expression, "UTC"),
            None,
        )
        .expect("routine");

    let due = store.take_due(now).expect("due");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].0, routine.id);
    assert!(store.take_due(now + 1).expect("same minute").is_empty());
    store
        .finish_run(
            due.into_iter().next().expect("run").1,
            RoutineRunStatus::Succeeded,
            None,
        )
        .expect("finish");
}

#[test]
fn routine_rejects_unknown_bot_and_malformed_schedule() {
    let (_root, store, workspace) = fixture();
    assert!(
        store
            .create_routine(
                "missing",
                &workspace,
                "work",
                once(Utc::now().timestamp()),
                None,
            )
            .is_err()
    );
    let bot = create_bot(&store, "routine_bot");
    assert!(
        store
            .create_routine(&bot.id, &workspace, "work", cron("0 9 * *", "UTC"), None,)
            .is_err()
    );
}

#[test]
fn routine_input_wraps_raw_instructions_within_the_message_limit() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "routine_input");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "inspect cache behavior",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");
    let input = store.routine_input(&routine.id).expect("input").1;
    let record = store
        .routine_record(&routine.id, Utc::now().timestamp())
        .expect("routine record");
    let oversized = "x".repeat(MAX_ROUTINE_INSTRUCTIONS_BYTES + 1);
    let oversized_rejected = store
        .create_routine(
            &bot.id,
            &workspace,
            &oversized,
            once(Utc::now().timestamp() + 60),
            None,
        )
        .is_err();

    assert_eq!(
        (input, record.instructions, oversized_rejected),
        (
            "# Routine\n\nThe instructions below relate to a routine task.\n\ninspect cache behavior"
                .to_string(),
            "inspect cache behavior".to_string(),
            true,
        )
    );
}

#[test]
fn routine_update_atomically_swaps_its_instruction_snapshot() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "writer");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "old instructions",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");
    let old_path = routine.instructions.clone();

    let updated = store
        .update_routine(
            &routine.id,
            &bot.id,
            &workspace,
            "new instructions",
            once(Utc::now().timestamp() + 120),
            None,
            true,
        )
        .expect("update routine");

    assert_ne!(updated.instructions, old_path);
    assert!(!old_path.exists());
    assert_eq!(
        store.routine_input(&routine.id).expect("instructions").1,
        format!("{ROUTINE_SUBMISSION_PREFIX}\n\nnew instructions")
    );
    let record = store
        .routine_record(&routine.id, Utc::now().timestamp())
        .expect("routine record");
    assert_eq!(record.bot_id, updated.bot_id);
    assert_eq!(record.schedule, updated.schedule);
    assert_eq!(record.instructions, "new instructions");
    let reopened = BotStore::open(&root.path().join("state")).expect("reopen");
    assert_eq!(
        reopened.routine(&routine.id).expect("routine").instructions,
        updated.instructions
    );
}

#[test]
fn routine_delete_refuses_to_orphan_instruction_files() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "cleaner");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "remove me",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");
    let BeginRun::Started(run) = store.begin_run(&routine.id).expect("run") else {
        panic!("run must start");
    };
    store
        .finish_run(run, RoutineRunStatus::Succeeded, None)
        .expect("finish");
    std::fs::remove_file(&routine.instructions).expect("remove instruction file");
    std::fs::create_dir(&routine.instructions).expect("replace file with directory");

    let error = store
        .prepare_routine_deletion(&routine.id)
        .expect_err("invalid instructions must fail preflight");

    assert!(error.to_string().contains("instructions must remain"));
    assert_eq!(
        store.routine(&routine.id).expect("routine remains"),
        routine
    );
    assert_eq!(store.history(None).expect("history").len(), 1);
    assert!(routine.instructions.is_dir());
}

#[test]
fn missing_routine_workspace_does_not_block_reopen_or_unrelated_bot_writes() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "traveler");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "work elsewhere",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");
    let stored_workspace = routine.workspace.clone();
    drop(store);
    std::fs::remove_dir(&workspace).expect("remove workspace");

    let reopened = BotStore::open(&root.path().join("state")).expect("reopen Bot store");

    assert_eq!(
        reopened.routine(&routine.id).expect("routine").workspace,
        stored_workspace
    );
    create_bot(&reopened, "still_usable");
    assert!(
        reopened
            .update_routine(
                &routine.id,
                &bot.id,
                &workspace,
                "cannot update into a missing workspace",
                once(Utc::now().timestamp() + 120),
                None,
                true,
            )
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn managed_instructions_cannot_be_replaced_with_an_outside_symlink() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "symlink_guard");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "inside",
            cron("0 9 * * *", "UTC"),
            None,
        )
        .expect("routine");
    let outside = root.path().join("outside.md");
    std::fs::write(&outside, "outside").expect("outside instructions");
    std::fs::remove_file(&routine.instructions).expect("remove instructions");
    std::os::unix::fs::symlink(&outside, &routine.instructions).expect("replace with symlink");

    assert!(store.routine_input(&routine.id).is_err());
}

#[test]
fn missing_instruction_contents_fail_closed() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "missing_input");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "inside",
            cron("0 9 * * *", "UTC"),
            None,
        )
        .expect("routine");
    std::fs::remove_file(&routine.instructions).expect("remove instructions");

    assert!(store.routine_records(None, 1_000).is_err());
}

#[test]
fn routines_and_history_persist_with_bot_ownership() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "persistent");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "do work",
            cron("0 9 * * MON", "UTC"),
            None,
        )
        .expect("routine");
    let BeginRun::Started(run) = store.begin_run(&routine.id).expect("begin run") else {
        panic!("first run must start");
    };
    let session_id = run.session_id().to_owned();
    store
        .finish_run(run, RoutineRunStatus::Succeeded, None)
        .expect("finish run");
    drop(store);

    let reopened = BotStore::open(&root.path().join("state")).expect("reopen");
    assert_eq!(reopened.routine(&routine.id).expect("routine"), routine);
    assert_eq!(
        reopened.routine_input(&routine.id).expect("instructions").1,
        format!("{ROUTINE_SUBMISSION_PREFIX}\n\ndo work")
    );
    let runs = reopened.history(Some(&routine.id)).expect("history");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].bot_id, bot.id);
    assert_eq!(runs[0].session_id.as_deref(), Some(session_id.as_str()));
}

#[test]
fn once_and_interval_schedules_advance_without_replay_storms() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "cadence");
    let now = 1_000;
    let once_routine = store
        .create_routine(&bot.id, &workspace, "once", once(now - 1), None)
        .expect("once routine");
    let interval_routine = store
        .create_routine(&bot.id, &workspace, "interval", interval(60), None)
        .expect("interval routine");
    store
        .lock_state()
        .expect("state")
        .routines
        .iter_mut()
        .find(|routine| routine.id == interval_routine.id)
        .expect("stored interval")
        .next_run_at = Some(now - 1);

    assert_eq!(
        finish_due(&store, now),
        [once_routine.id.clone(), interval_routine.id.clone()]
    );
    assert!(finish_due(&store, now).is_empty());
    assert!(
        store
            .routine_record(&once_routine.id, now)
            .expect("once record")
            .finished
    );
    assert_eq!(
        store
            .routine(&interval_routine.id)
            .expect("interval")
            .next_run_at,
        Some(1_059)
    );
}

#[test]
fn bounded_interval_runs_its_last_due_occurrence() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "bounded");
    let routine = store
        .create_routine(&bot.id, &workspace, "last run", interval(60), Some(1_000))
        .expect("bounded routine");
    store
        .lock_state()
        .expect("state")
        .routines
        .iter_mut()
        .find(|stored| stored.id == routine.id)
        .expect("stored routine")
        .next_run_at = Some(1_000);

    let due = finish_due(&store, 1_007);
    assert_eq!(due, std::slice::from_ref(&routine.id));
    assert!(
        store
            .routine_record(&routine.id, 1_007)
            .expect("record")
            .finished
    );
    assert!(!store.has_active_routines(1_007).expect("active routines"));
}

#[test]
fn cron_next_occurrence_uses_iana_timezone_across_dst() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "dst");
    let routine = store
        .create_routine(
            &bot.id,
            &workspace,
            "cross DST",
            cron("30 1 * * *", "America/New_York"),
            None,
        )
        .expect("routine");
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

    assert_eq!(routine.next_run_at(now), Some(expected));
}

#[test]
fn run_history_never_evicts_owned_sessions() {
    let running = RoutineRun {
        id: Uuid::new_v4().to_string(),
        routine_id: Uuid::new_v4().to_string(),
        bot_id: Uuid::new_v4().to_string(),
        started_at: 0,
        finished_at: None,
        status: RoutineRunStatus::Running,
        session_id: Some(Uuid::new_v4().to_string()),
        message: None,
    };
    let mut state = BotState::default();
    state.runs.push(running.clone());
    for index in 1..300 {
        state.runs.push(RoutineRun {
            id: Uuid::new_v4().to_string(),
            routine_id: running.routine_id.clone(),
            bot_id: running.bot_id.clone(),
            started_at: index as i64,
            finished_at: Some(index as i64),
            status: RoutineRunStatus::Succeeded,
            session_id: Some(Uuid::new_v4().to_string()),
            message: None,
        });
    }
    append_run(
        &mut state,
        RoutineRun {
            id: Uuid::new_v4().to_string(),
            routine_id: running.routine_id.clone(),
            bot_id: running.bot_id.clone(),
            started_at: 1,
            finished_at: Some(1),
            status: RoutineRunStatus::Succeeded,
            session_id: Some(Uuid::new_v4().to_string()),
            message: None,
        },
    );

    assert_eq!(state.runs.len(), 301);
    assert!(state.runs.contains(&running));
}

#[test]
fn persisted_routine_paths_must_stay_in_the_private_directory() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "path_guard");
    let mut state = BotState::default();
    state.bots.push(bot.clone());
    state.routines.push(StoredRoutine {
        id: Uuid::new_v4().to_string(),
        bot_id: bot.id,
        workspace: std::fs::canonicalize(workspace).expect("workspace"),
        instructions: root.path().join("outside.md"),
        schedule: cron("0 9 * * *", "UTC"),
        ends_at: None,
        enabled: true,
        next_run_at: None,
        last_matched_minute: None,
    });

    assert!(
        validate_state(&state, &store.routines_dir)
            .expect_err("outside persisted routine must fail")
            .to_string()
            .contains("private gateway routine directory")
    );
}

#[test]
fn previous_state_version_is_rejected_without_compatibility() {
    let root = tempfile::tempdir().expect("root");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&state_dir).expect("state");
    let state = BotState {
        version: STATE_VERSION - 1,
        bots: Vec::new(),
        routines: Vec::new(),
        runs: Vec::new(),
        pending_bot_deletion: None,
    };
    std::fs::write(
        state_dir.join(STATE_FILE),
        serde_json::to_vec(&state).expect("encode old state"),
    )
    .expect("write old state");

    let error = match BotStore::open(&state_dir) {
        Ok(_) => panic!("old state must fail"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("unsupported Bot state version"));
}

#[test]
fn persisted_state_requires_the_default_mobius_bot() {
    let root = tempfile::tempdir().expect("root");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&state_dir).expect("state");
    std::fs::write(
        state_dir.join(STATE_FILE),
        serde_json::to_vec(&BotState::default()).expect("encode state"),
    )
    .expect("write state");

    let error = BotStore::open(&state_dir)
        .err()
        .expect("persisted state without @mobius must fail");

    assert!(error.to_string().contains("no built-in @mobius Bot"));
}

#[test]
fn malformed_or_out_of_range_schedule_is_rejected() {
    assert!(validate_schedule(&cron("0 9 * *", "UTC"), None).is_err());
    assert!(validate_schedule(&cron("75 9 * * *", "UTC"), None).is_err());
    assert!(validate_schedule(&cron("0 9 * * MON", "UTC"), None).is_ok());
    assert!(validate_schedule(&interval(59), None).is_err());
    assert!(validate_schedule(&once(1), Some(0)).is_err());
    assert!(validate_schedule(&once(2), Some(1)).is_err());
}
