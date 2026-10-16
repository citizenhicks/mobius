//! Peer-message runtime and replay tests.

use super::*;

struct CountUserPromptSubmits(Arc<AtomicUsize>);

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
