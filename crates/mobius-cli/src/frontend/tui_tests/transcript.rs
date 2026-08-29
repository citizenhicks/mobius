use super::support::*;
use super::*;

#[test]
fn transcript_keeps_a_bounded_recent_window() {
    let mut state = state();
    state.transcript.clear();
    for index in 0..=MAX_TRANSCRIPT_ENTRIES {
        state.push_entry(format!("message {index}"), TranscriptTone::Neutral);
    }

    assert_eq!(state.transcript.len(), MAX_TRANSCRIPT_ENTRIES);
    assert_eq!(
        state.transcript.front().map(|entry| entry.text.as_str()),
        Some("message 1")
    );
}

#[test]
fn commentary_and_final_output_are_separate_assistant_messages() {
    let mut state = state();
    state.active_turn = Some("turn".into());
    state.handle_agent_event(
        EventMsg::AssistantContentDelta(mobius::protocol::AssistantContentDeltaEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "commentary".into(),
            delta: "Checking the workspace".into(),
            phase: ModelStepContentPhase::Commentary,
        }),
        Vec::new(),
    );

    assert_eq!(state.streaming, "Checking the workspace");
    assert_eq!(
        state.streaming_phase,
        Some(ModelStepContentPhase::Commentary)
    );

    state.handle_agent_event(
        EventMsg::AssistantMessage(mobius::protocol::AssistantMessageEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "final".into(),
            content: vec![ModelStepContent {
                output_index: 0,
                part_index: 0,
                phase: ModelStepContentPhase::FinalAnswer,
                text: "Done".into(),
                annotations: Vec::new(),
            }],
            message_target: None,
        }),
        Vec::new(),
    );

    assert!(state.streaming.is_empty());
    assert_eq!(state.streaming_phase, None);
    assert_eq!(
        state
            .transcript
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["Checking the workspace", "Done"]
    );
    assert!(
        state
            .transcript
            .iter()
            .all(|entry| matches!(entry.tone, TranscriptTone::Assistant))
    );
}

#[test]
fn commentary_is_committed_before_a_tool_block() {
    let mut state = state();
    state.handle_agent_event(
        EventMsg::AssistantContentDelta(mobius::protocol::AssistantContentDeltaEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "response".into(),
            delta: "I’ll inspect the file first.".into(),
            phase: ModelStepContentPhase::Commentary,
        }),
        Vec::new(),
    );

    state.handle_agent_event(
        EventMsg::ToolCallBegin(mobius::protocol::ToolCallBeginEvent {
            turn_id: "turn".into(),
            call_id: "call".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        }),
        vec![rendered(FrontendBlock {
            id: Some("tool-call".into()),
            group: None,
            update: FrontendBlockUpdate::Replace,
            state: FrontendBlockState::Pending,
            role: FrontendBlockRole::Tool,
            title: "Read src/lib.rs".into(),
            text: String::new(),
            symbol: None,
            format: FrontendBlockFormat::PlainText,
            tone: FrontendTone::Neutral,
            files: Vec::new(),
        })],
    );

    assert_eq!(
        state
            .transcript
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["I’ll inspect the file first.", "Read src/lib.rs"]
    );
}

#[test]
fn durable_commentary_is_an_assistant_message() {
    let mut state = state();
    state.handle_agent_event(
        EventMsg::AssistantMessage(mobius::protocol::AssistantMessageEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "commentary".into(),
            content: vec![ModelStepContent {
                output_index: 0,
                part_index: 0,
                phase: ModelStepContentPhase::Commentary,
                text: "The first check passed.".into(),
                annotations: Vec::new(),
            }],
            message_target: None,
        }),
        Vec::new(),
    );

    let entry = state
        .transcript
        .back()
        .expect("commentary transcript entry");
    assert_eq!(entry.text, "The first check passed.");
    assert!(matches!(entry.tone, TranscriptTone::Assistant));
}

