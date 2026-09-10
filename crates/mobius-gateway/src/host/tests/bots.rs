use std::sync::atomic::AtomicBool;

use tokio::sync::{broadcast, mpsc};

use crate::host::deletion::prepare_bot_session_tree_deletion;

use super::super::session::{HostCommand, HostInner};
use super::*;

async fn gateway_with_bot() -> (tempfile::TempDir, GatewayHost, crate::wire::BotRecord) {
    let root = tempfile::tempdir().expect("root");
    let (store, config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen"),
        None,
    )
    .expect("config");
    let mut composition = AgentComposition::default();
    composition.middleware.set_setting(
        "bots",
        "collaboration",
        Some(mobius::protocol::FrontendSettingValue::String(
            "swarm".into(),
        )),
    );
    let mut config = config
        .registering_provider(
            composition.provider.clone(),
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("provider");
    config.bot_defaults.as_mut().expect("defaults").config = composition.clone();
    store.save(&config).expect("save config");
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    bots.seed_default(config.bot_defaults.as_ref().expect("Bot defaults"))
        .expect("seed Mobius Bot");
    let bot = bots
        .create_bot("reviewer", "Reviewer", composition)
        .expect("Bot");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    (root, gateway, bot)
}

#[tokio::test]
async fn saving_bot_prepares_once_and_chats_bind_it_when_next_used() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let host = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let bots = Arc::clone(&gateway.state.lock().await.bots);
    let prepared = Arc::clone(bots.prepared.lock().await.get(&bot.id).unwrap());
    let sibling = gateway.create_session(&workspace, &bot.id).await.unwrap();
    assert!(Arc::ptr_eq(
        &prepared,
        bots.prepared.lock().await.get(&bot.id).unwrap()
    ));
    let mut config = bot.config.config.clone();
    let attachments = !host.accepts_file_attachments().await.unwrap();
    config.middleware.set_enabled("attachments", attachments);
    config.system_prompt = "Updated Bot instructions".into();
    let before = host.snapshot(None).await.unwrap();
    let mut events = host.subscribe();
    let tint = crate::wire::ProviderTint::Purple;
    assert_ne!(bot.tint, tint);
    let (commands, mut blocked_commands) = mpsc::channel(1);
    let (blocked_events, _) = broadcast::channel(1);
    let blocked = HostHandle {
        inner: Arc::new(HostInner {
            session_id: Arc::from("unresponsive-chat"),
            bot_id: Arc::from(bot.id.as_str()),
            commands,
            events: blocked_events,
            alive: Arc::new(AtomicBool::new(true)),
            terminated: Arc::new(AtomicBool::new(true)),
            termination: Arc::new(tokio::sync::Notify::new()),
            session_mutations: Arc::new(tokio::sync::RwLock::new(())),
            realtime_voice: Arc::new(tokio::sync::Mutex::new(())),
        }),
    };
    gateway
        .state
        .lock()
        .await
        .sessions
        .insert("unresponsive-chat".into(), blocked);
    let updated = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        gateway.update_bot(
            &bot.id,
            bot.config.revision,
            "Renamed",
            &bot.description,
            tint,
            config,
        ),
    )
    .await
    .expect("Bot save must never wait for a chat actor")
    .unwrap();
    assert!(matches!(
        blocked_commands.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    gateway
        .state
        .lock()
        .await
        .sessions
        .remove("unresponsive-chat");
    assert_eq!(updated.name, "Renamed");
    assert_eq!(updated.tint, tint);
    let after = host
        .snapshot(Some(before.ready.latest_sequence))
        .await
        .unwrap();
    assert_eq!(after.ready.latest_sequence, before.ready.latest_sequence);
    assert!(after.replay.is_empty());
    assert!(
        matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ),
        "metadata must not restart/replay the chat"
    );
    let updated_prepared = Arc::clone(bots.prepared.lock().await.get(&bot.id).unwrap());
    assert!(!Arc::ptr_eq(&prepared, &updated_prepared));
    assert_eq!(host.accepts_file_attachments().await.unwrap(), attachments);
    assert_eq!(
        sibling.accepts_file_attachments().await.unwrap(),
        attachments
    );
    assert!(Arc::ptr_eq(
        &updated_prepared,
        bots.prepared.lock().await.get(&bot.id).unwrap()
    ));
    gateway.shutdown().await;
}

