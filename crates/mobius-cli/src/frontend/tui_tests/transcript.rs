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
