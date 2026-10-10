use super::bots::gateway_with_bot;
use super::*;
use crate::wire::{
    BotAction, BotSubscription, HookBinding, HookData, HookEvent, HookKind, HookSource,
};
use mobius::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest};
use mobius::protocol::{ActiveMessageDelivery, MessageSource, ToolDiscoveryMode};

#[derive(Default)]
pub(super) struct CaptureModel {
    pub(super) entered: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
    calls: AtomicU64,
    tools: StdMutex<Vec<Vec<String>>>,
    block_all: bool,
}
impl Model for CaptureModel {
    fn supports_image_input(&self) -> bool {
        true
    }
    fn supports_tool_image_input(&self) -> bool {
        true
    }
    fn tool_discovery(&self) -> ToolDiscoveryMode {
        ToolDiscoveryMode::Native
    }
    fn respond<'a>(
        &'a self,
        request: ModelRequest<'a>,
        _: ModelEventSink,
    ) -> mobius::BoxFuture<'a, mobius::Result<ModelOutput>> {
        self.tools.lock().unwrap().push(
            request
                .tools
                .iter()
                .chain(request.deferred_tools)
                .map(|tool| tool.name.clone())
                .collect(),
        );
        let first = self.calls.fetch_add(1, Ordering::Relaxed) == 0;
        Box::pin(async move {
            if first || self.block_all {
                self.entered.notify_one();
                self.release.notified().await;
            }
            ModelOutput::from_output(
                vec![
                    serde_json::json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}),
                ],
                true,
                TokenUsage::default(),
            )
        })
    }
}

pub(super) async fn install_model(
    gateway: &GatewayHost,
    bot: &crate::wire::BotRecord,
    model: Arc<CaptureModel>,
) {
    let state = gateway.state.lock().await;
    let config = state.config.lock().unwrap().clone();
    let selection = &bot.config.config.provider;
    state
        .credentials
        .set(
            &selection.instance,
            &selection.provider,
            "test-token",
            selection.base_url.as_deref(),
            None,
        )
        .unwrap();
    let mut prepared = crate::assembly::prepare_bot(
        &config,
        bot.clone(),
        &state.store,
        &state.credentials,
        state.session_files.clone(),
        0,
        Arc::clone(gateway.remote_desktop.configuration()),
    )
    .await
    .unwrap();
    let route = crate::provider_catalog::configured_model_providers(
        &config,
        &state.store,
        &state.credentials,
    )
    .unwrap()
    .into_keys()
    .next()
    .unwrap();
    prepared.test_models(Arc::new(ModelRouter::new(route, model)));
    state
        .bots
        .prepared
        .lock()
        .await
        .insert(bot.id.clone(), Arc::new(prepared));
}

fn user_submission(id: &str, text: &str) -> Submission {
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
async fn bot_update_without_integer_settings_is_rejected() {
    let (_root, gateway, bot) = gateway_with_bot().await;
    let mut encoded = serde_json::to_value(&bot.config.config).expect("saved Bot configuration");
    let settings = encoded["middleware"]["settings"]
        .as_object_mut()
        .expect("middleware settings");
    settings["sandbox"]
        .as_object_mut()
        .expect("sandbox settings")
        .retain(|id, _| id == "approval_policy");
    settings["compaction"]
        .as_object_mut()
        .expect("compaction settings")
        .retain(|id, _| id == "allow_model_compaction" || id == "at_tokens");
    let composition =
        serde_json::from_value(encoded).expect("deserialize existing Bot configuration");
    let error = {
        let state = gateway.state.lock().await;
        state
            .bots
            .update_bot(
                &bot.id,
                bot.config.revision,
                crate::bots::BotIdentity {
                    name: &bot.name,
                    description: &bot.description,
                    tint: bot.tint,
                    shape: bot.shape,
                },
                composition,
            )
            .expect_err("missing integer settings are rejected")
    };
    assert!(error.to_string().contains("sandbox.tool_output_bytes"));
    gateway.shutdown().await;
}

#[tokio::test]
async fn queued_turns_retain_desktop_use_until_idle_and_duplicate_admission_releases_it() {
    let (_root, gateway, bot) = gateway_with_bot().await;
    let model = Arc::new(CaptureModel {
        block_all: true,
        ..CaptureModel::default()
    });
    install_model(&gateway, &bot, Arc::clone(&model)).await;
    let main = gateway
        .open_session(&bot.conversation_session_id)
        .await
        .unwrap();
    let sandbox = main.inner.gateway_sandbox.upgrade().unwrap();
    main.submit(user_submission("first", "Inspect the desktop"))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), model.entered.notified())
        .await
        .unwrap();
    sandbox.retain_desktop_use_for_test().await;
    let queued = Submission {
        id: "queued-desktop".into(),
        op: Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::Source {
                    message_id: "queued-desktop".into(),
                    source: MessageSource::Session {
                        session_id: "owned-session".into(),
                    },
                    cause_id: None,
                    ancestry: Vec::new(),
                    handle: "other chat".into(),
                    symbol: None,
                },
                text: "Continue working with the desktop".into(),
                attachments: Vec::new(),
                reply: None,
                requested_delivery: Some(ActiveMessageDelivery::Queue),
                target_turn_id: None,
            },
        },
    };
    main.deliver_source(queued.clone(), bot.id.clone())
        .await
        .unwrap();
    model.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), model.entered.notified())
        .await
        .unwrap();
    assert_eq!(gateway.remote_desktop.consumer_count(), 1);
    model.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), main.wait_idle())
        .await
        .unwrap();
    assert_eq!(gateway.remote_desktop.consumer_count(), 0);

    // Already-accepted admission has no new turn-completion event to release a use.
    sandbox.retain_desktop_use_for_test().await;
    main.deliver_source(queued, bot.id.clone()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), main.wait_idle())
        .await
        .unwrap();
    assert_eq!(gateway.remote_desktop.consumer_count(), 0);
    assert_eq!(model.calls.load(Ordering::Relaxed), 2);
    gateway.shutdown().await;
}