#[tokio::test]
async fn session_owners_wait_for_the_cascade_gate() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let mobius = gateway
        .state
        .lock()
        .await
        .bots
        .mobius()
        .expect("Mobius Bot");
    let existing_member = gateway
        .create_bot("Existing member", "Keep the existing team valid.")
        .await
        .expect("create existing member");
    let swarm_id = gateway
        .create_swarm("Existing team".into(), mobius.id, vec![existing_member.id])
        .await
        .expect("create existing swarm")[0]
        .id
        .clone();
    let independent = gateway
        .create_bot("Independent", "Own a separate team.")
        .await
        .expect("create independent Bot");
    let independent_member = gateway
        .create_bot("Independent member", "Join the separate team.")
        .await
        .expect("create independent member");
    let deletable = gateway
        .create_session(&workspace, &bot.id)
        .await
        .expect("create deletable session");
    let deletable_id = deletable.session_id().to_owned();
    let routine_id = {
        let state = gateway.state.lock().await;
        state
            .bots
            .create_routine(
                &bot.id,
                &workspace,
                "wait for the cascade gate",
                crate::wire::RoutineSchedule {
                    kind: crate::wire::RoutineScheduleKind::Once,
                    at: Some(Utc::now().timestamp() + 60),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
                None,
            )
            .expect("create routine")
            .id
    };
    let mutation_gate = Arc::clone(&gateway.state.lock().await.session_mutations);
    let bot_store = Arc::clone(&gateway.state.lock().await.bots);
    let writer = mutation_gate.write_owned().await;

    assert_eq!(
        deletable
            .begin_session_file_mutation(&bot_store)
            .expect_err("upload must not queue behind a cascade")
            .code,
        "gateway_busy"
    );

    let mut creating_session = tokio::spawn({
        let gateway = gateway.clone();
        let workspace = workspace.clone();
        let bot_id = bot.id.clone();
        async move { gateway.create_session(&workspace, &bot_id).await }
    });
    let mut joining_swarm = tokio::spawn({
        let gateway = gateway.clone();
        let bot_id = bot.id.clone();
        async move { gateway.add_swarm_member(&swarm_id, bot_id).await }
    });
    let mut creating_swarm = tokio::spawn({
        let gateway = gateway.clone();
        async move {
            gateway
                .create_swarm(
                    "Independent team".into(),
                    independent.id,
                    vec![independent_member.id],
                )
                .await
        }
    });
    let mut deleting_session = tokio::spawn({
        let gateway = gateway.clone();
        let session_id = deletable_id.clone();
        async move { gateway.delete_sessions(&[session_id]).await }
    });
    let mut running_routine = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.run_routine(routine_id).await }
    });

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut creating_session,)
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut joining_swarm,)
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut creating_swarm,)
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut deleting_session,)
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut running_routine,)
            .await
            .is_err()
    );

    drop(writer);
    creating_session
        .await
        .expect("session task")
        .expect("create session");
    joining_swarm.await.expect("join task").expect("join swarm");
    creating_swarm
        .await
        .expect("swarm task")
        .expect("create swarm");
    deleting_session
        .await
        .expect("delete task")
        .expect("delete session");
    running_routine
        .await
        .expect("routine task")
        .expect("run routine");
    assert_eq!(
        deletable
            .begin_session_file_mutation(&bot_store)
            .expect_err("deleted session must reject a stale upload")
            .code,
        "gateway_stopped"
    );
}

