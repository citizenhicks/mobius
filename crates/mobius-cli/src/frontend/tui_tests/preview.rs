use super::support::*;
use super::*;

#[test]
fn another_clients_preview_cannot_open_or_replace_the_requested_preview() {
    let mut state = state();
    state.preview_request_id = Some("preview-request".into());
    let foreign = || {
        let mut record = preview_record(
            "foreign",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["another client"],
            None,
        );
        record.event.submission_id = Some("another-client-request".into());
        record
    };
    events::handle_gateway_event(&mut state, foreign());
    assert!(state.preview.is_none());
    assert_eq!(state.preview_request_id.as_deref(), Some("preview-request"));

    events::handle_gateway_event(
        &mut state,
        preview_record(
            "ours",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["ours"],
            None,
        ),
    );
    assert!(state.preview_request_id.is_none());
    events::handle_gateway_event(&mut state, foreign());
    assert_eq!(snapshot(&state).id, "ours");
    assert_eq!(preview_messages(snapshot(&state)), ["› ours"]);
}

#[test]
fn unsuccessful_preview_model_steps_close_their_pending_narrative() {
    use mobius::protocol::{AssistantContentDeltaEvent, ModelStepCompletedEvent, ModelStepOutcome};

    for outcome in [
        ModelStepOutcome::Failed,
        ModelStepOutcome::Interrupted,
        ModelStepOutcome::Retrying,
    ] {
        let mut state = state();
        state.preview_request_id = Some("preview-request".into());
        let mut record =
            preview_record("child", "latest", FrontendPreviewUpdate::Replace, &[], None);
        record.preview.as_mut().unwrap().events = vec![
            RenderedEvent {
                submission_id: None,
                recorded_at_ms: 0,
                event: EventMsg::AssistantContentDelta(AssistantContentDeltaEvent {
                    session_id: "child".into(),
                    turn_id: "turn".into(),
                    model_step_id: "step".into(),
                    delta: "Partial answer".into(),
                    phase: ModelStepContentPhase::FinalAnswer,
                }),
                blocks: Vec::new(),
            },
            RenderedEvent {
                submission_id: None,
                recorded_at_ms: 1,
                event: EventMsg::ModelStepCompleted(ModelStepCompletedEvent {
                    session_id: "child".into(),
                    turn_id: "turn".into(),
                    model_step_id: "step".into(),
                    step_index: 0,
                    started_at_ms: 0,
                    completed_at_ms: 1,
                    outcome,
                    diagnostics: None,
                }),
                blocks: Vec::new(),
            },
        ];
        events::handle_gateway_event(&mut state, record);
        let entry = snapshot(&state)
            .transcript
            .front()
            .expect("partial narrative");
        assert_eq!(entry.text, "Partial answer");
        assert!(!entry.pending);
        assert!(matches!(entry.tone, TranscriptTone::Warning));
    }
}

#[test]
fn live_preview_replaces_the_changed_model_step_suffix_and_keeps_older_pages() {
    use mobius::protocol::{AssistantContentDeltaEvent, AssistantMessageEvent};

    let mut state = state();
    state.preview_request_id = Some("preview-request".into());
    let mut record = preview_record("child", "latest", FrontendPreviewUpdate::Replace, &[], None);
    record.preview.as_mut().unwrap().events = vec![RenderedEvent {
        submission_id: None,
        recorded_at_ms: 0,
        event: EventMsg::AssistantContentDelta(AssistantContentDeltaEvent {
            session_id: "child".into(),
            turn_id: "turn".into(),
            model_step_id: "step".into(),
            delta: "Obsolete draft reasoning".into(),
            phase: ModelStepContentPhase::Reasoning,
        }),
        blocks: Vec::new(),
    }];
    events::handle_gateway_event(&mut state, record);
    let older = preview_continuation("even older");
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "child",
            "older",
            FrontendPreviewUpdate::Prepend,
            &["older message"],
            Some(older.clone()),
        ),
    );
    let mut completed =
        preview_record("child", "latest", FrontendPreviewUpdate::Replace, &[], None);
    completed.event.submission_id = None;
    completed.preview.as_mut().unwrap().events = vec![RenderedEvent {
        submission_id: None,
        recorded_at_ms: 1,
        event: EventMsg::AssistantMessage(AssistantMessageEvent {
            session_id: "child".into(),
            turn_id: "turn".into(),
            model_step_id: "step".into(),
            message_target: None,
            content: vec![ModelStepContent {
                output_index: 0,
                part_index: 0,
                phase: ModelStepContentPhase::FinalAnswer,
                text: "Final answer".into(),
                annotations: Vec::new(),
            }],
        }),
        blocks: Vec::new(),
    }];
    events::handle_gateway_event(&mut state, completed);
    assert_eq!(
        preview_messages(snapshot(&state)),
        ["› older message", "Final answer"]
    );
    assert!(
        snapshot(&state)
            .transcript
            .iter()
            .all(|entry| !entry.pending)
    );
    assert_eq!(snapshot(&state).next.as_ref(), Some(&older));
}

