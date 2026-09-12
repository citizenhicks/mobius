use super::*;

pub(super) async fn gateway_with_group() -> (
    tempfile::TempDir,
    GatewayHost,
    PathBuf,
    HostHandle,
    Vec<crate::wire::BotRecord>,
) {
    let (root, gateway, bot) = bots::gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let member = gateway.state.lock().await.bots.mobius().unwrap();
    let members = vec![bot, member];
    let chat = gateway
        .create_chat(
            &workspace,
            &members.iter().map(|bot| bot.id.clone()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    (root, gateway, workspace, chat, members)
}

pub(super) fn message(id: impl Into<String>, text: impl Into<String>) -> Submission {
    Submission {
        id: id.into(),
        op: Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::User,
                text: text.into(),
                attachments: Vec::new(),
                reply: None,
                requested_delivery: None,
                target_turn_id: None,
            },
        },
    }
}

#[tokio::test]
async fn groups_use_the_session_catalog_and_unmentioned_posts_do_not_start_an_agent() {
    let (_root, gateway, workspace, chat, bots) = gateway_with_group().await;
    let snapshot = chat.snapshot(None).await.unwrap();
    assert_eq!(
        snapshot.ready.member_bot_ids,
        Some(bots.iter().map(|bot| bot.id.clone()).collect())
    );
    assert_eq!(
        snapshot.ready.workspace.path,
        workspace.canonicalize().unwrap()
    );
    assert!(snapshot.ready.session.context.bot_id.is_empty());
    assert_eq!(snapshot.ready.tool_count, 0);
    assert!(snapshot.ready.contributions.is_empty());
    chat.submit(message("quiet", "Notes for everyone"))
        .await
        .unwrap();
    let state = gateway.state.lock().await;
    assert_eq!(state.sessions.len(), 1);
    assert!(
        gateway_session_summaries(&state.checkpoints)
            .await
            .unwrap()
            .is_empty(),
        "creating and posting to an unmentioned group must not create Agent checkpoints"
    );
    assert!(
        state
            .checkpoints
            .load(chat.session_id())
            .await
            .unwrap()
            .is_none()
    );
    assert!(state.group.load(chat.session_id()).await.unwrap().is_some());

    assert!(
        state.bots.prepared.lock().await.is_empty(),
        "no Bot runtime should be prepared for an unmentioned post"
    );
    assert!(
        state
            .group
            .pending_recipient_bot_ids()
            .await
            .unwrap()
            .is_empty()
    );
    drop(state);
    gateway
        .rename_session(chat.session_id(), "Project discussion")
        .await
        .unwrap();
    gateway
        .set_session_pinned(chat.session_id(), true)
        .await
        .unwrap();
    let sessions = gateway.sessions().await.unwrap();
    let record = sessions
        .iter()
        .find(|record| record.session_id == chat.session_id())
        .unwrap();
    assert_eq!(record.member_bot_ids, snapshot.ready.member_bot_ids);
    assert_eq!(record.title.as_deref(), Some("Project discussion"));
    assert!(record.pinned);
    assert!(chat.stop_if_idle().await);
    let reopened = gateway.open_session(chat.session_id()).await.unwrap();
    assert_eq!(reopened.snapshot(None).await.unwrap().replay.len(), 1);
    let direct = gateway
        .create_chat(&workspace, &[bots[0].id.clone()])
        .await
        .unwrap();
    assert_eq!(
        direct.snapshot(None).await.unwrap().ready.member_bot_ids,
        None
    );
    gateway.shutdown().await;
}

