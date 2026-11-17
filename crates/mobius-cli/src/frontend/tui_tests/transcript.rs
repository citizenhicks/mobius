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
    let file = |size| SessionFileReference {
        id: "f".into(),
        name: "f".into(),
        size,
        media_type: "text/plain".into(),
    };
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
                files: vec![file(1)],
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
                files: vec![file(2)],
                state: Some(mobius::protocol::FrontendBlockState::Complete),
                ..BlockDelta::default()
            },
        }],
        true,
    );
    let entry = state.transcript.back().expect("the block entry");
    assert_eq!(
        (entry.text.as_str(), entry.detail.as_deref(), entry.pending),
        ("make\nbuilt\n[file] f · text/plain · 2 bytes", None, false),
        "the file is replaced by id, as reopening the chat shows it"
    );
}

#[test]
fn older_history_at_the_cap_stays_shown_and_evicted_blocks_are_forgotten() {
    let block = |id: &str| {
        Arc::new(TranscriptItem {
            id: format!("block:tools:{id}").into(),
            turn_id: None,
            recorded_at_ms: 0,
            state: TranscriptItemState::Complete,
            terminal: false,
            starts_turn: false,
            resumed: false,
            inherited: false,
            target: None,
            content: TranscriptItemContent::Block {
                capability: "tools".into(),
                block: FrontendBlock {
                    id: Some(id.into()),
                    role: FrontendBlockRole::Tool,
                    text: id.into(),
                    ..FrontendBlock::default()
                },
                group_title: None,
            },
        })
    };
    let mut state = state();
    state.transcript.clear();
    for index in 1..MAX_TRANSCRIPT_ENTRIES {
        state.push_entry(format!("message {index}"), TranscriptTone::Neutral);
    }
    events::handle_gateway_event(
        &mut state,
        recorded(EventMsg::ContextCompacted, None),
        vec![mobius_gateway::wire::TranscriptOp::Item {
            item: block("newest"),
        }],
        true,
    );
    assert_eq!(state.transcript.len(), MAX_TRANSCRIPT_ENTRIES);

    let mut page = message_page(&["older"]);
    page.items.push(block("older"));
    events::handle_gateway_history(&mut state, page, Evict::Newest);

    assert_eq!(state.transcript.len(), MAX_TRANSCRIPT_ENTRIES);
    assert_eq!(
        state.transcript.front().map(|entry| entry.text.as_str()),
        Some("› older")
    );
    let remembered: Vec<&str> = state.block_items.keys().map(|id| &**id).collect();
    assert_eq!(
        remembered,
        ["block:tools:older"],
        "only shown blocks take appends"
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
        Evict::Oldest,
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

#[test]
fn an_authentication_failure_shows_once_with_the_login_hint() {
    use mobius::protocol::{ErrorEvent, ErrorKind, TurnAbortedEvent};
    let mut state = state();
    state.transcript.clear();
    let message = "authentication error: the selected provider is not configured";
    let error = EventMsg::Error(ErrorEvent {
        kind: ErrorKind::Authentication,
        message: message.into(),
        retryable: false,
        status: None,
        retry_after: None,
        failure: None,
    });
    let aborted = EventMsg::TurnAborted(TurnAbortedEvent {
        idle: false,
        turn_id: "turn".into(),
        reason: message.into(),
    });
    for event in [error, aborted] {
        let blocks = event.presentation().into_iter().collect();
        state.handle_agent_event(event, blocks);
    }

    let texts: Vec<&str> = state
        .transcript
        .iter()
        .map(|entry| entry.text.as_str())
        .collect();
    assert_eq!(
        texts,
        [message, "set up or switch the provider with /login"]
    );
}
