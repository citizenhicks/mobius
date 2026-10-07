use std::sync::Arc;

use super::*;
use crate::wire::{BotAction, BotSubscription, HookBinding, HookKind, RoutineCommand};

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

#[test]
fn sparse_saved_bot_projects_integer_defaults_without_rewriting_storage() {
    let (_root, store, _workspace) = fixture();
    let mut state = store.fresh_state().expect("saved catalog");
    let middleware = &mut state.bots[0].config.config.middleware;
    for entry in crate::middleware_manifest::MIDDLEWARE.iter() {
        for setting in entry.manifest.settings {
            if matches!(
                setting,
                mobius::middleware::manifest::MiddlewareSettingManifest::Integer { .. }
            ) {
                middleware.set_setting(entry.manifest.id, setting.id(), None);
            }
        }
    }
    store
        .save(&state)
        .expect("persist sparse existing configuration");
    let before = store.storage.load_catalog().expect("raw stored catalog");
    let reopened = BotStore::open(&store.state_dir).expect("reopen sparse catalog");
    let bot = reopened.bots().expect("frontend records").remove(0);
    for id in ["max_depth", "max_concurrency", "max_agents"] {
        assert!(
            bot.config
                .config
                .middleware
                .setting("subagents", id)
                .is_none()
        );
    }
    let defaults = crate::middleware_manifest::default_config();
    for (owner, id) in [
        ("sandbox", "tool_output_bytes"),
        ("sandbox", "background_commands"),
        ("compaction", "reserve_tokens"),
    ] {
        assert_eq!(
            bot.config.config.middleware.setting(owner, id),
            defaults.setting(owner, id)
        );
    }
    assert_eq!(
        reopened
            .storage
            .load_catalog()
            .expect("unchanged stored catalog"),
        before
    );
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
    #[expect(
        clippy::too_many_arguments,
        reason = "existing fixture fields are translated into the shared typed definition and pause command"
    )]
    fn update_scheduled(
        &self,
        id: &str,
        bot_id: &str,
        workspace: &Path,
        instructions: &str,
        schedule: RoutineSchedule,
        ends_at: Option<i64>,
        enabled: bool,
    ) -> Result<StoredRoutine> {
        if self.routine(id)?.bot_id != bot_id {
            return Err(Error::Config(
                "routine owner changes require an explicit owner command".into(),
            ));
        }
        self.update_routine(
            id,
            &scheduled_definition(workspace, instructions, schedule, ends_at),
            None,
            None,
        )?;
        self.set_routine_enabled(id, enabled, None, None)
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
fn pause_changes_start_eligibility_without_interrupting_run_or_disabling_resume_hook() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "pausable");
    let mut definition = scheduled_definition(&workspace, "check it", interval(60), None);
    definition.bindings.push(RoutineBinding {
        id: "resume".into(),
        on: event_selector(
            HookSource::Session {
                session_id: "watched".into(),
            },
            HookKind::SessionTurnFinished,
        ),
        action: RoutineAction::Resume,
    });
    let routine = store
        .create_routine(&bot.id, &definition, None)
        .expect("routine");
    let now = Utc::now().timestamp();
    let BeginRun::Started(active) = store.begin_run(&routine.id).expect("start") else {
        panic!("expected run")
    };
    store
        .set_routine_enabled(&routine.id, false, None, None)
        .expect("pause during run");
    assert_eq!(
        store.run(active.id()).expect("still active").status,
        RoutineRunStatus::Running
    );
    assert!(
        store
            .begin_run(&routine.id)
            .err()
            .expect("paused")
            .to_string()
            .contains("paused")
    );
    assert!(
        store
            .poll_due(now + 600)
            .expect("paused clock")
            .events
            .is_empty()
    );
    let event = HookEvent {
        id: "finished-session".into(),
        source: HookSource::Session {
            session_id: "watched".into(),
        },
        cause_id: None,
        ancestry: Vec::new(),
        bot_id: bot.id.clone(),
        occurred_at: now,
        data: HookData::SessionTurnFinished {
            session_id: "watched".into(),
            turn_id: "turn".into(),
            outcome: mobius::backend::checkpoint::ExecutionOutcome::Completed,
        },
    };
    store.record_hook(&event).expect("event while paused");
    let pending = store
        .pending_actions(event.occurred_at, 10)
        .expect("resume consumer")
        .into_iter()
        .find(|pending| {
            matches!(
                &pending.action,
                BotAction::Routine { command } if command.action == RoutineAction::Resume
            )
        })
        .expect("resume action");
    let resumed = store
        .set_routine_enabled(&routine.id, true, Some(&pending.event), Some(&pending.id))
        .expect("resume");
    assert!(
        !store
            .action_pending(&pending.id)
            .expect("ack with mutation")
    );
    assert!(resumed.bindings[0].next_due_at.expect("future timer") > now);
    store
        .finish_run(active, RoutineRunStatus::Succeeded, None)
        .expect("finish prior invocation");
}

