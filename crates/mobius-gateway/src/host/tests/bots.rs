use crate::host::deletion::prepare_bot_session_tree_deletion;

use super::*;

pub(super) async fn gateway_with_bot() -> (tempfile::TempDir, GatewayHost, crate::wire::BotRecord) {
    let root = tempfile::tempdir().expect("root");
    let (store, config) = ConfigStore::initialize(
        root.path().join("state"),
        "127.0.0.1:8741".parse().expect("listen"),
        None,
    )
    .expect("config");
    let composition = AgentComposition::default();
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
    store
        .save(&config.clone().validated().expect("valid config"))
        .expect("save config");
    let bots = Arc::new(BotStore::open(store.state_dir()).expect("Bots"));
    bots.seed_default(config.bot_defaults.as_ref().expect("Bot defaults"))
        .expect("seed Mobius Bot");
    let bot = bots
        .create_bot("reviewer", "Reviewer", composition.into())
        .expect("Bot");
    let credentials =
        Arc::new(CredentialStore::open(store.credentials_path()).expect("credentials"));
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .expect("gateway");
    (root, gateway, bot)
}

#[tokio::test]
async fn reassigning_chat_preserves_history_and_binds_the_target_bot() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let (bots, checkpoints) = {
        let state = gateway.state.lock().await;
        (Arc::clone(&state.bots), Arc::clone(&state.checkpoints))
    };
    let mut config = bot.config.config.clone();
    config.system_prompt = "Follow the target Bot instructions.".into();
    config.middleware.set_enabled("attachments", false);
    let target = bots
        .create_bot("Target", "Target description", config.into())
        .unwrap();
    for stopped in [false, true] {
        let host = gateway.create_session(&workspace, &bot.id).await.unwrap();
        let id = host.session_id().to_string();
        host.submit(Submission {
            id: Uuid::new_v4().to_string().into(),
            op: Op::Message {
                message: MessageSubmission {
                    author: MessageAuthor::User,
                    text: "Preserve this conversation".into(),
                    attachments: Vec::new(),
                    reply: None,
                    requested_delivery: None,
                    target_turn_id: None,
                },
            },
        })
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), host.wait_idle())
            .await
            .unwrap();
        gateway
            .rename_session(&id, "Preserved title")
            .await
            .unwrap();
        gateway.set_session_pinned(&id, true).await.unwrap();
        let before = checkpoints.load(&id).await.unwrap().unwrap();
        assert_eq!(
            before.first_user_message.as_deref(),
            Some("Preserve this conversation")
        );
        let history_request = mobius::backend::checkpoint::TranscriptPageRequest {
            before_sequence: None,
            max_batches: 100,
        };
        let history = checkpoints
            .transcript_page(&id, history_request.clone())
            .await
            .unwrap();
        assert!(!history.batches.is_empty());
        let voice = host.claim_realtime_voice().unwrap();
        assert_eq!(
            gateway
                .reassign_session(&id, &target.id)
                .await
                .unwrap_err()
                .code,
            "realtime_voice"
        );
        drop(voice);
        assert!(gateway.reassign_session(&id, "missing-bot").await.is_err());
        assert_eq!(host.bot_id().await.unwrap(), bot.id);
        if stopped {
            assert!(host.stop_if_idle().await);
        }
        gateway.reassign_session(&id, &target.id).await.unwrap();
        let reassigned = gateway.open_session(&id).await.unwrap();
        assert_eq!(**reassigned.session_id(), *id);
        if !stopped {
            assert!(Arc::ptr_eq(&host.inner, &reassigned.inner));
        }
        assert_eq!(reassigned.bot_id().await.unwrap(), target.id);
        assert!(!reassigned.accepts_file_attachments().await.unwrap());
        let snapshot = reassigned.snapshot(None).await.unwrap();
        assert_eq!(snapshot.ready.session.context.owner_id, target.id);
        let after = checkpoints.load(&id).await.unwrap().unwrap();
        assert_eq!(after.context, before.context);
        assert_eq!(after.first_user_message, before.first_user_message);
        assert_eq!(after.execution_stats, before.execution_stats);
        assert_eq!(
            after.session_context.workspace_id,
            before.session_context.workspace_id
        );
        assert_eq!(
            checkpoints
                .transcript_page(&id, history_request)
                .await
                .unwrap(),
            history
        );
        let spec =
            ChatSpec::from_metadata(&after.metadata, &bots, &root.path().join("state"), None)
                .unwrap();
        assert_eq!(spec.bot_id, target.id);
        assert_eq!(spec.workspace, Some(workspace.canonicalize().unwrap()));
        let catalog = gateway.sessions().await.unwrap();
        let record = catalog
            .iter()
            .find(|record| *record.session_id == *id)
            .unwrap();
        assert_eq!(record.title.as_deref(), Some("Preserved title"));
        assert!(record.pinned);
        assert_eq!(record.session_context.owner_id, target.id);
        assert!(reassigned.stop_if_idle().await);
        assert_eq!(
            gateway
                .open_session(&id)
                .await
                .unwrap()
                .bot_id()
                .await
                .unwrap(),
            target.id
        );
    }
    gateway.shutdown().await;
}

