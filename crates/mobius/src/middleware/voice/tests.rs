use super::*;
use crate::protocol::{
    AssistantMessageEvent, MessageEvent, ModelStepContent, TurnCompleteEvent, TurnStartedEvent,
};

fn event(id: &str, msg: EventMsg) -> Event {
    Event {
        submission_id: Some(id.into()),
        msg,
    }
}

fn reply(mut commands: Vec<RealtimeVoiceCommand>) -> (String, String) {
    assert_eq!(commands.len(), 1);
    match commands.pop().expect("voice result") {
        RealtimeVoiceCommand::Reply { handoff_id, text } => (handoff_id, text),
        RealtimeVoiceCommand::Context { .. } | RealtimeVoiceCommand::Close => {
            panic!("expected handoff reply")
        }
    }
}

#[test]
fn voice_submits_once_and_only_speaks_its_committed_complete_answer() {
    let mut voice = VoiceConversation::new(
        "voice-session".parse().expect("session ID"),
        None,
        "Builder".into(),
    );
    let submission = voice
        .handoff("audio-1".into(), "Help me".into())
        .unwrap()
        .unwrap()
        .into_submission();
    assert!(
        voice
            .handoff("audio-1".into(), "duplicate".into())
            .unwrap()
            .is_none()
    );
    let Op::Message { message } = &submission.op else {
        panic!("normal message")
    };
    assert_eq!(
        message.requested_delivery,
        Some(crate::protocol::ActiveMessageDelivery::Steer)
    );
    assert!(
        matches!(&message.author, MessageAuthor::Source { source: crate::protocol::MessageSource::Call { call_id }, handle, symbol: Some(FrontendSymbol::Voice), .. }
            if &**call_id == "voice-session" && &**handle == "Builder (voice)")
    );
    let started = EventMsg::TurnStarted(TurnStartedEvent {
        idle: false,
        turn_id: "turn".into(),
        model_context_window: None,
    });
    assert!(voice.observe(&event("another", started.clone())).is_empty());
    assert!(voice.observe(&event(&submission.id, started)).is_empty());
    let answer = EventMsg::AssistantMessage(AssistantMessageEvent {
        session_id: "session".parse().expect("session ID"),
        turn_id: "turn".into(),
        model_step_id: "step".into(),
        message_target: None,
        content: ["First part", "Second part"]
            .into_iter()
            .enumerate()
            .map(|(index, text)| ModelStepContent {
                output_index: index,
                part_index: 0,
                phase: ModelStepContentPhase::FinalAnswer,
                text: text.into(),
                annotations: Vec::new(),
            })
            .collect(),
    });
    assert!(voice.observe(&event(&submission.id, answer)).is_empty());
    let complete = event(
        &submission.id,
        EventMsg::TurnComplete(TurnCompleteEvent {
            idle: false,
            turn_id: "turn".into(),
        }),
    );
    assert_eq!(
        reply(voice.observe(&complete)),
        ("audio-1".into(), "First part\nSecond part".into())
    );
    assert!(voice.observe(&complete).is_empty());
    assert!(
        voice
            .handoff("audio-1".into(), "late duplicate".into())
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_promoted_idle_turn_completes_a_voice_handoff_attached_without_an_active_turn() {
    let mut voice = VoiceConversation::new(
        "voice-session".parse().expect("session ID"),
        None,
        "Builder".into(),
    );
    let submission = voice
        .handoff("audio-idle".into(), "Help me".into())
        .expect("handoff")
        .expect("new handoff")
        .into_submission();
    assert!(voice.active_turn_id.is_none());
    let started = event(
        "idle-submission",
        EventMsg::TurnStarted(TurnStartedEvent {
            idle: false,
            turn_id: "idle-turn".into(),
            model_context_window: None,
        }),
    );
    assert!(voice.observe(&started).is_empty());
    let Op::Message { message } = submission.op else {
        panic!("normal message")
    };
    let accepted = event(
        &submission.id,
        EventMsg::Message(MessageEvent {
            author: message.author,
            delivery: MessageDelivery::Steer,
            text: message.text,
            attachments: message.attachments,
            reply: message.reply,
            message_target: None,
        }),
    );
    assert!(voice.observe(&accepted).is_empty());
    let answer = event(
        "idle-submission",
        EventMsg::AssistantMessage(AssistantMessageEvent {
            session_id: "session".parse().expect("session ID"),
            turn_id: "idle-turn".into(),
            model_step_id: "step".into(),
            message_target: None,
            content: vec![ModelStepContent {
                output_index: 0,
                part_index: 0,
                phase: ModelStepContentPhase::FinalAnswer,
                text: "Task complete".into(),
                annotations: Vec::new(),
            }],
        }),
    );
    assert!(voice.observe(&answer).is_empty());
    let complete = event(
        "idle-submission",
        EventMsg::TurnComplete(TurnCompleteEvent {
            idle: false,
            turn_id: "idle-turn".into(),
        }),
    );
    assert_eq!(
        reply(voice.observe(&complete)),
        ("audio-idle".into(), "Task complete".into())
    );
    assert!(voice.observe(&complete).is_empty());
}

#[test]
fn tail_keeps_complete_characters_within_the_byte_budget() {
    assert_eq!(tail("a🗣é", 0), "");
    assert_eq!(tail("a🗣é", 1), "");
    assert_eq!(tail("a🗣é", 3), "é");
    assert_eq!(tail("a🗣é", 6), "🗣é");
    assert_eq!(tail("a🗣é", usize::MAX), "a🗣é");
}