#[test]
fn no_op_definitions_and_pause_commands_emit_no_extra_fact_and_ack_hooks() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "noop");
    let now = Utc::now().timestamp();
    let definition = scheduled_definition(&workspace, "instructions", interval(60), None);
    let routine = store
        .create_routine(&bot.id, &definition, None)
        .expect("routine");
    let before = store.unpublished_events(100).expect("created").len();
    store
        .update_routine(&routine.id, &definition, None, None)
        .expect("unchanged definition");
    store
        .set_routine_enabled(&routine.id, true, None, None)
        .expect("already resumed");
    assert_eq!(
        store.unpublished_events(100).expect("no facts").len(),
        before
    );
    let subscription = BotSubscription {
        bot_id: bot.id.clone(),
        binding: HookBinding {
            id: "resume-hook".into(),
            on: event_selector(
                HookSource::Bot {
                    bot_id: bot.id.clone(),
                },
                HookKind::CustomReceived,
            ),
            action: BotAction::Routine {
                command: RoutineCommand {
                    routine_id: routine.id.clone(),
                    action: RoutineAction::Resume,
                },
            },
        },
        enabled: true,
    };
    store
        .set_subscription(&subscription, 0, now)
        .expect("consumer");
    let event = HookEvent {
        id: "resume-input".into(),
        source: HookSource::Bot {
            bot_id: bot.id.clone(),
        },
        cause_id: None,
        ancestry: Vec::new(),
        bot_id: bot.id.clone(),
        occurred_at: now,
        data: HookData::CustomReceived {
            name: "resume".into(),
            data: serde_json::json!({}),
        },
    };
    store.record_hook(&event).expect("input");
    let pending = store.pending_actions(now, 10).expect("command").remove(0);
    store
        .set_routine_enabled(&routine.id, true, Some(&pending.event), Some(&pending.id))
        .expect("no op acceptance");
    assert!(!store.action_pending(&pending.id).expect("accepted"));
    assert_eq!(
        store
            .unpublished_events(100)
            .expect("only input added")
            .len(),
        before + 1
    );
}

