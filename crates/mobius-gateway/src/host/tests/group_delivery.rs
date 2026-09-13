use super::group_management::{gateway_with_group, message};
use super::*;
use mobius::backend::checkpoint::{ExecutionPhase, PendingApproval};
use mobius::protocol::{ActiveMessageDelivery, MessageDelivery, MessageEvent};
use std::time::Duration;

pub(super) async fn pause_deliveries(gateway: &GatewayHost) {
    if let Some(task) = gateway.state.lock().await.chat_delivery_task.take() {
        task.abort();
        let _ = task.await;
    }
}

#[tokio::test]
async fn closing_chat_keeps_pending_delivery_for_retry_and_still_publishes_catalog() {
    let (_root, gateway, _workspace, chat, bots) = gateway_with_group().await;
    pause_deliveries(&gateway).await;
    let chat_id = chat.session_id().to_owned();
    let (commands, receiver) = mpsc::channel(1);
    drop(receiver);
    let (events, _) = broadcast::channel(1);
    let mut state = gateway.state.lock().await;
    let session_mutations = Arc::clone(&state.session_mutations);
    state.sessions.insert(
        chat_id.clone(),
        HostHandle {
            inner: Arc::new(HostInner {
                session_id: chat_id.clone().into(),
                commands,
                events,
                alive: Arc::new(AtomicBool::new(false)),
                terminated: Arc::new(AtomicBool::new(false)),
                termination: Arc::new(tokio::sync::Notify::new()),
                session_mutations,
                realtime_voice: Arc::new(tokio::sync::Mutex::new(())),
            }),
        },
    );
    let store = Arc::clone(&state.chat_store);
    drop(state);
    drop(chat);
    let Op::Message { message } = message("arrived-while-stopping", "Review this project").op
    else {
        unreachable!()
    };
    store
        .post_user(
            &chat_id,
            "arrived-while-stopping".into(),
            message,
            std::slice::from_ref(&bots[0].id),
        )
        .await
        .unwrap();
    let mut events = gateway.subscribe();
    gateway
        .handle_chat_delivery(ChatDelivery::Changed {
            chat_id: chat_id.clone(),
            records: Vec::new(),
        })
        .await;
    gateway
        .handle_chat_delivery(ChatDelivery::Pending {
            chat_id: chat_id.clone(),
        })
        .await;
    assert!(
        matches!(events.try_recv().unwrap().message, ServerMessage::Sessions { sessions, .. } if sessions.iter().any(|session| session.session_id == chat_id))
    );
    assert!(matches!(
        events.try_recv().unwrap().message,
        ServerMessage::BackgroundApprovals { .. }
    ));
    assert!(
        events.try_recv().is_err(),
        "a closing actor must not emit a delivery error"
    );
    assert_eq!(store.pending_deliveries(&chat_id).await.unwrap().len(), 1);

    gateway.state.lock().await.sessions.remove(&chat_id);
    gateway
        .handle_chat_delivery(ChatDelivery::Pending {
            chat_id: chat_id.clone(),
        })
        .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !store.pending_deliveries(&chat_id).await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("retained delivery resumes after the closing actor is removed");
    let participant_id = store
        .load(&chat_id)
        .await
        .unwrap()
        .unwrap()
        .session_id(&bots[0].id)
        .unwrap()
        .to_owned();
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    assert!(
        accepted_submission(
            checkpoints.as_ref(),
            &participant_id,
            "arrived-while-stopping"
        )
        .await
        .unwrap()
    );
    gateway.shutdown().await;
}