#[tokio::test]
async fn group_history_pages_and_replay_work_after_more_than_256_unmentioned_messages() {
    let (_root, gateway, _workspace, chat, _bots) = gateway_with_group().await;
    for index in 0..310 {
        chat.submit(message(
            format!("quiet-{index}"),
            format!("Message {index}"),
        ))
        .await
        .unwrap();
    }
    let current = chat.snapshot(None).await.unwrap();
    assert_eq!(current.replay.len(), 50);
    assert!(current.ready.next_before_sequence.is_some());
    assert_eq!(
        chat.snapshot(Some(1)).await.err().unwrap().code,
        "replay_unavailable"
    );
    assert!(
        chat.snapshot(Some(current.ready.latest_sequence))
            .await
            .unwrap()
            .replay
            .is_empty()
    );
    let mut cursor = None;
    let mut records = Vec::new();
    loop {
        let page = chat.history_page(cursor).await.unwrap();
        records.extend(page.records);
        match page.next_before_sequence {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(records.len(), 310);
    let submissions = records
        .iter()
        .filter_map(|record| record.event.submission_id.as_deref())
        .collect::<HashSet<_>>();
    assert_eq!(submissions.len(), 310);
    assert!(submissions.contains("quiet-0"));
    gateway.shutdown().await;
}

#[tokio::test]
async fn deleting_a_group_preserves_member_bots_routines_and_other_chats() {
    let (_root, gateway, workspace, chat, bots) = gateway_with_group().await;
    let direct = gateway
        .create_session(&workspace, &bots[0].id)
        .await
        .unwrap();
    let overlapping = gateway
        .create_chat(
            &workspace,
            &bots.iter().map(|bot| bot.id.clone()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    let bot_store = Arc::clone(&gateway.state.lock().await.bots);
    let routine = bot_store
        .create_routine(
            &bots[0].id,
            &workspace,
            "Keep this routine",
            crate::wire::RoutineSchedule {
                kind: crate::wire::RoutineScheduleKind::Once,
                at: Some(Utc::now().timestamp() + 60),
                every_seconds: None,
                expression: None,
                time_zone: None,
            },
            None,
        )
        .unwrap();
    let participant_id = crate::groups::participant_session_id(chat.session_id(), &bots[0].id);
    chat.submit(message(
        "before-delete",
        format!("@{} finish this chat", bots[0].handle),
    ))
    .await
    .unwrap();
    let groups = Arc::clone(&gateway.state.lock().await.group);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if groups.pending_recipient_bot_ids().await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("member should finish before deletion");
    assert!(
        gateway
            .state
            .lock()
            .await
            .checkpoints
            .load(&participant_id)
            .await
            .unwrap()
            .is_some()
    );
    gateway
        .delete_sessions(&[chat.session_id().to_owned()])
        .await
        .unwrap();
    assert!(
        gateway
            .state
            .lock()
            .await
            .checkpoints
            .load(&participant_id)
            .await
            .unwrap()
            .is_none()
    );
    for bot in &bots {
        assert!(bot_store.bot(&bot.id).is_ok());
    }
    assert!(bot_store.routine(&routine.id).is_ok());
    assert!(routine.instructions.exists());
    assert!(direct.snapshot(None).await.is_ok());
    assert!(overlapping.snapshot(None).await.is_ok());
    assert!(
        gateway
            .state
            .lock()
            .await
            .group
            .load(chat.session_id())
            .await
            .unwrap()
            .is_none()
    );
    gateway.shutdown().await;
}

#[tokio::test]
async fn startup_finishes_tombstoned_group_deletion_without_removing_member_bots_or_routines() {
    for executed in [false, true] {
        let (root, gateway, workspace, chat, bots) = gateway_with_group().await;
        let chat_id = chat.session_id().to_owned();
        let direct = gateway
            .create_session(&workspace, &bots[1].id)
            .await
            .unwrap();
        let direct_id = direct.session_id().to_owned();
        let participant_id = crate::groups::participant_session_id(&chat_id, &bots[0].id);
        let (groups, checkpoints, files, bot_store) = {
            let state = gateway.state.lock().await;
            (
                Arc::clone(&state.group),
                Arc::clone(&state.checkpoints),
                state.session_files.clone(),
                Arc::clone(&state.bots),
            )
        };
        let routine = bot_store
            .create_routine(
                &bots[0].id,
                &workspace,
                "Keep this after group recovery",
                crate::wire::RoutineSchedule {
                    kind: crate::wire::RoutineScheduleKind::Once,
                    at: Some(Utc::now().timestamp() + 3600),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
                None,
            )
            .unwrap();
        if executed {
            chat.submit(message(
                "before-crash",
                format!("@{} prepare work", bots[0].handle),
            ))
            .await
            .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if groups.pending_recipient_bot_ids().await.unwrap().is_empty() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("member execution should finish");
            assert!(checkpoints.load(&participant_id).await.unwrap().is_some());
            files
                .publish_artifact(
                    &participant_id,
                    "private.txt".into(),
                    "text/plain".into(),
                    b"private work",
                )
                .await
                .unwrap();
        }
        files
            .publish_artifact(
                &chat_id,
                "shared.txt".into(),
                "text/plain".into(),
                b"shared work",
            )
            .await
            .unwrap();
        gateway
            .rename_session(&chat_id, "Remove this metadata")
            .await
            .unwrap();
        gateway.shutdown().await;
        drop(chat);
        drop(direct);
        drop(gateway);
        groups
            .mark_deleted(std::slice::from_ref(&chat_id))
            .await
            .unwrap();
        assert!(groups.load(&chat_id).await.unwrap().is_none());
        assert_eq!(groups.chats(true).await.unwrap().len(), 1);
        drop(groups);
        drop(checkpoints);
        drop(bot_store);
        let (store, config) = ConfigStore::open(root.path().join("state")).unwrap();
        let bot_store = Arc::new(BotStore::open(store.state_dir()).unwrap());
        let credentials = Arc::new(CredentialStore::open(store.credentials_path()).unwrap());
        let recovered = GatewayHost::start(store, config, credentials, Arc::clone(&bot_store))
            .await
            .unwrap();
        let state = recovered.state.lock().await;
        assert!(state.group.chats(true).await.unwrap().is_empty());
        assert!(state.group.load(&chat_id).await.unwrap().is_none());
        assert!(state.checkpoints.load(&chat_id).await.unwrap().is_none());
        assert!(
            state
                .checkpoints
                .load(&participant_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(state.checkpoints.load(&direct_id).await.unwrap().is_some());
        assert!(
            !load_session_metadata(&state.checkpoints)
                .await
                .unwrap()
                .contains_key(&chat_id)
        );
        drop(state);
        for id in [&chat_id, &participant_id] {
            assert!(files.list_artifacts(id).await.unwrap().is_empty());
        }
        for bot in &bots {
            assert!(bot_store.bot(&bot.id).is_ok());
        }
        assert!(bot_store.routine(&routine.id).is_ok());
        assert!(routine.instructions.exists());
        let sessions = recovered.sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, direct_id);
        recovered.shutdown().await;
    }
}

#[tokio::test]
async fn snapshots_recover_all_current_approvals_beyond_the_replay_page() {
    use mobius::backend::checkpoint::PendingApproval;
    use mobius::backend::model::ToolCall;
    use mobius::protocol::ExecApprovalRequestEvent;

    let (_root, gateway, workspace, chat, bots) = gateway_with_group().await;
    let (groups, checkpoints) = {
        let state = gateway.state.lock().await;
        (Arc::clone(&state.group), Arc::clone(&state.checkpoints))
    };
    let mut expected = Vec::new();
    let mut private_ids = Vec::new();
    for (index, bot) in bots.iter().enumerate() {
        let private_id = crate::groups::participant_session_id(chat.session_id(), &bot.id);
        let participant = gateway
            .create_session_with_id(&workspace, &bot.id, private_id.clone(), false, "Group chat")
            .await
            .unwrap();
        assert!(participant.stop_if_idle().await);
        let mut checkpoint = checkpoints.load(&private_id).await.unwrap().unwrap();
        checkpoint.sequence += 1;
        checkpoint.active_execution = Some(ActiveExecution {
            submission_id: format!("message-{index}"),
            turn_id: format!("turn-{index}"),
            started_at_ms: 1_000,
            model_calls: 1,
            tool_calls: 0,
            failed_tool_calls: 0,
            usage: Default::default(),
            next_model_step: 1,
            stop_hook_active: false,
            phase: mobius::backend::checkpoint::ExecutionPhase::Model,
        });
        let selected = ToolCall {
            call_id: format!("selected-{index}"),
            name: "create_routine".into(),
            arguments: serde_json::json!({"instructions":"Review this project"}),
        };
        let authorized = ToolCall {
            call_id: format!("authorized-{index}"),
            name: "read_file".into(),
            arguments: serde_json::json!({"path":"README.md"}),
        };
        checkpoint.pending_approval = Some(PendingApproval {
            submission_id: format!("message-{index}"),
            turn_id: format!("turn-{index}"),
            request_id: format!("approval-{index}"),
            approval_call_ids: vec![selected.call_id.clone()],
            authorized_call_ids: vec![authorized.call_id.clone()],
            calls: vec![authorized, selected],
            reason: "Create a routine".into(),
            sandbox_mode: Default::default(),
            network_access: Default::default(),
            decision_received: false,
        });
        let request = checkpoint
            .pending_approval
            .as_ref()
            .unwrap()
            .request_event();
        assert_eq!(request.calls.len(), 1);
        assert_eq!(request.calls[0].call_id, format!("selected-{index}"));
        checkpoints.save(&checkpoint, &[], None).await.unwrap();
        groups
            .observe_event(
                &private_id,
                &Event {
                    submission_id: Some(format!("message-{index}")),
                    msg: EventMsg::ExecApprovalRequest(request.clone()),
                },
            )
            .await
            .unwrap();
        expected.push(ExecApprovalRequestEvent {
            reason: format!("@{}: Create a routine", bot.handle),
            ..request
        });
        private_ids.push(private_id);
    }
    for index in 0..60 {
        chat.submit(message(
            format!("later-{index}"),
            "Later discussion without a mention",
        ))
        .await
        .unwrap();
    }
    let snapshot = chat.snapshot(None).await.unwrap();
    assert_eq!(snapshot.replay.len(), 50);
    assert!(!snapshot.replay.iter().any(|frame| matches!(&frame.message, ServerMessage::AgentEvent { record, .. } if matches!(record.event.msg, EventMsg::ExecApprovalRequest(_)))));
    assert_eq!(snapshot.ready.active_turn_ids, ["turn-0", "turn-1"]);
    assert_eq!(snapshot.ready.pending_approvals, expected);
    let mut resolved = checkpoints.load(&private_ids[1]).await.unwrap().unwrap();
    resolved
        .pending_approval
        .as_mut()
        .unwrap()
        .decision_received = true;
    resolved.sequence += 1;
    checkpoints.save(&resolved, &[], None).await.unwrap();
    let current = chat
        .snapshot(Some(snapshot.ready.latest_sequence))
        .await
        .unwrap();
    assert!(current.replay.is_empty());
    assert_eq!(current.ready.active_turn_ids.len(), 2);
    assert_eq!(current.ready.pending_approvals, [expected.remove(0)]);
    gateway.shutdown().await;
}

#[tokio::test]
async fn startup_reservations_block_only_deletion_of_the_starting_chat_or_bot() {
    let (_root, gateway, workspace, chat, bots) = gateway_with_group().await;
    let direct = gateway
        .create_session(&workspace, &bots[0].id)
        .await
        .unwrap();
    let unrelated = gateway
        .create_chat(
            &workspace,
            &bots.iter().map(|bot| bot.id.clone()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    let starting = gateway
        .state
        .lock()
        .await
        .reserve_start(direct.session_id(), &bots[0].id)
        .unwrap();
    let error = gateway
        .delete_sessions(&[direct.session_id().to_owned()])
        .await
        .unwrap_err();
    assert_eq!(error.code, "agent_busy");
    let error = gateway
        .delete_bot(&bots[0].id, bots[0].config.revision)
        .await
        .unwrap_err();
    assert_eq!(error.code, "agent_busy");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        gateway
            .delete_sessions(&[unrelated.session_id().to_owned()])
            .await
            .unwrap();
        let ready = gateway.ready().await.unwrap();
        assert!(
            ready
                .sessions
                .iter()
                .any(|session| session.session_id == direct.session_id())
        );
        assert!(
            !ready
                .sessions
                .iter()
                .any(|session| session.session_id == unrelated.session_id())
        );
    })
    .await
    .expect("unrelated deletion and Ready must not wait for this startup");
    drop(starting);
    let participant_id = crate::groups::participant_session_id(chat.session_id(), &bots[0].id);
    let starting = gateway
        .state
        .lock()
        .await
        .reserve_start(&participant_id, &bots[0].id)
        .unwrap();
    let error = gateway
        .delete_sessions(&[chat.session_id().to_owned()])
        .await
        .unwrap_err();
    assert_eq!(error.code, "agent_busy");
    let error = gateway
        .delete_bot(&bots[0].id, bots[0].config.revision)
        .await
        .unwrap_err();
    assert_eq!(error.code, "agent_busy");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        gateway
            .delete_sessions(&[direct.session_id().to_owned()])
            .await
            .unwrap();
        gateway.ready().await.unwrap();
        gateway.open_session(chat.session_id()).await.unwrap();
    })
    .await
    .expect("a participant startup must not block unrelated work or opening its group");
    drop(starting);
    gateway
        .delete_sessions(&[chat.session_id().to_owned()])
        .await
        .unwrap();
    gateway.shutdown().await;
}