#[tokio::test]
async fn project_free_chats_expose_reads_without_coding_mutations() {
    let (_root, gateway, bot) = gateway_with_bot().await;
    let model = Arc::new(CaptureModel::default());
    model.release.notify_one();
    install_model(&gateway, &bot, Arc::clone(&model)).await;
    let main = gateway
        .open_session(&bot.conversation_session_id)
        .await
        .unwrap();
    let ordinary = gateway
        .create_session_with_id(None, &bot.id, Uuid::new_v4().to_string(), true, "test")
        .await
        .unwrap();
    for (chat, persistent) in [(main, true), (ordinary, false)] {
        assert!(chat.ready().await.unwrap().workspace.is_none());
        chat.submit(user_submission("read", "Read the computer-control guide"))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), chat.wait_idle())
            .await
            .unwrap();
        let tools = model.tools.lock().unwrap().last().unwrap().clone();
        assert!(tools.iter().any(|tool| tool == "read_file"));
        assert!(tools.iter().any(|tool| tool == "view_image"));
        assert_eq!(
            tools.iter().any(|tool| tool == "schedule_routine"),
            persistent
        );
        for mutation in ["write_file", "apply_patch", "bash", "manage_command"] {
            assert!(!tools.iter().any(|tool| tool == mutation));
        }
    }
    gateway.shutdown().await;
}

fn subscription(bot_id: &str, session_id: &str, kind: HookKind, id: &str) -> BotSubscription {
    BotSubscription {
        bot_id: bot_id.into(),
        enabled: true,
        binding: HookBinding {
            id: id.into(),
            on: crate::bots::event_selector(
                HookSource::Session {
                    session_id: session_id.into(),
                },
                kind,
            ),
            action: BotAction::Report {
                instruction: "Tell the user what happened.".into(),
            },
        },
    }
}

