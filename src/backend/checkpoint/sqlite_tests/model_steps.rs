use super::*;

#[test]
fn stream_metrics_tolerate_wall_clock_regression() {
    let mut metrics = StreamMetricAccumulator::default();
    metrics.observe(20, 2).expect("first chunk");
    metrics.observe(10, 3).expect("regressed clock chunk");

    assert_eq!(
        metrics.finish(ModelStepContentPhase::Reasoning),
        Some(StreamMetrics {
            phase: ModelStepContentPhase::Reasoning,
            first_delta_at_ms: 20,
            last_delta_at_ms: 20,
            chunk_count: 2,
            utf8_bytes: 5,
            longest_gap_ms: 0,
        })
    );
}

#[tokio::test]
async fn assistant_message_compacts_progressive_deltas_and_preserves_citations() {
    use crate::protocol::AssistantContentDeltaEvent;
    use crate::protocol::AssistantMessageEvent;
    use crate::protocol::ModelStepAnnotation;
    use crate::protocol::ModelStepCompletedEvent;
    use crate::protocol::ModelStepContent;
    use crate::protocol::ModelStepContentPhase;
    use crate::protocol::ModelStepStartedEvent;

    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    store
        .save(&checkpoint("session"), &[], None)
        .await
        .expect("save session");
    let event = |msg| Event {
        submission_id: Some("submission".into()),
        msg,
    };
    let events = [
        event(EventMsg::ModelStepStarted(ModelStepStartedEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "step".into(),
            step_index: 0,
            started_at_ms: 10,
        })),
        event(EventMsg::AssistantContentDelta(
            AssistantContentDeltaEvent {
                session_id: "session".into(),
                turn_id: "turn".into(),
                model_step_id: "step".into(),
                delta: "Plan".into(),
                phase: ModelStepContentPhase::Reasoning,
            },
        )),
        event(EventMsg::AssistantContentDelta(
            AssistantContentDeltaEvent {
                session_id: "session".into(),
                turn_id: "turn".into(),
                model_step_id: "step".into(),
                delta: "Done".into(),
                phase: ModelStepContentPhase::FinalAnswer,
            },
        )),
        event(EventMsg::ModelStepCompleted(ModelStepCompletedEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "step".into(),
            step_index: 0,
            started_at_ms: 10,
            completed_at_ms: 20,
            outcome: ModelStepOutcome::Completed {
                end_turn: true,
                tool_call_ids: Vec::new(),
                usage: crate::protocol::TokenUsage::default(),
            },
            diagnostics: None,
        })),
        event(EventMsg::AssistantMessage(AssistantMessageEvent {
            session_id: "session".into(),
            turn_id: "turn".into(),
            model_step_id: "step".into(),
            content: vec![
                ModelStepContent {
                    output_index: 0,
                    part_index: 0,
                    phase: ModelStepContentPhase::Reasoning,
                    text: "Plan".into(),
                    annotations: Vec::new(),
                },
                ModelStepContent {
                    output_index: 1,
                    part_index: 0,
                    phase: ModelStepContentPhase::FinalAnswer,
                    text: "Done".into(),
                    annotations: vec![ModelStepAnnotation::UrlCitation {
                        url: "https://example.com".into(),
                        title: "Example".into(),
                        content: Some("Relevant excerpt.".into()),
                        start_index: 0,
                        end_index: 4,
                    }],
                },
            ],
            message_target: None,
        })),
    ];
    for (index, event) in events.iter().enumerate() {
        store
            .append_event(
                "session",
                10 + i64::try_from(index).expect("timestamp"),
                event,
            )
            .await
            .expect("append event");
    }

    let page = store
        .event_page(
            "session",
            EventPageRequest {
                before_sequence: None,
                limit: 10,
            },
        )
        .await
        .expect("event page")
        .into_chronological();

    assert_eq!(
        page.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        [1, 4, 5]
    );
    let EventMsg::AssistantMessage(AssistantMessageEvent { content, .. }) = &page[2].event.msg
    else {
        panic!("expected assistant message");
    };
    assert_eq!(
        content,
        &[
            ModelStepContent {
                output_index: 0,
                part_index: 0,
                phase: ModelStepContentPhase::Reasoning,
                text: "Plan".into(),
                annotations: Vec::new(),
            },
            ModelStepContent {
                output_index: 1,
                part_index: 0,
                phase: ModelStepContentPhase::FinalAnswer,
                text: "Done".into(),
                annotations: vec![ModelStepAnnotation::UrlCitation {
                    url: "https://example.com".into(),
                    title: "Example".into(),
                    content: Some("Relevant excerpt.".into()),
                    start_index: 0,
                    end_index: 4,
                }],
            },
        ]
    );
    assert_eq!(
        page[1]
            .stream_metrics
            .iter()
            .map(|metrics| (metrics.phase, metrics.chunk_count, metrics.utf8_bytes))
            .collect::<Vec<_>>(),
        [
            (ModelStepContentPhase::Reasoning, 1, 4),
            (ModelStepContentPhase::FinalAnswer, 1, 4),
        ]
    );
}

#[tokio::test]
async fn incomplete_model_steps_retain_progressive_deltas() {
    use crate::protocol::AssistantContentDeltaEvent;
    use crate::protocol::ModelStepCompletedEvent;

    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    store
        .save(&checkpoint("session"), &[], None)
        .await
        .expect("save session");
    for (model_step_id, outcome) in [
        ("failed", ModelStepOutcome::Failed),
        ("interrupted", ModelStepOutcome::Interrupted),
        ("retrying", ModelStepOutcome::Retrying),
    ] {
        let delta = Event {
            submission_id: Some("submission".into()),
            msg: EventMsg::AssistantContentDelta(AssistantContentDeltaEvent {
                session_id: "session".into(),
                turn_id: "turn".into(),
                model_step_id: model_step_id.into(),
                delta: format!("partial {model_step_id}"),
                phase: crate::protocol::ModelStepContentPhase::FinalAnswer,
            }),
        };
        let completed = Event {
            submission_id: Some("submission".into()),
            msg: EventMsg::ModelStepCompleted(ModelStepCompletedEvent {
                session_id: "session".into(),
                turn_id: "turn".into(),
                model_step_id: model_step_id.into(),
                step_index: 0,
                started_at_ms: 10,
                completed_at_ms: 20,
                outcome,
                diagnostics: None,
            }),
        };
        store
            .append_event("session", 10, &delta)
            .await
            .expect("append partial delta");
        store
            .append_event("session", 20, &completed)
            .await
            .expect("append incomplete terminal event");
    }

    let page = store
        .event_page(
            "session",
            EventPageRequest {
                before_sequence: None,
                limit: 10,
            },
        )
        .await
        .expect("event page")
        .into_chronological();

    assert_eq!(
        page.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
    assert!(matches!(
        page[0].event.msg,
        EventMsg::AssistantContentDelta(_)
    ));
    assert!(matches!(
        page[2].event.msg,
        EventMsg::AssistantContentDelta(_)
    ));
    assert!(matches!(
        page[4].event.msg,
        EventMsg::AssistantContentDelta(_)
    ));
}
