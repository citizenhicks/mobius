//! Peer-message runtime and replay tests.

use super::*;

struct CountUserPromptSubmits(Arc<AtomicUsize>);

struct CountMessageSubmits(Arc<AtomicUsize>);

impl Middleware for CountUserPromptSubmits {
    fn name(&self) -> &'static str {
        "count_user_prompt_submits"
    }

    fn message_submit<'a>(
        &'a self,
        context: &'a mut MessageSubmitContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        if matches!(context.author, MessageAuthor::User) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async { Ok(()) })
    }
}

impl Middleware for CountMessageSubmits {
    fn name(&self) -> &'static str {
        "count_message_submits"
    }

    fn message_submit<'a>(
        &'a self,
        _context: &'a mut MessageSubmitContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn idle_peer_messages_start_turns_and_replay_without_becoming_user_prompts() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([
            scripted_message("First peer handled."),
            scripted_message("Second peer handled."),
            scripted_message("Resumed peer handled."),
        ])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let prompt_submits = Arc::new(AtomicUsize::new(0));
    let mut config = config_with_model(
        workspace.path(),
        checkpoints.clone(),
        "peer-message",
        "test",
        model.clone(),
    );
    config.middleware = test_middleware(vec![Arc::new(CountUserPromptSubmits(Arc::clone(
        &prompt_submits,
    )))]);
    let mut agent = create_agent(config).await.expect("create agent");

    let first = peer_op(
        "message-1",
        "session-reviewer",
        "reviewer",
        "Review the parser boundary.",
    );
    assert_eq!(
        serde_json::to_value(&first).expect("serialize peer message"),
        serde_json::json!({
            "type": "message",
            "message": {
                "author": {
                    "type": "source",
                    "message_id": "message-1",
                "source": {"type": "session", "session_id": "session-reviewer"},
                "cause_id": null,
                "ancestry": [],
                    "handle": "reviewer"
                },
                "text": "Review the parser boundary.",
                "attachments": [],
                "reply": null,
                "requested_delivery": "steer",
                "target_turn_id": null
            }
        })
    );
    let second = peer_op(
        "message-2",
        "session-builder",
        "builder",
        "The parser fix is ready.",
    );

    let mut live_peers = Vec::new();
    let mut user_messages = 0;
    for message in [first, second] {
        agent.sender().submit(message).expect("submit peer message");
        loop {
            match agent.next_event().await.expect("agent event").msg {
                EventMsg::Message(message)
                    if matches!(message.author, MessageAuthor::Source { .. }) =>
                {
                    live_peers.push(message);
                }
                EventMsg::Message(message) if message.author == MessageAuthor::User => {
                    user_messages += 1;
                }
                EventMsg::TurnComplete(_) => break,
                _ => {}
            }
        }
    }

    assert_eq!(prompt_submits.load(Ordering::SeqCst), 0);
    assert_eq!(user_messages, 0);
    assert_eq!(live_peers.len(), 2);
    assert!(
        live_peers
            .iter()
            .all(|message| message.delivery == MessageDelivery::Turn)
    );
    let saved = checkpoints
        .load("peer-message")
        .await
        .expect("load checkpoint")
        .expect("saved checkpoint");
    assert_eq!(
        saved.first_user_message.as_deref(),
        Some("Review the parser boundary.")
    );
    assert!(
        saved
            .delivered_once
            .get("messages")
            .is_some_and(|keys| keys.contains("permissions"))
    );
    {
        let inputs = model.inputs.lock().expect("model input lock");
        let notice = inputs[0]
            .iter()
            .position(|item| internal_message_kind(item) == Some("message_permissions"))
            .expect("first Source receives permission guidance");
        let peer = inputs[0]
            .iter()
            .position(|item| internal_message_kind(item) == Some("message_advisory"))
            .expect("first Source message");
        assert!(
            notice < peer,
            "guidance stays at the message submission point"
        );
        assert!(
            inputs[1].starts_with(&inputs[0]),
            "later messages preserve the exact prefix"
        );
        assert_eq!(
            inputs[1]
                .iter()
                .filter(|item| internal_message_kind(item) == Some("message_permissions"))
                .count(),
            1
        );
        let first_peer = inputs[0]
            .iter()
            .find(|item| internal_message_kind(item) == Some("message_advisory"))
            .expect("first peer model context");
        assert_eq!(first_peer["role"], "user");
        let text = first_peer["content"][0]["text"]
            .as_str()
            .expect("peer context text");
        assert!(text.starts_with("Peer reviewer. Audience: sender."));
        assert!(text.ends_with("\n\nReview the parser boundary."));
        let mut expected = live_peers[0].clone();
        // The journal assigns the reply target after constructing model input.
        expected.message_target = None;
        assert_eq!(
            serde_json::from_value::<MessageEvent>(first_peer["_mobius_message"].clone())
                .expect("typed peer context"),
            expected
        );
    }

    let transcript = checkpoints
        .transcript_page(
            "peer-message",
            TranscriptPageRequest {
                before_sequence: None,
                max_batches: 100,
            },
        )
        .await
        .expect("load transcript")
        .into_positioned_items_chronological();
    let replayed_peers = crate::protocol::replay_events(&transcript, "peer-message")
        .into_iter()
        .filter_map(|event| match event {
            EventMsg::Message(message)
                if matches!(message.author, MessageAuthor::Source { .. }) =>
            {
                Some(message)
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(replayed_peers, live_peers);

    let (sender, mut events) = agent.into_parts();
    drop(sender);
    while events.recv().await.is_some() {}
    let mut resumed = create_agent(config_with_model(
        workspace.path(),
        checkpoints,
        "peer-message",
        "test",
        Arc::<ScriptedModel>::clone(&model),
    ))
    .await
    .expect("resume peer session");
    resumed
        .sender()
        .submit(peer_op(
            "message-3",
            "session-resumed",
            "resumed-reviewer",
            "Resume the review.",
        ))
        .expect("submit resumed peer message");
    while !matches!(
        resumed.next_event().await.expect("resumed event").msg,
        EventMsg::TurnComplete(_)
    ) {}
    let inputs = model.inputs.lock().expect("model input lock");
    assert_eq!(inputs.len(), 3);
    assert!(
        inputs[2].starts_with(&inputs[1]),
        "resume preserves the exact prefix"
    );
    assert_eq!(
        inputs[2]
            .iter()
            .filter(|item| internal_message_kind(item) == Some("message_permissions"))
            .count(),
        1
    );
    assert!(inputs[2].iter().any(|item| {
        item.get("_mobius_message")
            .is_some_and(|message| message["author"]["handle"] == "resumed-reviewer")
    }));
}

#[tokio::test]
async fn rejected_source_does_not_consume_once_only_permission_guidance() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let model = Arc::new(ScriptedModel {
        outputs: Mutex::new(VecDeque::from([scripted_message("Accepted peer handled.")])),
        tool_counts: Mutex::new(Vec::new()),
        inputs: Mutex::new(Vec::new()),
    });
    let mut config = config_with_model(
        workspace.path(),
        Arc::<SqliteCheckpoint>::clone(&checkpoints),
        "rejected-source-guidance",
        "test",
        Arc::<ScriptedModel>::clone(&model),
    );
    config.middleware = test_middleware(vec![Arc::new(RejectFirstPrompt(AtomicBool::new(false)))]);
    let mut agent = create_agent(config).await.expect("create agent");
    agent
        .sender()
        .submit(peer_op("rejected", "peer", "reviewer", "Rejected report."))
        .expect("submit rejected Source");
    while !matches!(
        agent.next_event().await.expect("rejection event").msg,
        EventMsg::TurnAborted(_)
    ) {}
    let rejected = checkpoints
        .load("rejected-source-guidance")
        .await
        .expect("load rejected checkpoint")
        .expect("rejected checkpoint");
    assert!(rejected.delivered_once.is_empty());
    assert!(rejected.context.is_empty());
    assert!(model.inputs.lock().expect("model input lock").is_empty());

    agent
        .sender()
        .submit(peer_op("accepted", "peer", "reviewer", "Accepted report."))
        .expect("submit accepted Source");
    while !matches!(
        agent.next_event().await.expect("accepted event").msg,
        EventMsg::TurnComplete(_)
    ) {}
    let saved = checkpoints
        .load("rejected-source-guidance")
        .await
        .expect("load completed checkpoint")
        .expect("completed checkpoint");
    assert!(
        saved
            .delivered_once
            .get("messages")
            .is_some_and(|keys| keys.contains("permissions"))
    );
    assert_eq!(
        saved
            .context
            .iter()
            .filter(|item| internal_message_kind(item) == Some("message_permissions"))
            .count(),
        1
    );
    let inputs = model.inputs.lock().expect("model input lock");
    assert_eq!(inputs.len(), 1);
    assert!(
        inputs[0]
            .iter()
            .any(|item| internal_message_kind(item) == Some("message_permissions"))
    );
    assert!(inputs[0].iter().any(|item| {
        item.get("_mobius_message")
            .is_some_and(|message| message["text"] == "Accepted report.")
    }));
}

#[tokio::test]
async fn active_peer_message_steers_the_current_turn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
            .expect("checkpoint store"),
    );
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let model = Arc::new(BlockingModel {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
        calls: AtomicUsize::new(0),
    });
    let message_submits = Arc::new(AtomicUsize::new(0));
    let mut agent = create_agent(
        AgentConfig::new(
            Arc::new(ModelRouter::new("blocking", model.clone())),
            Arc::new(Sandbox::new(
                Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
                ApprovalPolicy::Ask,
            )),
            Arc::clone(&checkpoints),
            test_middleware(vec![Arc::new(CountMessageSubmits(Arc::clone(
                &message_submits,
            )))]),
            "test prompt",
        )
        .session_context(test_session_context())
        .session_id("active-peer"),
    )
    .await
    .expect("create agent");
    agent.sender().submit(user_op("start")).expect("start turn");
    loop {
        if matches!(
            agent.next_event().await.expect("turn event").msg,
            EventMsg::TurnStarted(_)
        ) {
            break;
        }
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        drain_until_notified(&mut agent, &started),
    )
    .await
    .expect("model started");
    let user_context = checkpoints
        .load("active-peer")
        .await
        .expect("load active user checkpoint")
        .expect("active user checkpoint");
    assert!(user_context.delivered_once.is_empty());
    assert!(
        user_context
            .context
            .iter()
            .all(|item| internal_message_kind(item) != Some("message_permissions"))
    );
    agent
        .sender()
        .submit(peer_op(
            "message-1",
            "session-reviewer",
            "reviewer",
            "Check the boundary.",
        ))
        .expect("submit peer message");
    agent
        .sender()
        .submit(peer_op(
            "message-2",
            "session-builder",
            "builder",
            "The boundary is ready.",
        ))
        .expect("submit second peer message");
    release.notify_one();

    let mut messages = Vec::new();
    loop {
        match agent.next_event().await.expect("agent event").msg {
            EventMsg::Message(message)
                if matches!(message.author, MessageAuthor::Source { .. }) =>
            {
                messages.push(message);
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(messages.len(), 2);
    assert!(
        messages
            .iter()
            .all(|message| message.delivery == MessageDelivery::Steer)
    );
    assert_eq!(
        messages[0]
            .message_target
            .as_ref()
            .expect("first message target")
            .checkpoint_sequence,
        messages[1]
            .message_target
            .as_ref()
            .expect("second message target")
            .checkpoint_sequence,
        "both Source messages enter the same durable batch"
    );
    assert_eq!(message_submits.load(Ordering::SeqCst), 3);
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    let saved = checkpoints
        .load("active-peer")
        .await
        .expect("load steered checkpoint")
        .expect("steered checkpoint");
    assert!(saved.context.starts_with(&user_context.context));
    let peers = saved
        .context
        .iter()
        .filter_map(|item| {
            let message = item.get("_mobius_message")?;
            Some((
                message["author"]["handle"].as_str()?,
                message["text"].as_str()?,
            ))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        peers,
        [
            ("reviewer", "Check the boundary."),
            ("builder", "The boundary is ready.")
        ]
    );
    let notices = saved
        .context
        .iter()
        .enumerate()
        .filter(|(_, item)| internal_message_kind(item) == Some("message_permissions"))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let peer = saved
        .context
        .iter()
        .position(|item| internal_message_kind(item) == Some("message_advisory"))
        .expect("steered Source message");
    assert_eq!(
        notices,
        [peer.checked_sub(1).expect("Source follows its guidance")]
    );
    assert!(notices[0] >= user_context.context.len());
    assert!(
        saved
            .delivered_once
            .get("messages")
            .is_some_and(|keys| keys.contains("permissions"))
    );
}