#[tokio::test]
async fn failed_reassignment_restores_the_owner_and_running_chat() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let host = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let (bots, checkpoints) = {
        let state = gateway.state.lock().await;
        (Arc::clone(&state.bots), Arc::clone(&state.checkpoints))
    };
    let before = checkpoints.load(host.session_id()).await.unwrap().unwrap();
    let mut config = bot.config.config.clone();
    config.middleware.set_enabled("instructions", true);
    let target = bots
        .create_bot("Instructions", "Read workspace instructions", config.into())
        .unwrap();
    // Only the target Bot loads this file, so assembly fails after ownership is saved.
    std::fs::write(workspace.join("AGENTS.md"), [0xff]).unwrap();
    gateway
        .reassign_session(host.session_id(), &target.id)
        .await
        .unwrap_err();
    let after = checkpoints.load(host.session_id()).await.unwrap().unwrap();
    assert_eq!(after.session_context, before.session_context);
    assert_eq!(after.metadata, before.metadata);
    assert_eq!(host.bot_id().await.unwrap(), bot.id);
    assert_eq!(
        host.snapshot(None)
            .await
            .unwrap()
            .ready
            .session
            .context
            .owner_id,
        bot.id
    );
    std::fs::remove_file(workspace.join("AGENTS.md")).unwrap();
    gateway
        .reassign_session(host.session_id(), &target.id)
        .await
        .unwrap();
    assert_eq!(host.bot_id().await.unwrap(), target.id);
    gateway.shutdown().await;
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
    let chat_id = chat.session_id().to_string();
    let (bots, checkpoints) = {
        let state = gateway.state.lock().await;
        (Arc::clone(&state.bots), Arc::clone(&state.checkpoints))
    };
    let routine = bots
        .create_routine(
            &bot.id,
            &timer_definition(
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
            ),
            crate::config::CanonicalWorkspace::for_test(&workspace),
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
async fn startup_finishes_a_bot_cascade() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let chat = gateway
        .create_session(&workspace, &bot.id)
        .await
        .expect("create Bot chat");
    let chat_id = chat.session_id().to_string();
    let (deletion, file_deletion, bot_store) = {
        let state = gateway.state.lock().await;
        let (roots, ids, file_deletion) = prepare_bot_session_tree_deletion(&state, &bot.id)
            .await
            .expect("prepare files");
        let bot_store = Arc::clone(&state.bots);
        drop(state);
        let mut deletion = bot_store
            .prepare_bot_deletion(&bot.id, bot.config.revision)
            .expect("prepare Bot deletion");
        bot_store
            .record_bot_deletion(&mut deletion, &roots, &ids)
            .expect("record deletion intent");
        (deletion, file_deletion, bot_store)
    };
    assert!(bot_store.bot(&bot.id).is_ok());

    drop(deletion);
    drop(file_deletion);
    drop(bot_store);
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
    let checkpoints = Arc::clone(&recovered.state.lock().await.checkpoints);
    drop(recovered);
    assert!(
        checkpoints
            .load(&chat_id)
            .await
            .expect("load deleted chat")
            .is_none()
    );
}

#[tokio::test]
async fn deleting_a_bot_removes_owned_state() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let chat = gateway
        .create_session(&workspace, &bot.id)
        .await
        .expect("create Bot chat");
    let chat_id = chat.session_id().to_string();
    let (bots, checkpoints, session_files) = {
        let state = gateway.state.lock().await;
        (
            Arc::clone(&state.bots),
            Arc::clone(&state.checkpoints),
            Arc::clone(&state.session_files),
        )
    };
    let parent = checkpoints
        .load(&chat_id)
        .await
        .expect("load chat")
        .expect("chat checkpoint");
    let child_id = Uuid::new_v4().to_string();
    let mut child = Checkpoint::empty(child_id.as_str());
    child.session_context = Arc::clone(&parent.session_context);
    checkpoints
        .fork(&chat_id, parent.sequence, child.clone())
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
        .insert(chat_id.as_str().into(), SessionActivity::default());

    let routine = bots
        .create_routine(
            &bot.id,
            &timer_definition(
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
            ),
            crate::config::CanonicalWorkspace::for_test(&workspace),
            None,
        )
        .expect("create routine");
    let BeginRun::Started(run) = bots.begin_run(&routine.id).expect("begin routine") else {
        panic!("routine must start");
    };
    let routine_session_id = run.session_id().to_owned();
    let mut routine_checkpoint = Checkpoint::empty(routine_session_id.as_str());
    routine_checkpoint.session_context = parent.session_context;
    routine_checkpoint.catalog_visible = false;
    checkpoints
        .save(routine_checkpoint.clone(), Vec::new(), None)
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
                .list_files(
                    session_id,
                    &[mobius::backend::session_files::SessionFileOrigin::Artifact]
                )
                .await
                .map(|files| files.into_iter().map(|(_, file)| file).collect::<Vec<_>>())
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
            .contains_key(chat_id.as_str())
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
            .contains_key(chat_id.as_str())
    );
}