#[tokio::test]
async fn background_dispatch_at_capacity_does_not_evict_its_own_chat() {
    let (_root, gateway, workspace, chat, bots) = gateway_with_group().await;
    pause_deliveries(&gateway).await;
    let chat_id = chat.session_id().to_owned();
    let mut retained = Vec::new();
    for _ in 1..MAX_ACTIVE_SESSIONS {
        retained.push(
            gateway
                .create_chat(&workspace, std::slice::from_ref(&bots[0].id), None)
                .await
                .unwrap(),
        );
    }
    chat.submit(message("background", "Review this project"))
        .await
        .unwrap();
    let mut events = chat.subscribe();
    // Dispatch outlives its caller while its lazy participant waits on gateway state.
    let state = gateway.state.lock().await;
    chat.send(HostCommand::Dispatch).await.unwrap();
    drop(chat);
    drop(state);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerMessage::Error { code, message, .. } = events.recv().await.unwrap().message
            {
                assert_eq!(code, "chat_execution");
                assert!(message.contains("connected or running chats"));
                break;
            }
        }
    })
    .await
    .expect("capacity rejection must not deadlock the Chat or gateway");
    let chat = gateway.open_session(&chat_id).await.unwrap();
    assert_eq!(
        gateway
            .state
            .lock()
            .await
            .chat_store
            .pending_deliveries(&chat_id)
            .await
            .unwrap()
            .len(),
        1
    );
    drop(retained.pop());
    chat.send(HostCommand::Dispatch).await.unwrap();
    let store = Arc::clone(&gateway.state.lock().await.chat_store);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !store.pending_deliveries(&chat_id).await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("work resumes after capacity is available");
    gateway.shutdown().await;
}

async fn participant_id(gateway: &GatewayHost, chat: &HostHandle, bot_id: &str) -> String {
    gateway
        .state
        .lock()
        .await
        .chat_store
        .load(chat.session_id())
        .await
        .unwrap()
        .unwrap()
        .session_id(bot_id)
        .unwrap()
        .to_owned()
}

pub(super) async fn seed_active(
    gateway: &GatewayHost,
    chat: &HostHandle,
    bot: &crate::wire::BotRecord,
    id: &str,
    approval: bool,
) -> String {
    let state = gateway.state.lock().await;
    let chat_record = state
        .chat_store
        .load(chat.session_id())
        .await
        .unwrap()
        .unwrap();
    let session_id = chat_record.session_id(&bot.id).unwrap().to_owned();
    let spec =
        ChatSpec::for_bot(&chat_record.workspace, bot, state.store.state_dir(), None).unwrap();
    let Op::Message { message: input } = message(id, "original request").op else {
        unreachable!()
    };
    state
        .chat_store
        .post_user(
            chat.session_id(),
            id.into(),
            input.clone(),
            std::slice::from_ref(&bot.id),
        )
        .await
        .unwrap();
    let mut checkpoint = Checkpoint::empty(&session_id);
    checkpoint.catalog_visible = false;
    checkpoint.metadata = spec.metadata().unwrap();
    checkpoint.session_context.workspace_id = Some(spec.workspace_info().id);
    checkpoint.session_context.workspace_label =
        Some(spec.workspace_info().path.display().to_string());
    checkpoint.context = vec![mobius::backend::model::user_message("original request")];
    checkpoint.active_execution = Some(ActiveExecution {
        submission_id: id.into(),
        turn_id: format!("{id}-turn"),
        started_at_ms: 1_000,
        model_calls: 0,
        tool_calls: 0,
        failed_tool_calls: 0,
        usage: Default::default(),
        next_model_step: 0,
        stop_hook_active: false,
        phase: ExecutionPhase::Model,
    });
    if approval {
        checkpoint.pending_approval = Some(PendingApproval {
            submission_id: id.into(),
            turn_id: format!("{id}-turn"),
            request_id: format!("{id}-approval"),
            approval_call_ids: Vec::new(),
            authorized_call_ids: Vec::new(),
            calls: Vec::new(),
            reason: "Approve work".into(),
            sandbox_mode: Default::default(),
            network_access: Default::default(),
            decision_received: false,
        });
    }
    state
        .checkpoints
        .save(&checkpoint, &[], None)
        .await
        .unwrap();
    state
        .checkpoints
        .append_event(
            &session_id,
            1,
            &Event {
                submission_id: Some(id.into()),
                msg: EventMsg::Message(MessageEvent {
                    author: input.author,
                    delivery: MessageDelivery::Turn,
                    text: input.text,
                    attachments: Vec::new(),
                    reply: None,
                    message_target: None,
                }),
            },
        )
        .await
        .unwrap();
    session_id
}