#[test]
fn fresh_state_seeds_one_mobius_bot_whose_handle_survives_rename() {
    let (root, store, _) = unseeded_fixture();
    assert!(root.path().join("state").join(STATE_FILE).exists());
    assert!(store.storage.load_catalog().expect("catalog").is_none());
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
            bot.shape,
            bot.config.revision,
        ),
        (
            "mobius",
            "Mobius",
            MOBIUS_DESCRIPTION,
            ProviderTint::Blue,
            BotShape::Circle,
            1,
        )
    );
    assert_eq!(bot.config.config, defaults.config);
    let renamed = store
        .update_bot(
            &bot.id,
            bot.config.revision,
            BotIdentity {
                name: "My assistant",
                description: &bot.description,
                tint: bot.tint,
                shape: bot.shape,
            },
            bot.config.config.clone(),
        )
        .expect("rename built-in Bot");
    assert_eq!(renamed.id, bot.id);
    assert_eq!(renamed.handle, "mobius");
    assert!(
        store
            .prepare_bot_deletion(&renamed.id, renamed.config.revision)
            .expect_err("built-in Bot cannot be deleted")
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
    assert_eq!(reopened.bots().expect("Bots"), [renamed]);
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
fn reads_reuse_the_parsed_catalog_until_any_store_commits() {
    let (root, store, _) = fixture();
    let other = BotStore::open(&root.path().join("state")).expect("second store");
    let parses = || CATALOG_PARSES.with(std::cell::Cell::get);
    store.bots().expect("first read");
    let before = parses();
    for _ in 0..3 {
        assert_eq!(store.bots().expect("cached read").len(), 1);
        assert!(!store.has_pending_bot_deletion().expect("cached check"));
    }
    assert_eq!(parses(), before);

    let created = other
        .create_bot("elsewhere", "Another store", AgentComposition::default())
        .expect("create from the other store");
    assert!(
        store.bot(&created.id).is_ok(),
        "another connection's commit"
    );
    let own = create_bot(&store, "own");
    assert!(store.bots().expect("own commit").contains(&own));
}

#[test]
fn opening_bot_state_rejects_removed_automatic_approval_settings_without_rewrite() {
    let (root, store, _) = fixture();
    create_bot(&store, "second");
    let mut state = store.fresh_state().expect("Bot state");
    for bot in &mut state.bots {
        let middleware = &mut bot.config.config.middleware;
        middleware.set_setting(
            "sandbox",
            "approval_policy",
            Some(mobius::protocol::FrontendSettingValue::String(
                "auto_approve".into(),
            )),
        );
        middleware.set_setting(
            "sandbox",
            "reviewer_model_route",
            Some(mobius::protocol::FrontendSettingValue::String(
                "reviewer".into(),
            )),
        );
        middleware.set_setting(
            "sandbox",
            "reviewer_strictness",
            Some(mobius::protocol::FrontendSettingValue::String(
                "strict".into(),
            )),
        );
    }
    store.save(&state).expect("write incompatible Bot state");
    drop(store);

    let state_dir = root.path().join("state");
    let before = std::fs::read(state_dir.join(STATE_FILE)).expect("Bot state");
    let error = match BotStore::open(&state_dir) {
        Ok(_) => panic!("removed Bot settings must be rejected"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("reviewer_model_route"));
    assert_eq!(
        std::fs::read(state_dir.join(STATE_FILE)).expect("unchanged Bot state"),
        before
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

#[test]
fn definition_updates_preserve_owner_and_earlier_history() {
    let (root, store, workspace) = fixture();
    let original = create_bot(&store, "original");

    let routine = store
        .create_scheduled(&original.id, &workspace, "work", interval(60), None)
        .expect("routine");
    let BeginRun::Started(active) = store.begin_run(&routine.id).expect("start") else {
        panic!("run must start");
    };
    let run = store
        .finish_run(active, RoutineRunStatus::Succeeded, None)
        .expect("finish");
    store
        .update_scheduled(
            &routine.id,
            &original.id,
            &workspace,
            "updated work",
            schedule_of(&routine),
            None,
            true,
        )
        .expect("update definition");
    assert_eq!(store.routine(&routine.id).unwrap().bot_id, original.id);
    drop(store);

    let reopened = BotStore::open(&root.path().join("state")).expect("reopen");
    for id in [routine.id.as_str(), &routine.id[..8]] {
        assert_eq!(
            reopened.history(Some(id)).expect("original Bot history"),
            std::slice::from_ref(&run)
        );
    }
}

#[test]
fn bot_deletion_refuses_to_orphan_instruction_files() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "blocked_cleanup");
    let routine = store
        .create_scheduled(
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
        .create_scheduled(
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
fn bot_profile_update_preserves_identity_and_persists_exact_revision() {
    let (root, store, _) = fixture();
    let created = create_bot(&store, "reviewer");
    let mut config = created.config.config.clone();
    config.system_prompt = "Review carefully".into();

    let updated = store
        .update_bot(
            &created.id,
            1,
            BotIdentity {
                name: "Code reviewer",
                description: "Review code carefully.",
                tint: ProviderTint::Purple,
                shape: BotShape::Star,
            },
            config,
        )
        .expect("update Bot");

    assert_eq!(updated.id, created.id);
    assert_eq!(updated.handle, "code-reviewer");
    assert_eq!(updated.name, "Code reviewer");
    assert_eq!(updated.config.revision, 2);
    assert_eq!(updated.shape, BotShape::Star);
    let reopened = BotStore::open(&root.path().join("state")).expect("reopen");
    assert_eq!(reopened.bot(&created.id).expect("stored Bot"), updated);
}

#[test]
fn new_bots_wear_the_first_shape_no_bot_has() {
    let (_root, store, _) = fixture();
    let shapes = ["first", "second"].map(|handle| create_bot(&store, handle).shape);
    assert_eq!(shapes, [BotShape::Squircle, BotShape::Triangle]);
}

#[test]
fn bot_records_project_runtime_semantics_without_persisting_them() {
    let (_root, store, _) = fixture();
    let mut config = AgentComposition::default();
    config
        .middleware
        .set_enabled(mobius::middleware::attachments::MANIFEST.id, true);
    config.middleware.set_setting(
        "sandbox",
        "approval_policy",
        Some(mobius::protocol::FrontendSettingValue::String("ask".into())),
    );

    let bot = store
        .create_bot("Semantic", "Project semantic capabilities.", config)
        .expect("create Bot");

    assert!(bot.accepts_file_attachments);
    assert_eq!(
        bot.routine_interaction_policy,
        crate::wire::RoutineInteractionPolicy::MayPauseForApproval
    );

    let catalog = store
        .storage
        .load_catalog()
        .expect("load catalog")
        .expect("persisted catalog");
    let catalog: serde_json::Value = serde_json::from_str(&catalog).expect("catalog JSON");
    let stored = catalog["bots"]
        .as_array()
        .expect("stored Bots")
        .iter()
        .find(|stored| stored["id"] == bot.id)
        .expect("stored Bot");
    assert!(stored.get("accepts_file_attachments").is_none());
    assert!(stored.get("routine_interaction_policy").is_none());
}

#[test]
fn bot_rename_derives_unique_handles_without_colliding_with_itself() {
    let (_root, store, _) = fixture();
    create_bot(&store, "builder");
    let duplicate = store
        .create_bot("builder", "Own other work.", AgentComposition::default())
        .expect("second Bot");
    let reserved = store
        .create_bot("User", "Human-facing work.", AgentComposition::default())
        .expect("reserved handle is suffixed");
    assert_eq!(duplicate.handle, "builder-2");
    assert_eq!(reserved.handle, "user-2");

    let mut bot = duplicate;
    for (name, handle) in [
        ("Builder", "builder-2"),
        ("Renamed", "renamed"),
        ("RENAMED", "renamed"),
        ("builder", "builder-2"),
        ("User", "user-3"),
        ("Mobius", "mobius-2"),
    ] {
        bot = store
            .update_bot(
                &bot.id,
                bot.config.revision,
                BotIdentity {
                    name,
                    description: "Own renamed work.",
                    tint: ProviderTint::Teal,
                    shape: bot.shape,
                },
                bot.config.config,
            )
            .expect("rename Bot");
        assert_eq!(bot.handle, handle, "renamed to {name}");
    }
}

#[test]
fn updating_bot_without_renaming_preserves_its_suffixed_handle() {
    let (_root, store, _) = fixture();
    let first = create_bot(&store, "reviewer");
    let duplicate = create_bot(&store, "reviewer");
    let deletion = store
        .prepare_bot_deletion(&first.id, first.config.revision)
        .expect("prepare deletion");
    store.delete_bot(deletion).expect("free unsuffixed handle");

    let updated = store
        .update_bot(
            &duplicate.id,
            duplicate.config.revision,
            BotIdentity {
                name: &duplicate.name,
                description: "Changed description, not name.",
                tint: duplicate.tint,
                shape: duplicate.shape,
            },
            duplicate.config.config,
        )
        .expect("update profile");

    assert_eq!(updated.handle, "reviewer-2");
}

#[test]
fn running_routine_reserves_its_fresh_session_before_execution() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "operator");
    let routine = store
        .create_scheduled(
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
        .create_scheduled(
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
        .create_scheduled(
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
        .record_bot_deletion(&mut deletion, &[], &[])
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
        .create_scheduled(&worker.id, &workspace, "wait for recovery", once(now), None)
        .expect("routine");
    let mut deletion = store
        .prepare_bot_deletion(&deleting.id, deleting.config.revision)
        .expect("prepare deletion");
    store
        .record_bot_deletion(&mut deletion, &[], &[])
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
#[cfg(unix)]
fn routine_lock_releases_even_with_an_inherited_descriptor() {
    let (_root, store, _workspace) = fixture();
    let lock = store
        .try_routine_lock("fixture")
        .expect("lock")
        .expect("available");
    // A duplicate shares the open file description just as a fork does.
    let inherited = lock.0.try_clone().expect("inherited descriptor");
    drop(lock);
    assert!(
        store
            .try_routine_lock("fixture")
            .expect("released lock")
            .is_some()
    );
    drop(inherited);
}

#[test]
fn routine_start_reloads_an_update_that_wins_before_its_lock() {
    let (_root, store, workspace) = fixture();
    let original_bot = create_bot(&store, "original");
    let routine = store
        .create_scheduled(
            &original_bot.id,
            &workspace,
            "original instructions",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");

    let BeginRun::Started(active) = store
        .begin_run_inner(&routine.id, &Uuid::new_v4().to_string(), None, || {
            store
                .update_scheduled(
                    &routine.id,
                    &original_bot.id,
                    &workspace,
                    "updated instructions",
                    schedule_of(&routine),
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
    assert_eq!(run.bot_id, original_bot.id);
    assert!(
        store
            .routine_input(&routine.id)
            .expect("updated instructions")
            .1
            .contains("updated instructions")
    );
    store
        .finish_run(active, RoutineRunStatus::Succeeded, None)
        .expect("finish invocation");
}

#[test]
fn routine_start_rejects_a_delete_that_wins_before_its_lock() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "deleted");
    let routine = store
        .create_scheduled(
            &bot.id,
            &workspace,
            "delete before start",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");

    let error = store
        .begin_run_inner(&routine.id, &Uuid::new_v4().to_string(), None, || {
            let deletion = store
                .prepare_routine_deletion(&routine.id)
                .expect("prepare interleaved delete");
            store
                .delete_routine(deletion, None, None)
                .expect("interleaved delete");
        })
        .err()
        .expect("deleted routine must not start");

    assert!(error.to_string().contains("routine was deleted"));
    assert!(store.history(None).expect("history").is_empty());
}

#[test]
fn routine_start_does_not_record_an_update_lock_as_an_overlap() {
    let (_root, store, workspace) = fixture();
    let original_bot = create_bot(&store, "locked_original");
    let updated_bot = create_bot(&store, "locked_updated");
    let routine = store
        .create_scheduled(
            &original_bot.id,
            &workspace,
            "update while locked",
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

#[test]
fn due_routines_are_bot_owned_and_deduplicated_by_local_minute() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "daily");
    let now = Utc::now().timestamp();
    let local = Utc.timestamp_opt(now, 0).single().expect("time");
    let expression = format!("{} {} * * *", local.minute(), local.hour());
    let routine = store
        .create_scheduled(
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
            .create_scheduled(
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
            .create_scheduled(&bot.id, &workspace, "work", cron("0 9 * *", "UTC"), None,)
            .is_err()
    );
}

#[test]
fn new_schedule_inputs_reject_milliseconds_without_breaking_stored_timestamps() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "seconds");
    let now = Utc::now().timestamp();
    let bad = scheduled_definition(&workspace, "work", once(now * 1000), None);
    assert!(
        store
            .create_routine(&bot.id, &bad, None)
            .unwrap_err()
            .to_string()
            .contains("Unix epoch seconds")
    );
    let routine = store
        .create_scheduled(&bot.id, &workspace, "work", once(now), None)
        .unwrap();
    assert!(store.update_routine(&routine.id, &bad, None, None).is_err());
    assert_eq!(
        schedule_of(&store.routine(&routine.id).unwrap()).at,
        Some(now)
    );
    let subscription = BotSubscription {
        bot_id: bot.id.clone(),
        enabled: true,
        binding: HookBinding {
            id: "bad-update".into(),
            on: event_selector(
                HookSource::Bot {
                    bot_id: bot.id.clone(),
                },
                HookKind::CustomReceived,
            ),
            action: BotAction::Routine {
                command: RoutineCommand {
                    routine_id: routine.id.clone(),
                    action: RoutineAction::Update { definition: bad },
                },
            },
        },
    };
    assert!(store.set_subscription(&subscription, 0, now).is_err());

    // Previously accepted state remains readable; it is never silently converted.
    let mut state = store.fresh_state().unwrap();
    let stored = state
        .routines
        .iter_mut()
        .find(|stored| stored.id == routine.id)
        .unwrap();
    let HookSelector::Schedule { schedule, .. } = &mut stored.bindings[0].definition.on else {
        panic!("schedule")
    };
    schedule.at = Some(now * 1000);
    stored.bindings[0].next_due_at = Some(now * 1000);
    store.save(&state).unwrap();
    drop(store);
    let reopened = BotStore::open(&root.path().join("state")).unwrap();
    assert_eq!(
        schedule_of(&reopened.routine(&routine.id).unwrap()).at,
        Some(now * 1000)
    );
}

#[test]
fn routine_completion_only_queues_reports_when_the_user_subscribes() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "reporting");
    let now = Utc::now().timestamp();
    let routine = store
        .create_scheduled(&bot.id, &workspace, "work", once(now), None)
        .unwrap();
    assert!(store.subscriptions(&bot.id).unwrap().is_empty());
    let BeginRun::Started(run) = store.begin_run(&routine.id).unwrap() else {
        panic!("run")
    };
    store
        .finish_run(run, RoutineRunStatus::Succeeded, None)
        .unwrap();
    assert!(store.pending_actions(now + 60, 10).unwrap().is_empty());
    assert_eq!(store.history(Some(&routine.id)).unwrap().len(), 1);

    store
        .set_subscription(
            &BotSubscription {
                bot_id: bot.id.clone(),
                enabled: true,
                binding: HookBinding {
                    id: "requested-report".into(),
                    on: event_selector(
                        HookSource::Routine {
                            routine_id: routine.id.clone(),
                        },
                        HookKind::RunFinished,
                    ),
                    action: BotAction::Report {
                        instruction: "Tell me the result".into(),
                    },
                },
            },
            0,
            now,
        )
        .unwrap();
    let BeginRun::Started(run) = store.begin_run(&routine.id).unwrap() else {
        panic!("run")
    };
    store
        .finish_run(run, RoutineRunStatus::Succeeded, None)
        .unwrap();
    let pending = store.pending_actions(now + 60, 10).unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        matches!(&pending[0].action, BotAction::Report { instruction } if instruction == "Tell me the result")
    );
}

#[test]
fn routine_input_wraps_raw_instructions_within_the_message_limit() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "routine_input");
    let routine = store
        .create_scheduled(
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
        .create_scheduled(
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
        .create_scheduled(
            &bot.id,
            &workspace,
            "old instructions",
            once(Utc::now().timestamp() + 60),
            None,
        )
        .expect("routine");
    let old_path = routine.instructions.clone();

    let updated = store
        .update_scheduled(
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
    assert_eq!(
        record.bindings,
        updated
            .bindings
            .iter()
            .map(|binding| binding.definition.clone())
            .collect::<Vec<_>>()
    );
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
        .create_scheduled(
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
        .create_scheduled(
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
            .update_scheduled(
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
fn missing_instruction_contents_fail_closed() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "missing_input");
    let routine = store
        .create_scheduled(
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
fn bounded_interval_runs_its_last_due_occurrence() {
    let (_root, store, workspace) = fixture();
    let bot = create_bot(&store, "bounded");
    let routine = store
        .create_scheduled(&bot.id, &workspace, "last run", interval(60), Some(1_000))
        .expect("bounded routine");
    let mut state = store.fresh_state().expect("state");
    state
        .routines
        .iter_mut()
        .find(|stored| stored.id == routine.id)
        .expect("stored routine")
        .bindings[0]
        .next_due_at = Some(1_000);
    store.save(&state).expect("persist routine");

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
fn reopening_during_dst_fallback_does_not_replay_an_ambiguous_cron_time() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "dst_restart");
    let routine = store
        .create_scheduled(
            &bot.id,
            &workspace,
            "cross fallback",
            cron("30 1 * * *", "America/New_York"),
            None,
        )
        .expect("routine");
    let stale = Utc
        .with_ymd_and_hms(2024, 11, 2, 5, 30, 0)
        .single()
        .expect("stale timestamp")
        .timestamp();
    let now = Utc
        .with_ymd_and_hms(2024, 11, 3, 6, 15, 0)
        .single()
        .expect("restart timestamp")
        .timestamp();
    let expected = Utc
        .with_ymd_and_hms(2024, 11, 4, 6, 30, 0)
        .single()
        .expect("next timestamp")
        .timestamp();
    let mut state = store.fresh_state().expect("Bot state");
    let stored = state
        .routines
        .iter_mut()
        .find(|stored| stored.id == routine.id)
        .expect("stored routine");
    stored.bindings[0].next_due_at = Some(stale);
    stored.bindings[0].last_matched_minute = None;
    store.save(&state).expect("persist stale cron cursor");
    drop(store);

    let reopened = BotStore::open(&root.path().join("state")).expect("reopen Bot store");
    assert!(reopened.poll_due(now).expect("poll due").events.is_empty());
    assert_eq!(
        reopened
            .routine_record(&routine.id, now)
            .expect("routine record")
            .next_run_at,
        Some(expected)
    );
}

#[test]
fn reopening_does_not_dispatch_a_missed_cron_minute() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "missed_cron");
    let routine = store
        .create_scheduled(
            &bot.id,
            &workspace,
            "do not backfill",
            cron("0 9 * * *", "UTC"),
            None,
        )
        .expect("routine");
    let stale = Utc
        .with_ymd_and_hms(2024, 3, 10, 9, 0, 0)
        .single()
        .expect("stale timestamp")
        .timestamp();
    let now = Utc
        .with_ymd_and_hms(2024, 3, 11, 10, 0, 0)
        .single()
        .expect("restart timestamp")
        .timestamp();
    let expected = Utc
        .with_ymd_and_hms(2024, 3, 12, 9, 0, 0)
        .single()
        .expect("next timestamp")
        .timestamp();
    let mut state = store.fresh_state().expect("Bot state");
    let stored = state
        .routines
        .iter_mut()
        .find(|stored| stored.id == routine.id)
        .expect("stored routine");
    stored.bindings[0].next_due_at = Some(stale);
    stored.bindings[0].last_matched_minute = None;
    store.save(&state).expect("persist stale cron cursor");
    drop(store);

    let reopened = BotStore::open(&root.path().join("state")).expect("reopen Bot store");
    assert!(reopened.poll_due(now).expect("poll due").events.is_empty());
    assert_eq!(
        reopened
            .routine_record(&routine.id, now)
            .expect("routine record")
            .next_run_at,
        Some(expected)
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
fn reopening_retains_running_run_for_gateway_reconciliation() {
    let (root, store, workspace) = fixture();
    let bot = create_bot(&store, "recovery");
    let routine = store
        .create_scheduled(
            &bot.id,
            &workspace,
            "recover me",
            cron("0 9 * * *", "UTC"),
            None,
        )
        .expect("routine");
    let BeginRun::Started(run) = store.begin_run(&routine.id).expect("begin run") else {
        panic!("run must start");
    };
    let session_id = run.session_id().to_owned();
    drop(run);
    drop(store);

    let reopened = BotStore::open(&root.path().join("state")).expect("reopen");
    let recovered = reopened.history(Some(&routine.id)).expect("history");
    assert_eq!(recovered[0].status, RoutineRunStatus::Running);
    assert!(recovered[0].message.is_none());
    assert_eq!(
        recovered[0].session_id.as_deref(),
        Some(session_id.as_str())
    );
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

#[test]
fn previous_state_version_is_rejected_without_compatibility() {
    let root = tempfile::tempdir().expect("root");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&state_dir).expect("state");
    std::fs::write(state_dir.join(STATE_FILE), b"not a Bot database").expect("write old state");

    let error = match BotStore::open(&state_dir) {
        Ok(_) => panic!("old state must fail"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("Bot storage"));
}

#[test]
fn persisted_state_requires_the_default_mobius_bot() {
    let root = tempfile::tempdir().expect("root");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&state_dir).expect("state");
    let store = BotStore::open(&state_dir).expect("fresh store");
    store.save(&BotState::default()).expect("write state");
    drop(store);

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
