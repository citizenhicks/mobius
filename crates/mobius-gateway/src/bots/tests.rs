use std::sync::Arc;

use super::*;
use crate::wire::BotAction;

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

impl BotStore {
    fn create_scheduled(
        &self,
        bot_id: &str,
        workspace: &Path,
        instructions: &str,
        schedule: RoutineSchedule,
        ends_at: Option<i64>,
    ) -> Result<StoredRoutine> {
        self.create_routine(
            bot_id,
            &scheduled_definition(workspace, instructions, schedule, ends_at),
            None,
        )
    }

    fn take_due(&self, now: i64) -> Result<Vec<(String, ActiveRoutineRun)>> {
        self.poll_due(now)?;
        let mut runs = Vec::new();
        for pending in self.pending_actions(now, 1000)? {
            let BotAction::Routine { command } = &pending.action else {
                continue;
            };
            if command.action != RoutineAction::Start {
                continue;
            }
            match self.begin_run_with_cause(
                &command.routine_id,
                &pending.id,
                Some(&pending.event),
            )? {
                BeginRun::Started(run) => runs.push((command.routine_id.clone(), run)),
                BeginRun::Skipped | BeginRun::AlreadyRecorded => {}
            }
            self.action_accepted(&pending.id)?;
        }
        Ok(runs)
    }
}
fn scheduled_definition(
    workspace: &Path,
    instructions: &str,
    schedule: RoutineSchedule,
    ends_at: Option<i64>,
) -> RoutineDefinition {
    RoutineDefinition {
        workspace: workspace.to_path_buf(),
        instructions: instructions.into(),
        bindings: vec![RoutineBinding {
            id: Uuid::new_v4().to_string(),
            on: HookSelector::Schedule { schedule, ends_at },
            action: RoutineAction::Start,
        }],
    }
}
fn schedule_of(routine: &StoredRoutine) -> RoutineSchedule {
    let HookSelector::Schedule { schedule, .. } = &routine.bindings[0].definition.on else {
        panic!("scheduled fixture");
    };
    schedule.clone()
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
fn clock_records_only_due_facts_and_start_commands_deduplicate_after_completion() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "clock");
    let now = Utc::now().timestamp();
    let routine = store
        .create_scheduled(&bot.id, &workspace, "check the service", once(now), None)
        .expect("routine");
    let poll = store.poll_due(now).expect("clock");
    assert_eq!(poll.events.len(), 1);
    assert!(
        store
            .history(None)
            .expect("no clock reservation")
            .is_empty()
    );
    assert!(store.poll_due(now).expect("no replay").events.is_empty());
    let action = store
        .pending_actions(now, 10)
        .expect("start command")
        .into_iter()
        .find(|pending| matches!(pending.action, BotAction::Routine { .. }))
        .expect("start");
    let BeginRun::Started(run) = store
        .begin_run_with_cause(&routine.id, &action.id, Some(&action.event))
        .expect("start")
    else {
        panic!("expected run")
    };
    assert!(matches!(
        store
            .begin_run_with_cause(&routine.id, &action.id, Some(&action.event))
            .expect("retry while active"),
        BeginRun::AlreadyRecorded
    ));
    store
        .finish_run(run, RoutineRunStatus::Succeeded, None)
        .expect("finish");
    assert!(matches!(
        store
            .begin_run_with_cause(&routine.id, &action.id, Some(&action.event))
            .expect("retry after finish"),
        BeginRun::AlreadyRecorded
    ));
    assert_eq!(store.history(None).expect("one invocation").len(), 1);
}

#[test]
fn concurrent_bot_stores_refresh_the_catalog_before_reads_and_seed() {
    let root = tempfile::tempdir().expect("root");
    let state = root.path().join("state");
    std::fs::create_dir(&state).expect("state");
    let first = BotStore::open(&state).expect("first store");
    let second = BotStore::open(&state).expect("second store");
    let defaults = VersionedAgentConfig {
        revision: 1,
        config: AgentComposition::default(),
    };

    let mobius = first
        .seed_default(&defaults)
        .expect("seed default")
        .expect("first seed");
    assert!(
        second
            .seed_default(&defaults)
            .expect("second seed")
            .is_none()
    );
    assert_eq!(second.bots().expect("refreshed bots"), [mobius]);

    let created = first
        .create_bot("fresh", "Fresh Bot", AgentComposition::default())
        .expect("create Bot");
    assert!(
        second
            .bots()
            .expect("refreshed catalog")
            .iter()
            .any(|bot| bot.id == created.id)
    );
}