async fn private_events(gateway: &GatewayHost, session_id: &str) -> Vec<JournalEvent> {
    gateway
        .state
        .lock()
        .await
        .checkpoints
        .event_page(
            session_id,
            EventPageRequest {
                before_sequence: None,
                limit: 100,
            },
        )
        .await
        .unwrap()
        .events
}

#[tokio::test]
async fn one_bot_chat_accepts_steering_and_queueing_before_the_active_turn_finishes() {
    let (root, gateway, bot) = bots::gateway_with_bot().await;
    pause_deliveries(&gateway).await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let chat = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let session_id = seed_active(&gateway, &chat, &bot, "active", true).await;
    assert_ne!(chat.session_id(), session_id);
    let ready = chat.snapshot(None).await.unwrap().ready;
    assert_eq!(ready.member_bot_ids, vec![bot.id.clone()]);
    assert_eq!(ready.active_turn_ids, vec!["active-turn"]);
    assert!(!ready.contributions.is_empty());
    for (id, requested_delivery) in [
        ("steer", ActiveMessageDelivery::Steer),
        ("queue", ActiveMessageDelivery::Queue),
    ] {
        let mut submission = message(id, "next instruction");
        let Op::Message { message } = &mut submission.op else {
            unreachable!()
        };
        message.requested_delivery = Some(requested_delivery);
        chat.submit(submission).await.unwrap();
        chat.send(HostCommand::Dispatch).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), chat.snapshot(None))
            .await
            .unwrap()
            .unwrap();
    }
    let checkpoint = gateway
        .state
        .lock()
        .await
        .checkpoints
        .load(&session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        checkpoint.active_execution.as_ref().unwrap().submission_id,
        "active"
    );
    assert_eq!(
        checkpoint
            .pending_messages
            .iter()
            .map(|message| message.id())
            .collect::<Vec<_>>(),
        vec!["steer", "queue"]
    );
    let chats = Arc::clone(&gateway.state.lock().await.chat_store);
    let pending = chats.pending_deliveries(chat.session_id()).await.unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|(_, entry)| entry.id.as_str())
            .collect::<Vec<_>>(),
        vec!["active", "steer", "queue"]
    );
    chat.stop().await.unwrap();
    let checkpoint = gateway
        .state
        .lock()
        .await
        .checkpoints
        .load(&session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(checkpoint.active_execution.is_none());
    assert!(checkpoint.pending_approval.is_none());
    assert!(checkpoint.pending_messages.is_empty());
    assert!(
        chats
            .pending_deliveries(chat.session_id())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        private_events(&gateway, &session_id)
            .await
            .iter()
            .any(|record| {
                record.event.submission_id.as_deref() == Some("active")
                    && matches!(record.event.msg, EventMsg::TurnAborted(_))
            })
    );
    gateway.shutdown().await;
}