#[tokio::test]
async fn canonical_chat_queues_sources_and_deduplicates_delivery_after_ack_gap() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let model = Arc::new(CaptureModel::default());
    install_model(&gateway, &bot, Arc::clone(&model)).await;
    let (left, right) = tokio::join!(
        gateway.open_session(&bot.conversation_session_id),
        gateway.open_session(&bot.conversation_session_id)
    );
    let main = left.unwrap();
    assert!(Arc::ptr_eq(&main.inner, &right.unwrap().inner));
    assert!(main.ready().await.unwrap().workspace.is_none());
    main.submit(user_submission("user", "Inspect my work"))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), model.entered.notified())
        .await
        .unwrap();
    let source = MessageAuthor::Source {
        message_id: "peer".into(),
        source: MessageSource::Session {
            session_id: "owned-session".into(),
        },
        cause_id: Some("original-hook".into()),
        ancestry: vec!["original-hook".into()],
        handle: "other chat".into(),
        symbol: None,
    };
    main.deliver_source(
        Submission::message(MessageSubmission {
            author: source.clone(),
            text: "Status report".into(),
            attachments: Vec::new(),
            reply: None,
            requested_delivery: Some(ActiveMessageDelivery::Steer),
            target_turn_id: None,
        }),
        bot.id.clone(),
    )
    .await
    .unwrap();
    let checkpoints = Arc::clone(&gateway.state.lock().await.checkpoints);
    let checkpoint = checkpoints.load(main.session_id()).await.unwrap().unwrap();
    assert_eq!(
        checkpoint.active_execution.unwrap().author,
        MessageAuthor::User
    );
    assert_eq!(checkpoint.pending_messages.len(), 1);
    let queued = serde_json::to_value(&checkpoint.pending_messages[0]).unwrap();
    assert_eq!(queued["boundary"]["type"], "queue");
    assert_eq!(queued["author"], serde_json::to_value(&source).unwrap());
    model.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), main.wait_idle())
        .await
        .unwrap();

    gateway
        .set_bot_subscription(BotSubscription {
            bot_id: bot.id.clone(),
            enabled: true,
            binding: HookBinding {
                id: "custom-report".into(),
                on: crate::bots::event_selector(
                    HookSource::Bot {
                        bot_id: bot.id.clone(),
                    },
                    HookKind::CustomReceived,
                ),
                action: BotAction::Report {
                    instruction: "Summarize this evidence.".into(),
                },
            },
        })
        .await
        .unwrap();
    let event = gateway
        .emit_bot_hook(
            &bot.id,
            "outage",
            serde_json::json!({"Server_ID": "alpha"}),
            "custom-input",
        )
        .await
        .unwrap();
    let bots = Arc::clone(&gateway.state.lock().await.bots);
    let pending = bots
        .pending_actions(Utc::now().timestamp(), 10)
        .unwrap()
        .remove(0);
    gateway.execute_hook_action(&pending).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), main.wait_idle())
        .await
        .unwrap();
    // Simulate a crash after Core admission but before the outbox acknowledgement.
    gateway.execute_hook_action(&pending).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), main.wait_idle())
        .await
        .unwrap();
    assert_eq!(model.calls.load(Ordering::Relaxed), 3);
    assert!(
        checkpoints
            .message_accepted(main.session_id(), &pending.id)
            .await
            .unwrap()
    );
    gateway.dispatch_bot_events().await.unwrap();
    assert!(!bots.action_pending(&pending.id).unwrap());
    let history = main.history_page(None).await.unwrap();
    let report_count = history.records.iter().filter(|record| matches!(&record.event.msg, EventMsg::Message(message) if matches!(&message.author, MessageAuthor::Source { message_id, .. } if message_id == &pending.id))).count();
    assert_eq!(report_count, 1);
    let executions = checkpoints
        .execution_page(
            main.session_id(),
            mobius::backend::checkpoint::ExecutionPageRequest {
                before_sequence: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert!(executions.executions.iter().any(|execution| matches!(&execution.author, MessageAuthor::Source { cause_id: Some(cause), ancestry, .. } if cause == &event.id && ancestry.as_slice() == std::slice::from_ref(&event.id))));
    for record in &history.records {
        if let EventMsg::TurnComplete(turn) = &record.event.msg {
            let hook = bots
                .hook_event(&format!(
                    "session-{}-{}",
                    main.session_id(),
                    record.sequence
                ))
                .unwrap()
                .unwrap();
            if executions
                .executions
                .iter()
                .any(|execution| execution.turn_id == turn.turn_id && execution.author == source)
            {
                assert_eq!(hook.cause_id.as_deref(), Some("peer"));
                assert_eq!(hook.ancestry, ["original-hook", "peer"]);
            }
        }
    }
    assert!(
        model.tools.lock().unwrap()[0]
            .iter()
            .any(|tool| tool == "schedule_routine")
    );
    let workspace = root.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let ordinary = gateway.create_session(&workspace, &bot.id).await.unwrap();
    ordinary
        .submit(user_submission("ordinary", "Hello"))
        .await
        .unwrap();
    ordinary.wait_idle().await;
    assert!(
        !model
            .tools
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .iter()
            .any(|tool| tool == "schedule_routine")
    );
    assert_eq!(
        gateway
            .delete_sessions(
                &[main.session_id().into()],
                mobius::backend::session_files::SessionFileSelection::All
            )
            .await
            .unwrap_err()
            .code,
        "persistent_chat"
    );
    gateway.shutdown().await;
}

#[tokio::test]
async fn reassignment_projects_completed_turns_before_closing_the_old_source() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let model = Arc::new(CaptureModel::default());
    model.release.notify_one();
    install_model(&gateway, &bot, model).await;
    let workspace = root.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let source = gateway.create_session(&workspace, &bot.id).await.unwrap();
    gateway.dispatch_bot_events().await.unwrap();
    for (kind, id) in [
        (HookKind::SessionTurnFinished, "finished"),
        (HookKind::SessionOwnerChanged, "owner-change"),
    ] {
        gateway
            .set_bot_subscription(subscription(&bot.id, source.session_id(), kind, id))
            .await
            .unwrap();
    }
    source
        .submit(user_submission("finish", "Hello"))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), source.wait_idle())
        .await
        .unwrap();
    let finished = source
        .history_page(None)
        .await
        .unwrap()
        .records
        .into_iter()
        .find(|record| matches!(record.event.msg, EventMsg::TurnComplete(_)))
        .unwrap();
    let bots = Arc::clone(&gateway.state.lock().await.bots);
    assert!(bots.source_cursor(source.session_id()).unwrap().0 < finished.sequence);
    let target = bots
        .create_bot("New owner", "Own this chat.", bot.config.config.clone())
        .unwrap();
    gateway
        .reassign_session(source.session_id(), &target.id)
        .await
        .unwrap();
    let event = bots
        .hook_event(&format!(
            "session-{}-{}",
            source.session_id(),
            finished.sequence
        ))
        .unwrap()
        .unwrap();
    assert_eq!(event.bot_id, bot.id);
    assert!(matches!(
        event.data,
        HookData::SessionTurnFinished {
            outcome: ExecutionOutcome::Completed,
            ..
        }
    ));
    let pending = bots.pending_actions(Utc::now().timestamp(), 10).unwrap();
    assert_eq!(pending.len(), 1);
    assert!(matches!(
        pending[0].event.data,
        HookData::SessionOwnerChanged { .. }
    ));
    gateway.shutdown().await;
}