#[tokio::test]
async fn routine_sessions_stay_hidden_and_swarm_membership_does_not_dispatch_results() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let swarm = Arc::clone(&gateway.state.lock().await.swarm);
    let leader = gateway.state.lock().await.bots.mobius().expect("leader");
    swarm
        .create(
            "Routine team".into(),
            leader.id.clone(),
            vec![leader.id, bot.id.clone()],
        )
        .await
        .expect("swarm");
    let routine = {
        let state = gateway.state.lock().await;
        state
            .bots
            .create_routine(
                &bot.id,
                &workspace,
                "test routine",
                crate::wire::RoutineSchedule {
                    kind: crate::wire::RoutineScheduleKind::Once,
                    at: Some(Utc::now().timestamp() + 60),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
                None,
            )
            .expect("routine")
    };

    gateway
        .run_routine(routine.id.clone())
        .await
        .expect("run routine");
    let (checkpoints, session_id) = {
        let state = gateway.state.lock().await;
        let session_id = state.bots.history(Some(&routine.id)).expect("history")[0]
            .session_id
            .clone()
            .expect("routine session");
        (Arc::clone(&state.checkpoints), session_id)
    };

    assert!(
        !checkpoints
            .load(&session_id)
            .await
            .expect("load routine session")
            .expect("routine checkpoint")
            .catalog_visible
    );
    assert!(
        gateway
            .sessions()
            .await
            .expect("Bot conversations")
            .iter()
            .all(|session| session.session_id != session_id)
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if gateway
                .state
                .lock()
                .await
                .bots
                .history(Some(&routine.id))
                .expect("history")[0]
                .status
                != RoutineRunStatus::Running
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("routine completion");
    assert!(swarm.records().await.expect("board")[0].messages.is_empty());
    assert!(
        swarm
            .pending_recipient_bot_ids()
            .await
            .expect("deliveries")
            .is_empty()
    );
    assert!(
        swarm
            .pending_attentions()
            .await
            .expect("attention")
            .is_empty()
    );
}

#[tokio::test]
async fn deleting_a_completed_routine_run_removes_its_session_data() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (bots, checkpoints, files) = {
        let state = gateway.state.lock().await;
        (
            Arc::clone(&state.bots),
            Arc::clone(&state.checkpoints),
            state.session_files.clone(),
        )
    };
    let routine = bots
        .create_routine(
            &bot.id,
            &workspace,
            "prepare report",
            crate::wire::RoutineSchedule {
                kind: crate::wire::RoutineScheduleKind::Once,
                at: Some(Utc::now().timestamp() + 60),
                every_seconds: None,
                expression: None,
                time_zone: None,
            },
            None,
        )
        .expect("routine");
    let BeginRun::Started(active) = bots.begin_run(&routine.id).expect("begin run") else {
        panic!("run must start");
    };
    let session_id = active.session_id().to_owned();
    let mut checkpoint = Checkpoint::empty(&session_id);
    checkpoint.session_context.bot_id = bot.id;
    checkpoint.catalog_visible = false;
    checkpoints
        .save(&checkpoint, &[], None)
        .await
        .expect("save routine session");
    files
        .publish_artifact(
            &session_id,
            "report.txt".into(),
            "text/plain".into(),
            b"report",
        )
        .await
        .expect("publish routine artifact");
    bots.finish_run(active, RoutineRunStatus::Succeeded, None)
        .expect("finish run");
    let run_id = bots.history(Some(&routine.id)).expect("history")[0]
        .id
        .clone();

    gateway
        .delete_routine_run(&run_id)
        .await
        .expect("delete routine run");

    assert!(bots.run(&run_id).is_err());
    assert!(
        checkpoints
            .load(&session_id)
            .await
            .expect("load deleted session")
            .is_none()
    );
    assert!(
        files
            .list_artifacts(&session_id)
            .await
            .expect("deleted artifacts")
            .is_empty()
    );
}

#[tokio::test]
async fn bot_delete_preflight_preserves_sessions_when_instructions_are_invalid() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let chat = gateway
        .create_session(&workspace, &bot.id)
        .await
        .expect("create Bot chat");
    let chat_id = chat.session_id().to_owned();
    let (bots, checkpoints) = {
        let state = gateway.state.lock().await;
        (Arc::clone(&state.bots), Arc::clone(&state.checkpoints))
    };
    let routine = bots
        .create_routine(
            &bot.id,
            &workspace,
            "retain on failed preflight",
            crate::wire::RoutineSchedule {
                kind: crate::wire::RoutineScheduleKind::Once,
                at: Some(Utc::now().timestamp() + 60),
                every_seconds: None,
                expression: None,
                time_zone: None,
            },
            None,
        )
        .expect("create routine");
    std::fs::remove_file(&routine.instructions).expect("remove instructions");
    std::fs::create_dir(&routine.instructions).expect("replace instructions with directory");

    let rejection = gateway
        .delete_bot(&bot.id, bot.config.revision)
        .await
        .expect_err("invalid instructions reject cascade");

    assert_eq!(rejection.code, "invalid_bot");
    assert!(bots.bot(&bot.id).is_ok());
    assert!(bots.routine(&routine.id).is_ok());
    assert!(
        checkpoints
            .load(&chat_id)
            .await
            .expect("load preserved chat")
            .is_some()
    );
}

#[tokio::test]
async fn bot_delete_rejects_an_active_upload_before_any_owner_commit() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let chat = gateway
        .create_session(&workspace, &bot.id)
        .await
        .expect("create Bot chat");
    let chat_id = chat.session_id().to_owned();
    let (bots, checkpoints, files) = {
        let state = gateway.state.lock().await;
        (
            Arc::clone(&state.bots),
            Arc::clone(&state.checkpoints),
            state.session_files.clone(),
        )
    };
    let upload = files
        .begin_upload(&chat_id, "pending.txt".into(), 1, "text/plain".into())
        .await
        .expect("begin upload");

    let rejection = gateway
        .delete_bot(&bot.id, bot.config.revision)
        .await
        .expect_err("active upload rejects cascade");

    assert!(rejection.message.contains("upload is active"));
    assert!(bots.bot(&bot.id).is_ok());
    assert!(
        checkpoints
            .load(&chat_id)
            .await
            .expect("load preserved chat")
            .is_some()
    );

    drop(upload);
    gateway
        .delete_bot(&bot.id, bot.config.revision)
        .await
        .expect("delete after upload release");
    assert!(bots.bot(&bot.id).is_err());
    assert!(
        checkpoints
            .load(&chat_id)
            .await
            .expect("load deleted chat")
            .is_none()
    );
}