#[test]
fn live_voice_preview_updates_only_its_open_surface_and_preserves_message_identity() {
    let rendered = |id: &str, event| RenderedEvent {
        submission_id: Some(id.into()),
        recorded_at_ms: 0,
        event,
        blocks: Vec::new(),
    };
    let user = |text: &str| {
        EventMsg::Message(MessageEvent {
            author: MessageAuthor::User,
            delivery: MessageDelivery::Turn,
            text: text.into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        })
    };
    let delta = |id: &str, text: &str| {
        EventMsg::AssistantContentDelta(mobius::protocol::AssistantContentDeltaEvent {
            session_id: "voice-child".into(),
            turn_id: id.into(),
            model_step_id: id.into(),
            delta: text.into(),
            phase: ModelStepContentPhase::FinalAnswer,
        })
    };
    let final_answer = |id: &str, text: &str| {
        EventMsg::AssistantMessage(mobius::protocol::AssistantMessageEvent {
            session_id: "voice-child".into(),
            turn_id: id.into(),
            model_step_id: id.into(),
            message_target: None,
            content: vec![ModelStepContent {
                output_index: 0,
                part_index: 0,
                phase: ModelStepContentPhase::FinalAnswer,
                text: text.into(),
                annotations: Vec::new(),
            }],
        })
    };
    let preview = |events| {
        recorded(
            EventMsg::ContextCompacted,
            Vec::new(),
            Some(RenderedPreview {
                id: "voice-child".into(),
                title: "Voice".into(),
                subtitle: String::new(),
                page_id: "voice-child:latest".into(),
                update: FrontendPreviewUpdate::Replace,
                events,
                next: None,
            }),
        )
    };
    let draft_events = vec![
        rendered("older", user("Earlier call")),
        rendered(
            "user-1",
            EventMsg::MessageDelta(mobius::protocol::MessageDeltaEvent { text: "Hel".into() }),
        ),
        rendered("assistant-1", delta("assistant-1", "First")),
        rendered("assistant-2", delta("assistant-2", "Second")),
        rendered("assistant-1", delta("assistant-1", " response")),
    ];
    let mut state = state();
    state.preview_request_id = Some("preview-request".into());
    state.active_turn = Some("parent-work".into());
    let parent_entries = state.transcript.len();
    let mut unsolicited = preview(draft_events.clone());
    unsolicited.event.submission_id = None;
    events::handle_gateway_event(&mut state, unsolicited);
    assert!(state.preview.is_none());
    events::handle_gateway_event(&mut state, preview(draft_events));
    assert_eq!(
        preview_messages(snapshot(&state)),
        ["› Earlier call", "› Hel", "First response", "Second"]
    );

    let mut completed = preview(vec![
        rendered(
            "user-1",
            EventMsg::MessageDelta(mobius::protocol::MessageDeltaEvent {
                text: "Hello".into(),
            }),
        ),
        rendered("user-1", user("Hello")),
        rendered(
            "assistant-1",
            final_answer("assistant-1", "First response corrected"),
        ),
        rendered(
            "assistant-2",
            final_answer("assistant-2", "Second response"),
        ),
    ]);
    completed.event.submission_id = None;
    events::handle_gateway_event(&mut state, completed);
    assert_eq!(
        preview_messages(snapshot(&state)),
        [
            "› Earlier call",
            "› Hello",
            "First response corrected",
            "Second response"
        ]
    );
    assert!(
        snapshot(&state)
            .transcript
            .iter()
            .all(|entry| !entry.pending)
    );
    assert_eq!(state.transcript.len(), parent_entries);
    assert_eq!(state.active_turn.as_deref(), Some("parent-work"));
}

#[test]
fn live_transcript_preview_uses_a_centered_popup_and_new_entries() {
    let mut state = state();
    state.transcript.clear();
    state.open_transcript_preview();
    state.push_entry("new live row".into(), TranscriptTone::Neutral);
    let mut terminal = Terminal::new(TestBackend::new(40, 16)).expect("terminal");

    terminal
        .draw(|frame| view::render_preview(frame, &mut state))
        .expect("preview draw");
    let rendered = terminal.backend().to_string();
    let lines = rendered.lines().collect::<Vec<_>>();

    assert!(rendered.contains("new live row"));
    assert!(
        lines.first().is_some_and(|line| !line.contains('┌')),
        "{rendered}"
    );
    assert!(
        lines.iter().skip(1).any(|line| line.contains('┌')),
        "{rendered}"
    );
}