#[test]
fn final_message_replaces_an_incomplete_stream() {
    let mut state = state();
    state.handle_agent_event(
        EventMsg::AssistantContentDelta(mobius::protocol::AssistantContentDeltaEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "final".into(),
            delta: "partial".into(),
            phase: ModelStepContentPhase::FinalAnswer,
        }),
        Vec::new(),
    );

    state.handle_agent_event(
        EventMsg::AssistantMessage(mobius::protocol::AssistantMessageEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "final".into(),
            content: vec![ModelStepContent {
                output_index: 0,
                part_index: 0,
                phase: ModelStepContentPhase::FinalAnswer,
                text: "complete answer".into(),
                annotations: Vec::new(),
            }],
            message_target: None,
        }),
        Vec::new(),
    );

    assert!(state.streaming.is_empty());
    assert_eq!(state.transcript.len(), 1);
    assert_eq!(
        state.transcript.back().map(|entry| entry.text.as_str()),
        Some("complete answer")
    );
}

#[test]
fn assistant_message_is_authoritative_for_replay_and_summary_events() {
    use mobius::protocol::{
        ModelStepCompletedEvent, ModelStepContent, ModelStepContentPhase, ModelStepOutcome,
        TokenUsage,
    };

    let mut state = state();
    state.handle_agent_event(
        EventMsg::ModelStepCompleted(ModelStepCompletedEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "step".into(),
            step_index: 0,
            started_at_ms: 1,
            completed_at_ms: 2,
            outcome: ModelStepOutcome::Completed {
                end_turn: true,
                tool_call_ids: Vec::new(),
                usage: TokenUsage::default(),
            },
            diagnostics: None,
        }),
        Vec::new(),
    );
    state.handle_agent_event(
        EventMsg::AssistantMessage(mobius::protocol::AssistantMessageEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "step".into(),
            content: vec![
                ModelStepContent {
                    output_index: 0,
                    part_index: 0,
                    phase: ModelStepContentPhase::Reasoning,
                    text: "Checked the state".into(),
                    annotations: Vec::new(),
                },
                ModelStepContent {
                    output_index: 1,
                    part_index: 0,
                    phase: ModelStepContentPhase::FinalAnswer,
                    text: "Everything is ready".into(),
                    annotations: Vec::new(),
                },
            ],
            message_target: None,
        }),
        Vec::new(),
    );

    assert_eq!(
        state
            .transcript
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["Checked the state", "Everything is ready"]
    );
}

#[test]
fn retrying_model_step_closes_pending_search_and_marks_the_reconnect() {
    use mobius::protocol::{
        ModelStepCompletedEvent, ModelStepOutcome, WebSearchAction, WebSearchBeginEvent,
        WebSearchEndEvent,
    };

    let mut state = state();
    let search = EventMsg::WebSearchBegin(WebSearchBeginEvent {
        session_id: "session".into(),
        turn_id: "turn".into(),
        model_step_id: "step".into(),
        call_id: "search".into(),
    });
    state.handle_agent_event(
        EventMsg::AssistantContentDelta(mobius::protocol::AssistantContentDeltaEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "step".into(),
            delta: "Partial answer".into(),
            phase: ModelStepContentPhase::FinalAnswer,
        }),
        Vec::new(),
    );
    state.handle_agent_event(search.clone(), search.presentation().into_iter().collect());
    // The backend closes a search its step can never finish; the protocol record,
    // not a frontend rule, carries the interrupted presentation.
    let end = EventMsg::WebSearchEnd(WebSearchEndEvent {
        session_id: "session".into(),
        turn_id: "turn".into(),
        model_step_id: "step".into(),
        call_id: "search".into(),
        action: WebSearchAction::Interrupted,
    });
    state.handle_agent_event(end.clone(), end.presentation().into_iter().collect());
    let retry = EventMsg::ModelStepCompleted(ModelStepCompletedEvent {
        session_id: "session".into(),
        turn_id: "turn".into(),
        model_step_id: "step".into(),
        step_index: 0,
        started_at_ms: 1,
        completed_at_ms: 2,
        outcome: ModelStepOutcome::Retrying,
        diagnostics: None,
    });
    state.handle_agent_event(retry.clone(), retry.presentation().into_iter().collect());

    assert!(state.transcript.iter().all(|entry| !entry.pending));
    assert!(
        state
            .transcript
            .iter()
            .any(|entry| entry.text == "Web search interrupted"
                && matches!(entry.tone, TranscriptTone::Warning))
    );
    assert!(
        state
            .transcript
            .iter()
            .any(|entry| entry.text.contains("Reconnecting"))
    );
    let texts = state
        .transcript
        .iter()
        .map(|entry| entry.text.as_str())
        .collect::<Vec<_>>();
    let partial = texts
        .iter()
        .position(|text| *text == "Partial answer")
        .expect("partial answer");
    let reconnecting = texts
        .iter()
        .position(|text| text.contains("Reconnecting"))
        .expect("reconnecting notice");
    assert!(partial < reconnecting);
}

