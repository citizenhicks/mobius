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
    let checkpoints = test_checkpoints(workspace.path());
    let model = Arc::new(ScriptedModel::new([
        scripted_message("First peer handled."),
        scripted_message("Second peer handled."),
        scripted_message("Resumed peer handled."),
    ]));
    let prompt_submits = Arc::new(AtomicUsize::new(0));
    let mut config = config_with_model(
        workspace.path(),
        Arc::<SqliteCheckpoint>::clone(&checkpoints),
        "peer-message",
        "test",
        Arc::<ScriptedModel>::clone(&model),
    );
    config.middleware = Arc::new(test_middleware(vec![Arc::new(CountUserPromptSubmits(
        Arc::clone(&prompt_submits),
    ))]));
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
        .load(&"peer-message".parse().expect("session ID"))
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
        drop(inputs);
    }

    let transcript = checkpoints
        .transcript_page(
            &"peer-message".parse().expect("session ID"),
            TranscriptPageRequest {
                before_sequence: None,
                max_batches: 100,
            },
        )
        .await
        .expect("load transcript")
        .into_positioned_items_chronological();
    let replayed_peers =
        crate::protocol::replay_events(&transcript, &"peer-message".parse().expect("session ID"))
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
    let inputs = std::mem::take(&mut *model.inputs.lock().expect("model input lock"));
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

fn shell_call(id: &str, command: &str, background: bool) -> ModelOutput {
    let arguments = serde_json::json!({"command": command, "run_in_background": background});
    ModelOutput::from_output(vec![serde_json::json!({
        "type": "function_call", "name": "bash", "call_id": id, "arguments": arguments.to_string()
    })], false, scripted_usage()).expect("bash tool call")
}

fn shell_config(workspace: &std::path::Path, model: Arc<ScriptedModel>) -> AgentConfig {
    use crate::backend::session_files::SessionFileStore;

    runtime_config(
        Arc::new(ModelRouter::new("test", model)),
        test_sandbox(workspace, ApprovalPolicy::FullAccess),
        test_checkpoints(workspace),
        test_middleware(vec![Arc::new(Tools::coding(Arc::new(
            SessionFileStore::new(workspace, None),
        )))]),
        "command-wake",
    )
}