#[tokio::test]
async fn startup_finishes_a_bot_cascade_after_the_swarm_commit() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let chat = gateway
        .create_session(&workspace, &bot.id)
        .await
        .expect("create Bot chat");
    let chat_id = chat.session_id().to_owned();
    let mobius = gateway
        .state
        .lock()
        .await
        .bots
        .mobius()
        .expect("Mobius Bot");
    let swarm_id = gateway
        .create_swarm("Crash-safe team".into(), bot.id.clone(), vec![mobius.id])
        .await
        .expect("create swarm")[0]
        .id
        .clone();
    let (deletion, file_deletion, bot_store, swarm_store) = {
        let mut state = gateway.state.lock().await;
        state
            .scratchpad
            .add_swarm(&swarm_id, "retain until the cascade commits")
            .await
            .expect("collective note");
        let (roots, ids, file_deletion) = prepare_bot_session_tree_deletion(&mut state, &bot.id)
            .await
            .expect("prepare files");
        let bot_store = Arc::clone(&state.bots);
        let swarm_store = Arc::clone(&state.swarm);
        let planned = swarm_store
            .planned_bot_removal(&bot.id)
            .await
            .expect("plan Swarm removal")
            .expect("Bot swarm");
        let mut deletion = bot_store
            .prepare_bot_deletion(&bot.id, bot.config.revision)
            .expect("prepare Bot deletion");
        bot_store
            .record_bot_deletion(
                &mut deletion,
                &roots,
                &ids,
                Some((&planned.swarm_id, planned.disbanded)),
            )
            .expect("record deletion intent");
        drop(state);
        swarm_store
            .remove_bot(&bot.id)
            .await
            .expect("persist Swarm removal");
        (deletion, file_deletion, bot_store, swarm_store)
    };
    assert!(bot_store.bot(&bot.id).is_ok());
    assert!(
        swarm_store
            .records()
            .await
            .expect("removed swarm")
            .is_empty()
    );

    drop(deletion);
    drop(file_deletion);
    drop(bot_store);
    drop(swarm_store);
    drop(gateway);

    let (store, config) = ConfigStore::open(root.path().join("state")).expect("reopen config");
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("reopen Bots"));
    assert!(bots.bot(&bot.id).is_ok());
    assert!(bots.pending_bot_deletion().expect("intent").is_some());
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let recovered = GatewayHost::start(store, config, credentials, Arc::clone(&bots))
        .await
        .expect("restart gateway");
    recovered.ready().await.expect("recover before ready");

    assert!(bots.bot(&bot.id).is_err());
    assert!(
        bots.pending_bot_deletion()
            .expect("cleared intent")
            .is_none()
    );
    let state = recovered.state.lock().await;
    assert!(state.swarm.records().await.expect("swarms").is_empty());
    assert!(
        state
            .checkpoints
            .load(&chat_id)
            .await
            .expect("load deleted chat")
            .is_none()
    );
    let collective = state
        .scratchpad
        .swarm_contribution(&swarm_id)
        .await
        .expect("cleared collective scratchpad");
    assert!(matches!(
        &collective.widgets[0].content,
        Some(mobius::protocol::FrontendWidgetContent::ActionList { items, .. }) if items.is_empty()
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn startup_rejects_an_unrecoverable_bot_cascade_before_serving() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let chat = gateway
        .create_session(&workspace, &bot.id)
        .await
        .expect("create Bot chat");
    let chat_id = chat.session_id().to_owned();
    let (mobius, bot_store) = {
        let state = gateway.state.lock().await;
        (
            state.bots.mobius().expect("Mobius Bot"),
            Arc::clone(&state.bots),
        )
    };
    let unrelated = gateway
        .create_session(&workspace, &mobius.id)
        .await
        .expect("create unrelated chat");
    gateway
        .state
        .lock()
        .await
        .session_files
        .publish_artifact(&chat_id, "proof.txt".into(), "text/plain".into(), b"proof")
        .await
        .expect("publish session artifact");
    {
        let mut state = gateway.state.lock().await;
        let (roots, ids, file_deletion) = prepare_bot_session_tree_deletion(&mut state, &bot.id)
            .await
            .expect("prepare sessions");
        assert!(ids.contains(&chat_id));
        let mut deletion = state
            .bots
            .prepare_bot_deletion(&bot.id, bot.config.revision)
            .expect("prepare Bot deletion");
        state
            .bots
            .record_bot_deletion(&mut deletion, &roots, &ids, None)
            .expect("record deletion intent");
        drop(deletion);
        drop(file_deletion);
    }
    let rejection = match gateway.create_session(&workspace, &bot.id).await {
        Ok(_) => panic!("pending recovery must block session creation"),
        Err(rejection) => rejection,
    };
    assert_eq!(rejection.code, "bot_deletion_recovery");
    assert_eq!(
        gateway
            .sessions()
            .await
            .expect_err("pending recovery blocks the session catalog")
            .code,
        "bot_deletion_recovery"
    );
    assert_eq!(
        gateway
            .bots()
            .await
            .expect_err("pending recovery blocks the Bot catalog")
            .code,
        "bot_deletion_recovery"
    );
    assert_eq!(
        gateway
            .hidden_bot_sessions(&mobius.id)
            .await
            .expect_err("pending recovery blocks hidden Bot sessions")
            .code,
        "bot_deletion_recovery"
    );
    assert_eq!(
        gateway
            .profile(false)
            .await
            .expect_err("pending recovery blocks the profile")
            .code,
        "bot_deletion_recovery"
    );
    assert_eq!(
        unrelated
            .submit(Submission {
                id: Uuid::new_v4().to_string(),
                op: Op::CapabilityCommand {
                    capability: "scratchpad".into(),
                    command: "scratchpad".into(),
                    arguments: "refresh".into(),
                    input: None,
                    target: None,
                },
            })
            .await
            .expect_err("pending recovery blocks accepted commands")
            .code,
        "bot_deletion_recovery"
    );
    assert_eq!(
        unrelated
            .begin_session_file_mutation(&bot_store)
            .expect_err("pending recovery blocks uploads")
            .code,
        "bot_deletion_recovery"
    );
    assert_eq!(
        gateway
            .create_swarm("Blocked".into(), mobius.id, vec![])
            .await
            .expect_err("pending recovery blocks Swarm mutation")
            .code,
        "bot_deletion_recovery"
    );
    drop(chat);
    drop(unrelated);
    drop(gateway);

    let session_files = root.path().join("state/session-files");
    let digest = <sha2::Sha256 as sha2::Digest>::digest(chat_id.as_bytes());
    let session_dir = session_files.join(base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        digest,
    ));
    std::fs::remove_dir_all(&session_dir).expect("replace session directory");
    let outside = tempfile::tempdir().expect("outside");
    std::os::unix::fs::symlink(outside.path(), session_dir).expect("unsafe session link");

    let (store, config) = ConfigStore::open(root.path().join("state")).expect("reopen config");
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("reopen Bots"));
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let error = match GatewayHost::start(store, config, credentials, Arc::clone(&bots)).await {
        Ok(_) => panic!("unrecoverable startup must not serve"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("protected directory"));
    assert!(bots.bot(&bot.id).is_ok());
    assert!(bots.pending_bot_deletion().expect("intent").is_some());
}

