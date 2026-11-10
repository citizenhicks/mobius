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
fn gateway_block_appends_extend_the_drawn_entry() {
    use mobius_gateway::wire::{BlockDelta, TranscriptOp};
    let mut state = state();
    let item = TranscriptItem {
        id: "block:tools:run".into(),
        turn_id: None,
        recorded_at_ms: 0,
        state: TranscriptItemState::Pending,
        terminal: false,
        starts_turn: false,
        resumed: false,
        inherited: false,
        target: None,
        content: TranscriptItemContent::Block {
            capability: "tools".into(),
            block: FrontendBlock {
                id: Some("run".into()),
                state: mobius::protocol::FrontendBlockState::Pending,
                role: FrontendBlockRole::Tool,
                title: "Bash · make".into(),
                text: "make".into(),
                ..FrontendBlock::default()
            },
            group_title: None,
        },
    };
    let event = || recorded(EventMsg::ContextCompacted, None);
    events::handle_gateway_event(
        &mut state,
        event(),
        vec![TranscriptOp::Item {
            item: Arc::new(item),
        }],
        true,
    );
    events::handle_gateway_event(
        &mut state,
        event(),
        vec![TranscriptOp::AppendBlock {
            id: "block:tools:run".into(),
            delta: BlockDelta {
                text: "\nbuilt".into(),
                state: Some(mobius::protocol::FrontendBlockState::Complete),
                ..BlockDelta::default()
            },
        }],
        true,
    );
    let entry = state.transcript.back().expect("the block entry");
    assert_eq!(
        (entry.text.as_str(), entry.detail.as_deref(), entry.pending),
        ("make\nbuilt", None, false)
    );
}

#[test]
fn a_snapshot_answer_continues_under_the_same_item_id() {
    use mobius::protocol::{AssistantContentDeltaEvent, AssistantMessageEvent};
    use mobius_gateway::wire::TranscriptOp;

    let mut state = state();
    let mut item = TranscriptItem {
        id: "assistant:step:final_answer".into(),
        turn_id: Some("turn".into()),
        recorded_at_ms: 0,
        state: TranscriptItemState::Streaming,
        terminal: false,
        starts_turn: false,
        resumed: false,
        inherited: false,
        target: None,
        content: TranscriptItemContent::Assistant {
            phase: ModelStepContentPhase::FinalAnswer,
            text: "Hel".into(),
            annotations: Vec::new(),
        },
    };
    events::handle_gateway_history(
        &mut state,
        TranscriptPage {
            items: vec![Arc::new(item.clone())],
            rows: Vec::new(),
        },
    );
    events::handle_gateway_event(
        &mut state,
        recorded(
            EventMsg::AssistantContentDelta(AssistantContentDeltaEvent {
                session_id: mobius::SessionId::new("session").unwrap(),
                turn_id: "turn".into(),
                model_step_id: "step".into(),
                phase: ModelStepContentPhase::FinalAnswer,
                delta: "lo".into(),
            }),
            None,
        ),
        vec![TranscriptOp::AppendText {
            id: Arc::clone(&item.id),
            text: "lo".into(),
        }],
        true,
    );
    assert_eq!(state.transcript.len(), 1);
    assert_eq!(state.transcript.front().unwrap().text, "Hello");
    item.state = TranscriptItemState::Complete;
    item.content = TranscriptItemContent::Assistant {
        phase: ModelStepContentPhase::FinalAnswer,
        text: "Hello!".into(),
        annotations: Vec::new(),
    };
    events::handle_gateway_event(
        &mut state,
        recorded(
            EventMsg::AssistantMessage(AssistantMessageEvent {
                session_id: mobius::SessionId::new("session").unwrap(),
                turn_id: "turn".into(),
                model_step_id: "step".into(),
                content: vec![ModelStepContent {
                    output_index: 0,
                    part_index: 0,
                    phase: ModelStepContentPhase::FinalAnswer,
                    text: "Hello!".into(),
                    annotations: Vec::new(),
                }],
                message_target: None,
            }),
            None,
        ),
        vec![TranscriptOp::Item {
            item: Arc::new(item),
        }],
        true,
    );
    assert_eq!(state.transcript.len(), 1);
    assert_eq!(state.transcript.front().unwrap().text, "Hello!");
}