#[tokio::test]
async fn stop_cancels_uncached_active_and_pending_work_without_starting_a_model() {
    let (root, gateway, workspace, chat, bots) = gateway_with_group().await;
    pause_deliveries(&gateway).await;
    let first = seed_active(&gateway, &chat, &bots[0], "first", false).await;
    let second = seed_active(&gateway, &chat, &bots[1], "second", true).await;
    chat.submit_to(
        message("queued", "later work"),
        bots.iter().map(|bot| bot.id.clone()).collect(),
    )
    .await
    .unwrap();
    let other_workspace = root.path().join("other");
    std::fs::create_dir(&other_workspace).unwrap();
    let other = gateway
        .create_session(&other_workspace, &bots[0].id)
        .await
        .unwrap();
    let other_id = seed_active(&gateway, &other, &bots[0], "other", true).await;
    other.snapshot(None).await.unwrap();
    chat.stop().await.unwrap();
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let chats = Arc::clone(&gateway.state.lock().await.chat_store);
    for session_id in [&first, &second] {
        let checkpoint = checkpoints.load(session_id).await.unwrap().unwrap();
        assert!(checkpoint.active_execution.is_none());
        assert!(checkpoint.pending_approval.is_none());
        assert!(checkpoint.pending_messages.is_empty());
        assert!(
            !private_events(&gateway, session_id)
                .await
                .iter()
                .any(|record| matches!(record.event.msg, EventMsg::ModelStepStarted(_)))
        );
    }
    assert!(
        checkpoints
            .load(&other_id)
            .await
            .unwrap()
            .unwrap()
            .active_execution
            .is_some()
    );
    assert_eq!(
        chat.snapshot(None).await.unwrap().ready.workspace.path,
        workspace.canonicalize().unwrap()
    );
    assert!(
        chats
            .pending_deliveries(chat.session_id())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !chats
            .settle_delivery(
                "first",
                &first,
                &bots[0].id,
                ChatRunOutcome::Succeeded {
                    summary: format!(
                        r#"{{"text":"late reply","recipient_bot_ids":["{}"]}}"#,
                        bots[1].id
                    ),
                }
            )
            .await
            .unwrap()
    );
    chat.send(HostCommand::Dispatch).await.unwrap();
    let ready = chat.snapshot(None).await.unwrap().ready;
    assert!(ready.active_turn_ids.is_empty());
    assert!(ready.pending_approvals.is_empty());
    gateway.shutdown().await;
}