#[tokio::test]
async fn deleting_a_bot_removes_all_owned_state_and_its_led_swarm() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let chat = gateway
        .create_session(&workspace, &bot.id)
        .await
        .expect("create Bot chat");
    let chat_id = chat.session_id().to_owned();
    let (bots, checkpoints, session_files, scratchpad, mobius) = {
        let state = gateway.state.lock().await;
        (
            Arc::clone(&state.bots),
            Arc::clone(&state.checkpoints),
            state.session_files.clone(),
            state.scratchpad.clone(),
            state.bots.mobius().expect("Mobius Bot"),
        )
    };
    let parent = checkpoints
        .load(&chat_id)
        .await
        .expect("load chat")
        .expect("chat checkpoint");
    let child_id = Uuid::new_v4().to_string();
    let mut child = Checkpoint::empty(&child_id);
    child.session_context = parent.session_context.clone();
    checkpoints
        .fork(&chat_id, parent.sequence, &child)
        .await
        .expect("fork Bot chat");
    checkpoints
        .save_state(&chat_id, "tasks.v1", &serde_json::json!([]))
        .await
        .expect("save chat scratchpad");
    for session_id in [&chat_id, &child_id] {
        session_files
            .publish_artifact(
                session_id,
                "owned.txt".into(),
                "text/plain".into(),
                b"owned",
            )
            .await
            .expect("publish owned artifact");
    }
    gateway
        .rename_session(&chat_id, "Delete with Bot")
        .await
        .expect("rename chat");
    gateway
        .state
        .lock()
        .await
        .activities
        .lock()
        .await
        .activities
        .insert(chat_id.clone(), SessionActivity::default());

    let routine = bots
        .create_routine(
            &bot.id,
            &workspace,
            "delete routine state",
            crate::wire::RoutineSchedule {
                kind: crate::wire::RoutineScheduleKind::Once,
                at: Some(Utc::now().timestamp() + 60),
                every_seconds: None,
                expression: None,
                time_zone: None,
            },
            None,
        )
        .expect("create routine");
    let BeginRun::Started(run) = bots.begin_run(&routine.id).expect("begin routine") else {
        panic!("routine must start");
    };
    let routine_session_id = run.session_id().to_owned();
    let mut routine_checkpoint = Checkpoint::empty(&routine_session_id);
    routine_checkpoint.session_context = parent.session_context;
    routine_checkpoint.catalog_visible = false;
    checkpoints
        .save(&routine_checkpoint, &[], None)
        .await
        .expect("save hidden routine session");
    session_files
        .publish_artifact(
            &routine_session_id,
            "routine.txt".into(),
            "text/plain".into(),
            b"routine",
        )
        .await
        .expect("publish routine artifact");
    bots.finish_run(run, RoutineRunStatus::Succeeded, None)
        .expect("finish routine");

    let swarm_id = gateway
        .create_swarm("Disposable team".into(), bot.id.clone(), vec![mobius.id])
        .await
        .expect("create led swarm")[0]
        .id
        .clone();
    scratchpad
        .add_swarm(&swarm_id, "collective context")
        .await
        .expect("add collective note");

    let (remaining, deleted_sessions) = gateway
        .delete_bot(&bot.id, bot.config.revision)
        .await
        .expect("delete Bot and owned state");

    assert!(remaining.iter().all(|candidate| candidate.id != bot.id));
    assert_eq!(
        deleted_sessions.into_iter().collect::<HashSet<_>>(),
        HashSet::from([
            chat_id.clone(),
            child_id.clone(),
            routine_session_id.clone()
        ])
    );
    assert!(bots.bot(&bot.id).is_err());
    assert!(bots.routine(&routine.id).is_err());
    assert!(bots.history(None).expect("routine history").is_empty());
    assert!(!routine.instructions.exists());
    for session_id in [&chat_id, &child_id, &routine_session_id] {
        assert!(
            checkpoints
                .load(session_id)
                .await
                .expect("load deleted session")
                .is_none()
        );
        assert!(
            session_files
                .list_artifacts(session_id)
                .await
                .expect("deleted artifacts")
                .is_empty()
        );
    }
    assert!(
        checkpoints
            .load_state(&chat_id, "tasks.v1")
            .await
            .expect("load deleted scratchpad")
            .is_none()
    );
    assert!(
        !load_session_metadata(&checkpoints)
            .await
            .expect("session metadata")
            .contains_key(&chat_id)
    );
    assert!(
        !gateway
            .state
            .lock()
            .await
            .activities
            .lock()
            .await
            .activities
            .contains_key(&chat_id)
    );
    assert!(
        gateway
            .state
            .lock()
            .await
            .swarm
            .records()
            .await
            .expect("swarms")
            .is_empty()
    );
    let collective = scratchpad
        .swarm_contribution(&swarm_id)
        .await
        .expect("cleared collective scratchpad");
    assert!(matches!(
        &collective.widgets[0].content,
        Some(mobius::protocol::FrontendWidgetContent::ActionList { items, .. }) if items.is_empty()
    ));
}