#[test]
fn failed_model_step_clears_pending_blocks_without_an_end_event() {
    use mobius::protocol::{ModelStepCompletedEvent, ModelStepOutcome, WebSearchBeginEvent};

    // Replay after a crash journals no end event, so the step-namespaced sweep
    // must not strand the pending block.
    let mut state = state();
    let search = EventMsg::WebSearchBegin(WebSearchBeginEvent {
        session_id: "session".into(),
        turn_id: "turn".into(),
        model_step_id: "step".into(),
        call_id: "search".into(),
    });
    state.handle_agent_event(search.clone(), search.presentation().into_iter().collect());
    let failed = EventMsg::ModelStepCompleted(ModelStepCompletedEvent {
        session_id: "session".into(),
        turn_id: "turn".into(),
        model_step_id: "step".into(),
        step_index: 0,
        started_at_ms: 1,
        completed_at_ms: 2,
        outcome: ModelStepOutcome::Failed,
        diagnostics: None,
    });
    state.handle_agent_event(failed.clone(), failed.presentation().into_iter().collect());

    let entry = state
        .transcript
        .iter()
        .find(|entry| entry.text == "Searching the web")
        .expect("search entry");
    assert!(!entry.pending);
    assert!(matches!(entry.tone, TranscriptTone::Warning));
}

#[test]
fn completed_model_step_does_not_duplicate_live_streams() {
    use mobius::protocol::{
        ModelStepCompletedEvent, ModelStepContent, ModelStepContentPhase, ModelStepOutcome,
        TokenUsage,
    };

    let mut state = state();
    state.handle_agent_event(
        EventMsg::AssistantContentDelta(mobius::protocol::AssistantContentDeltaEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "step".into(),
            delta: "Everything is ready".into(),
            phase: ModelStepContentPhase::FinalAnswer,
        }),
        Vec::new(),
    );
    state.handle_agent_event(
        EventMsg::ModelStepCompleted(ModelStepCompletedEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "step".into(),
            step_index: 0,
            started_at_ms: 1,
            completed_at_ms: 2,
            outcome: ModelStepOutcome::Completed {
                end_turn: true,
                tool_call_ids: Vec::new(),
                usage: TokenUsage::default(),
            },
            diagnostics: None,
        }),
        Vec::new(),
    );
    state.handle_agent_event(
        EventMsg::AssistantMessage(mobius::protocol::AssistantMessageEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "step".into(),
            content: vec![ModelStepContent {
                output_index: 0,
                part_index: 0,
                phase: ModelStepContentPhase::FinalAnswer,
                text: "Everything is ready".into(),
                annotations: Vec::new(),
            }],
            message_target: None,
        }),
        Vec::new(),
    );

    assert_eq!(state.transcript.len(), 1);
    assert_eq!(
        state.transcript.back().map(|entry| entry.text.as_str()),
        Some("Everything is ready")
    );
}