#[tokio::test]
async fn reopening_after_durable_cancellation_does_not_resume_the_old_execution() {
    let (root, gateway, bot) = bots::gateway_with_bot().await;
    pause_deliveries(&gateway).await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let chat = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let chat_id = chat.session_id().to_owned();
    let session_id = seed_active(&gateway, &chat, &bot, "canceled-before-crash", false).await;
    gateway
        .state
        .lock()
        .await
        .chat_store
        .cancel_pending(&chat_id)
        .await
        .unwrap();
    gateway.shutdown().await;
    let (store, config) = ConfigStore::open(root.path().join("state")).unwrap();
    let credentials = Arc::new(CredentialStore::open(store.credentials_path()).unwrap());
    let bots = Arc::new(BotStore::open(store.state_dir()).unwrap());
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .unwrap();
    let chat = gateway.open_session(&chat_id).await.unwrap();
    let ready = chat.snapshot(None).await.unwrap().ready;
    assert!(ready.active_turn_ids.is_empty());
    let events = private_events(&gateway, &session_id).await;
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event.msg, EventMsg::ModelStepStarted(_)))
    );
    assert!(
        events
            .iter()
            .any(
                |record| record.event.submission_id.as_deref() == Some("canceled-before-crash")
                    && matches!(record.event.msg, EventMsg::TurnAborted(_))
            )
    );
    chat.submit(message("fresh", "new request after Stop"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if private_events(&gateway, &session_id)
                .await
                .iter()
                .any(|record| {
                    record.event.submission_id.as_deref() == Some("fresh")
                        && matches!(record.event.msg, EventMsg::Message(_))
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("new work is admitted after canceled recovery");
    gateway.shutdown().await;
}

#[tokio::test]
async fn restart_reopens_pending_approval_without_resubmitting_the_chat_message() {
    let (root, gateway, _workspace, chat, bots) = gateway_with_group().await;
    pause_deliveries(&gateway).await;
    let chat_id = chat.session_id().to_owned();
    let session_id = seed_active(&gateway, &chat, &bots[0], "replayed", true).await;
    gateway.shutdown().await;
    let (store, config) = ConfigStore::open(root.path().join("state")).unwrap();
    let credentials = Arc::new(CredentialStore::open(store.credentials_path()).unwrap());
    let bots = Arc::new(BotStore::open(store.state_dir()).unwrap());
    let gateway = GatewayHost::start(store, config, credentials, bots)
        .await
        .unwrap();
    let chat = gateway.open_session(&chat_id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if gateway
                .state
                .lock()
                .await
                .sessions
                .contains_key(&session_id)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pending participant is resumed by its Chat");
    let ready = chat.snapshot(None).await.unwrap().ready;
    assert_eq!(ready.active_turn_ids, vec!["replayed-turn"]);
    assert_eq!(ready.pending_approvals.len(), 1);
    let events = private_events(&gateway, &session_id).await;
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(record.event.msg, EventMsg::Message(_)))
            .count(),
        1
    );
    chat.stop().await.unwrap();
    gateway.shutdown().await;
}

#[tokio::test]
async fn typed_recipients_use_distinct_executions_for_overlapping_chats() {
    let (root, gateway, workspace, first, bots) = gateway_with_group().await;
    pause_deliveries(&gateway).await;
    let second_workspace = root.path().join("second");
    std::fs::create_dir(&second_workspace).unwrap();
    let second = gateway
        .create_chat(
            &second_workspace,
            &bots.iter().map(|bot| bot.id.clone()).collect::<Vec<_>>(),
            Some(&bots[0].id),
        )
        .await
        .unwrap();
    let first_id = seed_active(&gateway, &first, &bots[0], "first", true).await;
    let second_id = seed_active(&gateway, &second, &bots[0], "second", true).await;
    assert_ne!(first_id, second_id);
    for (chat, expected_workspace, private_id) in [
        (&first, &workspace, &first_id),
        (&second, &second_workspace, &second_id),
    ] {
        let host = gateway.open_session(private_id).await.unwrap();
        assert_eq!(host.session_id(), private_id);
        assert_eq!(
            host.snapshot(None).await.unwrap().ready.workspace.path,
            expected_workspace.canonicalize().unwrap()
        );
        let unused = participant_id(&gateway, chat, &bots[1].id).await;
        assert!(
            gateway
                .state
                .lock()
                .await
                .checkpoints
                .load(&unused)
                .await
                .unwrap()
                .is_none()
        );
    }
    first.stop().await.unwrap();
    assert!(
        gateway
            .state
            .lock()
            .await
            .checkpoints
            .load(&second_id)
            .await
            .unwrap()
            .unwrap()
            .active_execution
            .is_some()
    );
    second.stop().await.unwrap();
    gateway.shutdown().await;
}

#[tokio::test]
async fn reopening_recovers_unpublished_results_and_settles_every_completed_delivery_once() {
    use mobius::protocol::{
        AssistantMessageEvent, ModelStepContent, TurnCompleteEvent, TurnStartedEvent,
    };

    let (root, gateway, bot) = bots::gateway_with_bot().await;
    pause_deliveries(&gateway).await;
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let chat = gateway.create_session(&workspace, &bot.id).await.unwrap();
    let session_id = seed_active(&gateway, &chat, &bot, "first", false).await;
    let state = gateway.state.lock().await;
    let checkpoints = Arc::clone(&state.checkpoints);
    let chats = Arc::clone(&state.chat_store);
    let files = state.session_files.clone();
    drop(state);
    let observation = files
        .publish_artifact(
            &session_id,
            "result.txt".into(),
            "text/plain".into(),
            b"recovered file",
        )
        .await
        .unwrap();
    let Op::Message { message: second } = message("second", "queued request").op else {
        unreachable!()
    };
    chats
        .post_user(chat.session_id(), "second".into(), second, &[])
        .await
        .unwrap();
    let mut checkpoint = checkpoints.load(&session_id).await.unwrap().unwrap();
    checkpoint.active_execution = None;
    checkpoint.sequence += 1;
    checkpoints.save(&checkpoint, &[], None).await.unwrap();
    for id in ["first", "second"] {
        for msg in [
            EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: format!("{id}-turn"),
                model_context_window: None,
            }),
            EventMsg::AssistantMessage(AssistantMessageEvent {
                session_id: session_id.clone(),
                turn_id: format!("{id}-turn"),
                model_step_id: format!("{id}-step"),
                content: vec![ModelStepContent {
                    output_index: 0,
                    part_index: 0,
                    annotations: Vec::new(),
                    phase: ModelStepContentPhase::FinalAnswer,
                    text: format!("{id} result"),
                }],
                message_target: None,
            }),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: format!("{id}-turn"),
            }),
        ] {
            let record = checkpoints
                .append_event(
                    &session_id,
                    1_000,
                    &Event {
                        submission_id: Some(id.into()),
                        msg,
                    },
                )
                .await
                .unwrap();
            if id == "second" && matches!(record.event.msg, EventMsg::TurnStarted(_)) {
                checkpoints
                    .append_event(
                        &session_id,
                        1_000,
                        &Event {
                            submission_id: Some(id.into()),
                            msg: EventMsg::ToolCallEnd(mobius::protocol::ToolCallEndEvent {
                                turn_id: format!("{id}-turn"),
                                call_id: "observation".into(),
                                name: "inspect_result".into(),
                                output: mobius::protocol::ToolContent(vec![
                                    mobius::protocol::ContentPart::File {
                                        file: observation.clone(),
                                    },
                                ]),
                                is_error: false,
                            }),
                        },
                    )
                    .await
                    .unwrap();
            }
            if id == "first" {
                chats
                    .observe_record(&session_id, &record, &[])
                    .await
                    .unwrap();
            }
        }
    }
    assert_eq!(
        chats
            .pending_deliveries(chat.session_id())
            .await
            .unwrap()
            .len(),
        2
    );
    // The first result was published before the crash but never settled; the second only reached the private journal.
    for _ in 0..2 {
        chat.send(HostCommand::Dispatch).await.unwrap();
        chat.snapshot(None).await.unwrap();
        assert_eq!(
            files
                .file_reference(chat.session_id(), &observation.id)
                .await
                .unwrap(),
            observation
        );
        assert!(
            chats
                .pending_deliveries(chat.session_id())
                .await
                .unwrap()
                .is_empty()
        );
        let records = chats
            .event_page(
                chat.session_id(),
                EventPageRequest {
                    before_sequence: None,
                    limit: 100,
                },
            )
            .await
            .unwrap()
            .events;
        for id in ["first", "second"] {
            assert_eq!(
                records
                    .iter()
                    .filter(|record| record.event.submission_id.as_deref() == Some(id)
                        && matches!(record.event.msg, EventMsg::AssistantMessage(_)))
                    .count(),
                1
            );
            assert_eq!(
                records
                    .iter()
                    .filter(|record| record.event.submission_id.as_deref() == Some(id)
                        && matches!(record.event.msg, EventMsg::TurnComplete(_)))
                    .count(),
                1
            );
        }
        chat.stop().await.unwrap();
    }
    gateway.shutdown().await;
}

#[tokio::test]
async fn participant_changes_refresh_the_public_chat_ready_payload() {
    let (root, gateway, bot) = bots::gateway_with_bot().await;
    let workspace = root.path().join("workspace");
    let attached = root.path().join("attached");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&attached).unwrap();
    let chat = gateway.create_session(&workspace, &bot.id).await.unwrap();
    chat.snapshot(None).await.unwrap();
    let mut events = chat.subscribe();
    chat.attach_folder(attached.clone()).await.unwrap();
    let ready = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let ServerMessage::SessionChanged { payload } = events.recv().await.unwrap().message
            {
                break payload;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(ready.session.session_id, chat.session_id());
    assert_eq!(ready.member_bot_ids, vec![bot.id]);
    assert_eq!(
        ready.attached_folders,
        vec![attached.canonicalize().unwrap()]
    );
    gateway.shutdown().await;
}