#[tokio::test]
async fn deleting_a_nonleader_bot_preserves_the_swarm() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let mobius = gateway
        .state
        .lock()
        .await
        .bots
        .mobius()
        .expect("Mobius Bot");
    let swarm_id = gateway
        .create_swarm(
            "Persistent team".into(),
            mobius.id.clone(),
            vec![bot.id.clone()],
        )
        .await
        .expect("create swarm")[0]
        .id
        .clone();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let chat = gateway
        .create_session(&workspace, &bot.id)
        .await
        .expect("member chat");
    let leader_chat = gateway
        .create_session(&workspace, &mobius.id)
        .await
        .expect("leader chat");
    gateway
        .state
        .lock()
        .await
        .swarm
        .post(
            &bot.id,
            chat.session_id(),
            "This requires @user attention".into(),
            None,
        )
        .await
        .expect("pending member work");
    gateway
        .state
        .lock()
        .await
        .swarm
        .post(
            &mobius.id,
            leader_chat.session_id(),
            format!("@{} review before deletion", bot.handle),
            None,
        )
        .await
        .expect("pending delivery to deleted Bot");

    gateway
        .delete_bot(&bot.id, bot.config.revision)
        .await
        .expect("delete member Bot");

    let swarms = gateway
        .state
        .lock()
        .await
        .swarm
        .records()
        .await
        .expect("remaining swarm");
    assert_eq!(swarms[0].id, swarm_id);
    assert_eq!(swarms[0].leader_bot_id, mobius.id);
    assert_eq!(swarms[0].members.len(), 1);
    assert_eq!(swarms[0].messages.len(), 1);
    assert_eq!(swarms[0].messages[0].author_bot_id, mobius.id);
    let state = gateway.state.lock().await;
    assert!(
        gateway_session_summaries(&state.checkpoints)
            .await
            .expect("session catalog")
            .iter()
            .all(|session| session.session_context.bot_id != bot.id)
    );
}