#[tokio::test]
async fn session_closure_keeps_final_fact_and_revokes_old_source_authorization() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let source = gateway.create_session(&workspace, &bot.id).await.unwrap();
    gateway.dispatch_bot_events().await.unwrap();
    gateway
        .set_bot_subscription(subscription(
            &bot.id,
            source.session_id(),
            HookKind::SessionOwnerChanged,
            "owner-change",
        ))
        .await
        .unwrap();
    let target = {
        let state = gateway.state.lock().await;
        state
            .bots
            .create_bot("New owner", "Own this chat.", bot.config.config.clone())
            .unwrap()
    };
    gateway
        .reassign_session(source.session_id(), &target.id)
        .await
        .unwrap();
    let bots = Arc::clone(&gateway.state.lock().await.bots);
    let pending = bots.pending_actions(Utc::now().timestamp(), 10).unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        matches!(&pending[0].event.data, HookData::SessionOwnerChanged { previous_bot_id, .. } if previous_bot_id == &bot.id)
    );
    assert!(bots.subscriptions(&bot.id).unwrap().is_empty());
    // Exact replay keeps the captured final action, even after subscriptions are removed.
    bots.close_session_sources(std::slice::from_ref(&pending[0].event))
        .unwrap();
    assert!(bots.action_pending(&pending[0].id).unwrap());
    gateway
        .set_bot_subscription(subscription(
            &target.id,
            source.session_id(),
            HookKind::SessionDeleted,
            "deleted",
        ))
        .await
        .unwrap();
    gateway
        .delete_sessions(
            &[source.session_id().into()],
            mobius::backend::session_files::SessionFileSelection::All,
        )
        .await
        .unwrap();
    assert!(
        bots.pending_actions(Utc::now().timestamp(), 10)
            .unwrap()
            .iter()
            .any(
                |action| matches!(action.event.data, HookData::SessionDeleted { .. })
                    && action.bot_id == target.id
            )
    );
    assert!(bots.subscriptions(&target.id).unwrap().is_empty());
    gateway.shutdown().await;
}

#[tokio::test]
async fn deletion_hook_recovers_after_checkpoint_commit_before_bot_commit() {
    let (root, gateway, bot) = gateway_with_bot().await;
    let workspace = root.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let source = gateway.create_session(&workspace, &bot.id).await.unwrap();
    gateway
        .set_bot_subscription(subscription(
            &bot.id,
            source.session_id(),
            HookKind::SessionDeleted,
            "deleted",
        ))
        .await
        .unwrap();
    let event = HookEvent {
        id: "committed-deletion".into(),
        bot_id: bot.id.clone(),
        source: HookSource::Session {
            session_id: source.session_id().into(),
        },
        data: HookData::SessionDeleted {
            session_id: source.session_id().into(),
        },
        occurred_at: Utc::now().timestamp(),
        cause_id: None,
        ancestry: Vec::new(),
    };
    assert!(source.stop_if_idle().await);
    let state = gateway.state.lock().await;
    state
        .checkpoints
        .save_state(
            "gateway",
            "session_hook_closures",
            &serde_json::json!([event]),
        )
        .await
        .unwrap();
    state
        .checkpoints
        .delete_sessions(&[source.session_id().into()])
        .await
        .unwrap();
    super::super::deletion::recover_session_hook_closures(&state)
        .await
        .unwrap();
    super::super::deletion::recover_session_hook_closures(&state)
        .await
        .unwrap();
    assert_eq!(
        state
            .bots
            .pending_actions(Utc::now().timestamp(), 10)
            .unwrap()
            .len(),
        1
    );
    assert!(state.bots.subscriptions(&bot.id).unwrap().is_empty());
    drop(state);
    gateway.shutdown().await;
}