#[test]
fn snapshot_preview_scrolls_with_the_mouse_wheel() {
    let mut state = state();
    state.preview_request_id = Some("preview-request".into());
    events::handle_gateway_event(
        &mut state,
        recorded(
            EventMsg::ContextCompacted,
            Vec::new(),
            Some(RenderedPreview {
                id: "/root/subagent".into(),
                title: "subagent".into(),
                subtitle: String::new(),
                page_id: "subagent:latest".into(),
                update: mobius::protocol::FrontendPreviewUpdate::Replace,
                events: (0..30)
                    .map(|index| RenderedEvent {
                        submission_id: None,
                        recorded_at_ms: i64::from(index),
                        event: EventMsg::Message(MessageEvent {
                            author: MessageAuthor::User,
                            delivery: MessageDelivery::Turn,
                            text: format!("subagent row {index}"),
                            attachments: Vec::new(),
                            reply: None,
                            message_target: None,
                        }),
                        blocks: Vec::new(),
                    })
                    .collect(),
                next: None,
            }),
        ),
    );
    let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("terminal");
    terminal
        .draw(|frame| view::render_preview(frame, &mut state))
        .expect("preview draw");
    let bottom = terminal.backend().to_string();
    assert!(bottom.contains("subagent row 29"), "{bottom}");

    assert!(!state.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(state.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }));
    terminal
        .draw(|frame| view::render_preview(frame, &mut state))
        .expect("scrolled preview draw");
    let scrolled = terminal.backend().to_string();
    assert!(scrolled.contains("subagent row 25"), "{scrolled}");
    assert!(!scrolled.contains("subagent row 29"), "{scrolled}");

    state.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    terminal
        .draw(|frame| view::render_preview(frame, &mut state))
        .expect("restored preview draw");
    let restored = terminal.backend().to_string();
    assert!(restored.contains("subagent row 29"), "{restored}");
}

#[test]
fn snapshot_preview_prepends_an_older_page_without_losing_the_latest_page() {
    let mut state = state();
    state.preview_request_id = Some("preview-request".into());
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/agent",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["latest"],
            Some(preview_continuation("older")),
        ),
    );
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/agent",
            "older",
            FrontendPreviewUpdate::Prepend,
            &["older"],
            None,
        ),
    );

    let snapshot = snapshot(&state);
    assert_eq!(
        (
            snapshot.id.as_str(),
            snapshot.page_ids.len(),
            preview_messages(snapshot),
            snapshot.next.as_ref(),
        ),
        ("/root/agent", 2, vec!["› older", "› latest"], None,)
    );
}

#[test]
fn snapshot_preview_deduplicates_page_ids() {
    let mut state = state();
    state.preview_request_id = Some("preview-request".into());
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/agent",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["latest"],
            None,
        ),
    );
    for message in ["older", "duplicate"] {
        events::handle_gateway_event(
            &mut state,
            preview_record(
                "/root/agent",
                "older",
                FrontendPreviewUpdate::Prepend,
                &[message],
                None,
            ),
        );
    }

    assert_eq!(
        preview_messages(snapshot(&state)),
        vec!["› older", "› latest"]
    );
}

#[test]
fn snapshot_preview_replace_refreshes_a_reused_latest_page_id() {
    let mut state = state();
    for message in ["stale", "fresh"] {
        state.preview_request_id = Some("preview-request".into());
        events::handle_gateway_event(
            &mut state,
            preview_record(
                "/root/agent",
                "latest",
                FrontendPreviewUpdate::Replace,
                &[message],
                None,
            ),
        );
    }

    assert_eq!(preview_messages(snapshot(&state)), vec!["› fresh"]);
}

#[test]
fn snapshot_preview_does_not_merge_a_page_for_another_preview_id() {
    let mut state = state();
    state.preview_request_id = Some("preview-request".into());
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/agent",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["latest"],
            None,
        ),
    );
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/other",
            "older",
            FrontendPreviewUpdate::Prepend,
            &["wrong preview"],
            None,
        ),
    );

    let snapshot = snapshot(&state);
    assert_eq!(
        (snapshot.id.as_str(), preview_messages(snapshot)),
        ("/root/agent", vec!["› latest"])
    );
}

#[test]
fn snapshot_preview_older_key_submits_the_retained_continuation() {
    let mut state = state();
    state.preview_request_id = Some("preview-request".into());
    let next = preview_continuation("older");
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/agent",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["latest"],
            Some(next.clone()),
        ),
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 10)).expect("terminal");
    terminal
        .draw(|frame| view::render_preview(frame, &mut state))
        .expect("preview draw");
    let rendered = terminal.backend().to_string();
    let action = state.handle_key(
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
        &default_catalog(),
    );
    let retained = snapshot(&state).next.as_ref();

    assert_eq!(
        (rendered.contains("O older"), action, retained),
        (true, UiAction::Submit(next.clone()), Some(&next))
    );
}