#[tokio::test]
async fn routine_acceptance_keeps_the_gateway_registry_locked() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (bots, routine, run) = {
        let state = gateway.state.lock().await;
        let bots = Arc::clone(&state.bots);
        let routine = bots
            .create_routine(
                &bot.id,
                &workspace,
                "test routine",
                crate::wire::RoutineSchedule {
                    kind: crate::wire::RoutineScheduleKind::Once,
                    at: Some(Utc::now().timestamp() + 60),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
                None,
            )
            .expect("routine");
        let BeginRun::Started(run) = bots.begin_run(&routine.id).expect("begin run") else {
            panic!("routine must start");
        };
        (bots, routine, run)
    };
    let (commands, mut receiver) = mpsc::channel(1);
    let (events, _) = broadcast::channel(1);
    let (received, waiting) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let actor_release = Arc::clone(&release);
    let actor_bots = Arc::clone(&bots);
    tokio::spawn(async move {
        let Some(HostCommand::RunRoutine { run, reply, .. }) = receiver.recv().await else {
            panic!("routine command");
        };
        let _ = received.send(());
        actor_release.notified().await;
        actor_bots
            .finish_run(
                run,
                RoutineRunStatus::Failed,
                Some("fixture rejection".into()),
            )
            .expect("finish fixture run");
        let _ = reply.send(Err(Rejection {
            code: "fixture_rejection",
            message: "fixture rejection".into(),
            fatal: false,
        }));
    });
    let host = HostHandle {
        inner: Arc::new(HostInner {
            session_id: Arc::from("routine-acceptance"),
            bot_id: Arc::from(bot.id.as_str()),
            commands,
            events,
            alive: Arc::new(AtomicBool::new(true)),
            terminated: Arc::new(AtomicBool::new(true)),
            termination: Arc::new(tokio::sync::Notify::new()),
            session_mutations: Arc::new(tokio::sync::RwLock::new(())),
            realtime_voice: Arc::new(tokio::sync::Mutex::new(())),
        }),
    };
    let mut state = gateway.state.lock().await;
    let contender = {
        let gateway = gateway.clone();
        async move {
            waiting.await.expect("routine command received");
            let blocked = gateway.state.try_lock().is_err();
            release.notify_one();
            blocked
        }
    };
    let acceptance = accept_routine_while_state_locked(
        &mut state,
        &host,
        run,
        "test routine".into(),
        bots.as_ref(),
    );

    let (result, blocked) = tokio::join!(acceptance, contender);

    assert!(
        blocked,
        "gateway mutation entered before routine acceptance"
    );
    assert_eq!(
        result.expect_err("fixture rejection").code,
        "fixture_rejection"
    );
    assert_eq!(
        bots.history(Some(&routine.id)).expect("history")[0].status,
        RoutineRunStatus::Failed
    );
}

#[tokio::test]
async fn routine_command_gate_rejection_terminalizes_the_run() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let leader = gateway.state.lock().await.bots.mobius().expect("leader");
    gateway
        .create_swarm(
            "Routine team".into(),
            leader.id.clone(),
            vec![bot.id.clone()],
        )
        .await
        .expect("swarm");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let host = gateway
        .create_session(&workspace, &bot.id)
        .await
        .expect("chat");
    let (bots, gate, routine, run) = {
        let state = gateway.state.lock().await;
        let routine = state
            .bots
            .create_routine(
                &bot.id,
                &workspace,
                "test routine",
                crate::wire::RoutineSchedule {
                    kind: crate::wire::RoutineScheduleKind::Once,
                    at: Some(Utc::now().timestamp() + 60),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
                None,
            )
            .expect("routine");
        let BeginRun::Started(run) = state.bots.begin_run(&routine.id).expect("begin run") else {
            panic!("routine must start");
        };
        (
            Arc::clone(&state.bots),
            Arc::clone(&state.session_mutations),
            routine,
            run,
        )
    };
    let _mutation = gate.write_owned().await;

    let rejection = host
        .run_routine(run, "test routine".into(), bots.as_ref())
        .await
        .expect_err("mutation gate must reject the command");

    assert_eq!(rejection.code, "gateway_busy");
    assert_eq!(
        bots.history(Some(&routine.id)).expect("history")[0].status,
        RoutineRunStatus::Failed
    );
    let swarms = gateway
        .state
        .lock()
        .await
        .swarm
        .records()
        .await
        .expect("swarms");
    assert!(swarms[0].messages.is_empty());
}