#[tokio::test]
async fn stopping_one_background_command_and_finishing_another_each_wake_the_idle_chat() {
    use crate::backend::sandbox::RUNNING_COMMANDS_WIDGET;
    use crate::protocol::{FrontendWidgetContent, MessageSource};

    let workspace = tempfile::tempdir().expect("workspace");
    let build = "while [ ! -f release ]; do sleep 0.05; done; exit 7";
    let other = "sleep 120";
    let model = Arc::new(ScriptedModel::new([
        shell_call("build", build, true),
        shell_call("other", other, true),
        scripted_message("Waiting."),
        scripted_message("The second command was stopped."),
        scripted_message("The build exited."),
    ]));
    let mut agent = create_agent(shell_config(workspace.path(), model))
        .await
        .expect("create agent");
    agent
        .sender()
        .submit(user_op("Run both commands."))
        .expect("start turn");
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let mut turns = 0;
        let mut stop = None;
        let mut build_id = None;
        let mut stopped_id = None;
        let mut counts = Vec::new();
        let mut notices = Vec::new();
        while turns < 3 || counts.last() != Some(&0) {
            match agent.next_event().await.expect("agent event").msg {
                EventMsg::Frontend(FrontendEvent::Widget { item, .. })
                    if item.id.as_ref() == RUNNING_COMMANDS_WIDGET =>
                {
                    let Some(FrontendWidgetContent::ActionList { items, .. }) = &item.content
                    else {
                        panic!("clickable command list")
                    };
                    let active = items
                        .iter()
                        .filter(|row| {
                            row.state == crate::protocol::FrontendListItemState::InProgress
                        })
                        .count();
                    assert_eq!(item.count, Some(active));
                    counts.push(active);
                    for row in items {
                        match row.state {
                            crate::protocol::FrontendListItemState::InProgress
                                if row.text == build =>
                            {
                                build_id = Some(Arc::clone(&row.id));
                            }
                            crate::protocol::FrontendListItemState::InProgress => {
                                assert_eq!(row.text, other);
                                stopped_id = Some(Arc::clone(&row.id));
                                stop = Some(row.actions[0].operation());
                            }
                            crate::protocol::FrontendListItemState::Failed => {
                                assert_eq!(row.text, build);
                                assert!(row.actions.is_empty());
                            }
                            crate::protocol::FrontendListItemState::Interrupted => {
                                assert_eq!(row.text, other);
                                assert!(row.actions.is_empty());
                            }
                            state => panic!("unexpected command state {state:?}"),
                        }
                    }
                    if turns > 0 {
                        assert_eq!(items.len(), 2, "retain both outcomes in open details");
                    }
                }
                EventMsg::Message(message) => {
                    if let MessageAuthor::Source {
                        source: MessageSource::External { event_id, .. },
                        ..
                    } = &message.author
                    {
                        assert_eq!(message.delivery, MessageDelivery::Turn);
                        let (id, text) = if notices.is_empty() {
                            (&stopped_id, "stopped by the user")
                        } else {
                            (&build_id, "exited with code 7")
                        };
                        assert_eq!(Some(event_id.as_ref()), id.as_deref());
                        assert!(message.text.contains(text), "{}", message.text);
                        notices.push(Arc::clone(event_id));
                    }
                }
                EventMsg::TurnComplete(_) => {
                    turns += 1;
                    match turns {
                        1 => {
                            agent
                                .sender()
                                .submit(stop.take().expect("stop action from command row"))
                                .expect("stop command");
                        }
                        2 => std::fs::write(workspace.path().join("release"), [])
                            .expect("release remaining command"),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        assert_eq!(counts, [1, 2, 1, 0]);
        assert_eq!(notices.len(), 2);
    })
    .await
    .expect("background notices woke the idle chat");
}

#[tokio::test]
async fn stopping_a_foreground_command_returns_its_result_to_the_active_turn() {
    use crate::protocol::FrontendWidgetContent;
    let workspace = tempfile::tempdir().expect("workspace");
    let model = Arc::new(ScriptedModel::new([
        shell_call("foreground", "sleep 120", false),
        scripted_message("Stopped."),
    ]));
    let mut agent = create_agent(shell_config(workspace.path(), Arc::clone(&model)))
        .await
        .expect("create agent");
    agent
        .sender()
        .submit(user_op("Run the command."))
        .expect("start turn");
    tokio::time::timeout(std::time::Duration::from_secs(6), async {
        loop {
            match agent.next_event().await.expect("agent event").msg {
                EventMsg::Frontend(FrontendEvent::Widget { item, .. })
                    if item.id.as_ref() == crate::backend::sandbox::RUNNING_COMMANDS_WIDGET
                        && item.count == Some(1) =>
                {
                    let Some(FrontendWidgetContent::ActionList { items, .. }) = &item.content
                    else {
                        panic!("command list")
                    };
                    agent
                        .sender()
                        .submit(items[0].actions[0].operation())
                        .expect("stop foreground command");
                }
                EventMsg::TurnComplete(_) => break,
                _ => {}
            }
        }
    })
    .await
    .expect("stop did not wait for foreground timeout");
    let inputs = std::mem::take(&mut *model.inputs.lock().expect("model inputs"));
    assert!(
        inputs.last().expect("final step").iter().any(|item| {
            item["type"] == "function_call_output"
                && item["output"][0]["text"]
                    .as_str()
                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
                    .is_some_and(|result| result["status"] == "stopped")
        }),
        "model must receive the stopped tool result: {inputs:?}"
    );
}