#[test]
fn bot_deletion_removes_owned_routines_history_and_scripts() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "retired");
    let routine = store
        .create_scheduled(
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
    drop(store);

    let reopened = BotStore::open(&root.path().join("state")).expect("reopen");
    assert!(reopened.bot(&bot.id).is_err());
    assert!(reopened.routine(&routine.id).is_err());
    assert!(reopened.history(None).expect("history").is_empty());
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
            store.create_scheduled(
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
fn routine_start_does_not_orphan_history_behind_a_delete_lock() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "locked_delete");
    let routine = store
        .create_scheduled(
            &bot.id,
            &workspace,
            "delete while locked",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");
    let held_lock = std::cell::RefCell::new(None);

    let error = store
        .begin_run_inner(&routine.id, &Uuid::new_v4().to_string(), None, || {
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

    assert!(error.to_string().contains("routine was deleted"));
    assert!(store.history(None).expect("history").is_empty());
}

#[cfg(unix)]
#[test]
fn managed_instructions_cannot_be_replaced_with_an_outside_symlink() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "symlink_guard");
    let routine = store
        .create_scheduled(
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
fn routines_and_history_persist_with_bot_ownership() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "persistent");
    let routine = store
        .create_scheduled(
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
        .create_scheduled(&bot.id, &workspace, "once", once(now - 1), None)
        .expect("once routine");
    let interval_routine = store
        .create_scheduled(&bot.id, &workspace, "interval", interval(60), None)
        .expect("interval routine");
    let mut state = store.fresh_state().expect("state");
    state
        .routines
        .iter_mut()
        .find(|routine| routine.id == interval_routine.id)
        .expect("stored interval")
        .bindings[0]
        .next_due_at = Some(now - 1);
    store.save(&state).expect("persist interval");

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
            .bindings[0]
            .next_due_at,
        Some(1_059)
    );
}

#[test]
fn cron_next_occurrence_uses_iana_timezone_across_dst() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "dst");
    let routine = store
        .create_scheduled(
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

    assert_eq!(
        next_cron_occurrence(&schedule_of(&routine), now, false).expect("next cron occurrence"),
        expected
    );
}

#[test]
fn run_history_exceeds_one_megabyte_without_evicting_sessions() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "long_history");
    let routine = store
        .create_scheduled(
            &bot.id,
            &workspace,
            "keep every run",
            cron("0 9 * * *", "UTC"),
            None,
        )
        .expect("routine");
    let message = "x".repeat(20 * 1024);
    let mut newest = None;
    for _ in 0..64 {
        let BeginRun::Started(run) = store.begin_run(&routine.id).expect("begin run") else {
            panic!("run must start");
        };
        newest = Some(
            store
                .finish_run(run, RoutineRunStatus::Succeeded, Some(message.clone()))
                .expect("finish run")
                .id,
        );
    }

    let history = store.history(Some(&routine.id)).expect("history");
    assert!(serde_json::to_vec(&history).expect("history JSON").len() > MAX_STATE_BYTES as usize);
    assert_eq!(history.len(), 64);
    assert_eq!(
        history.first().expect("newest run").id,
        newest.expect("run")
    );
    drop(store);

    let reopened = BotStore::open(&root.path().join("state")).expect("reopen");
    assert_eq!(
        reopened.history(Some(&routine.id)).expect("history"),
        history
    );
    let deletion = reopened
        .prepare_routine_deletion(&routine.id)
        .expect("prepare routine deletion");
    reopened
        .delete_routine(deletion, None, None)
        .expect("delete routine");
    drop(reopened);

    let reopened = BotStore::open(&root.path().join("state")).expect("reopen after deletion");
    assert!(reopened.routine(&routine.id).is_err());
    assert!(reopened.history(None).expect("deleted history").is_empty());
}

#[test]
fn persisted_routine_paths_must_stay_in_the_private_directory() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "path_guard");
    let mut state = BotState::default();
    state.bots.push(StoredBot::from(&bot));
    state.routines.push(StoredRoutine {
        id: Uuid::new_v4().to_string(),
        bot_id: bot.id,
        workspace: std::fs::canonicalize(workspace).expect("workspace"),
        instructions: root.path().join("outside.md"),
        bindings: vec![
            StoredRoutineBinding::new(
                RoutineBinding {
                    id: Uuid::new_v4().to_string(),
                    on: HookSelector::Schedule {
                        schedule: cron("0 9 * * *", "UTC"),
                        ends_at: None,
                    },
                    action: RoutineAction::Start,
                },
                Utc::now().timestamp(),
                false,
            )
            .unwrap(),
        ],
        enabled: true,
    });

    assert!(
        validate_state(&state, &store.routines_dir)
            .expect_err("outside persisted routine must fail")
            .to_string()
            .contains("private gateway routine directory")
    );
}