#[tokio::test]
async fn due_routine_terminalizes_when_bot_deletion_recovery_is_pending() {
    let (root, gateway, deleting_bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (bots, routine, run) = {
        let state = gateway.state.lock().await;
        let worker = state.bots.mobius().expect("worker Bot");
        let routine = state
            .bots
            .create_routine(
                &worker.id,
                &workspace,
                "test due routine",
                crate::wire::RoutineSchedule {
                    kind: crate::wire::RoutineScheduleKind::Once,
                    at: Some(Utc::now().timestamp() + 60),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
                None,
            )
            .expect("routine");
        let BeginRun::Started(run) = state.bots.begin_run(&routine.id).expect("begin run") else {
            panic!("routine must start");
        };
        (Arc::clone(&state.bots), routine, run)
    };
    let mut deletion = bots
        .prepare_bot_deletion(&deleting_bot.id, deleting_bot.config.revision)
        .expect("prepare deletion");
    bots.record_bot_deletion(&mut deletion, &[], &[], None)
        .expect("record recovery intent");
    drop(deletion);

    let rejection = gateway
        .run_due_routine(routine.id.clone(), run)
        .await
        .expect_err("pending recovery must reject the run");

    assert_eq!(rejection.code, "bot_deletion_recovery");
    assert_eq!(
        bots.history(Some(&routine.id)).expect("history")[0].status,
        RoutineRunStatus::Skipped
    );
}

#[tokio::test]
async fn stopped_host_terminalizes_a_queued_unconsumed_routine() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let (bots, routine, run) = {
        let state = gateway.state.lock().await;
        let routine = state
            .bots
            .create_routine(
                &bot.id,
                &workspace,
                "queued routine",
                crate::wire::RoutineSchedule {
                    kind: crate::wire::RoutineScheduleKind::Once,
                    at: Some(Utc::now().timestamp() + 60),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
                None,
            )
            .expect("routine");
        let BeginRun::Started(run) = state.bots.begin_run(&routine.id).expect("begin run") else {
            panic!("routine must start");
        };
        (Arc::clone(&state.bots), routine, run)
    };
    let (commands, mut receiver) = mpsc::channel(1);
    let (reply, response) = tokio::sync::oneshot::channel();
    commands
        .send(HostCommand::RunRoutine {
            run,
            input: "queued routine".into(),
            reply,
        })
        .await
        .expect("queue routine command");
    receiver.close();

    let state_error =
        super::super::session::fail_queued_routine_commands(&mut receiver, bots.as_ref());

    assert!(state_error.is_none());
    assert_eq!(
        response
            .await
            .expect("routine response")
            .expect_err("stopped host rejection")
            .code,
        "gateway_stopped"
    );
    assert_eq!(
        bots.history(Some(&routine.id)).expect("history")[0].status,
        RoutineRunStatus::Failed
    );
}

#[tokio::test]
async fn enabling_computer_control_requires_runtime_before_saving_a_bot_without_sessions() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let runtime = crate::computer_runtime::managed_directory(&root.path().join("state"));
    std::fs::create_dir_all(&runtime).expect("runtime directory");
    std::fs::write(runtime.join("ready"), "incomplete").expect("broken installation");
    let mut config = bot.config.config.clone();
    config.middleware.set_enabled("computer_control", true);
    let failure = gateway
        .update_bot(
            &bot.id,
            bot.config.revision,
            &bot.name,
            &bot.description,
            bot.tint,
            config,
        )
        .await
        .expect_err("installation failure");
    assert!(
        failure.message.contains("missing node"),
        "{}",
        failure.message
    );
    let current = gateway
        .bots()
        .await
        .expect("bots")
        .into_iter()
        .find(|item| item.id == bot.id)
        .expect("bot");
    assert_eq!(current.config, bot.config);
}

#[tokio::test]
async fn bot_presentation_edits_do_not_prepare_or_reassemble_chats() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let host = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let operations = Arc::clone(&gateway.state.lock().await.store.runtime_operations);
    let before = operations.counts();
    let before_sequence = host.snapshot(None).await.unwrap().ready.latest_sequence;
    let mut current = bot;
    for (name, tint) in [
        ("reviewer", crate::wire::ProviderTint::Purple),
        ("renamed", crate::wire::ProviderTint::Purple),
    ] {
        current = gateway
            .update_bot(
                &current.id,
                current.config.revision,
                name,
                &current.description,
                tint,
                current.config.config.clone(),
            )
            .await
            .unwrap();
        host.accepts_file_attachments().await.unwrap();
    }
    gateway
        .create_session(&workspace, &current.id)
        .await
        .unwrap();
    assert_eq!(
        operations.counts(),
        (before.0, before.1 + 1),
        "only the newly created chat needs assembly"
    );
    assert_eq!(
        host.snapshot(None).await.unwrap().ready.latest_sequence,
        before_sequence
    );
    gateway.shutdown().await;
}
