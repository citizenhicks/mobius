use std::sync::Mutex;

use super::*;
use crate::backend::checkpoint::SessionPageRequest;
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::middleware::messages::Messages;
use crate::middleware::{ActiveCommandContext, MessageQueue, Middleware, SubmissionResult};

#[tokio::test]
async fn handoff_cursor_advances_only_on_acknowledgement_and_survives_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3")).unwrap());
    let mut parent = Checkpoint::empty("parent");
    parent.session_context.owner_id = "bot".into();
    checkpoints.save(&parent, &[], None).await.unwrap();
    let sink: FrontendEventSink = Arc::new(|_| Ok(()));
    let mut voice = VoiceTranscript::open(Arc::clone(&checkpoints), "parent", Arc::clone(&sink))
        .await
        .unwrap();
    voice
        .record(
            "plan",
            ConversationRole::Assistant,
            "Use blue; preserve the toolbar.",
            true,
        )
        .await
        .unwrap();
    voice
        .record("request", ConversationRole::User, "Do that.", true)
        .await
        .unwrap();
    let first = voice.handoff_context().await.unwrap();
    assert!(first.text.contains("Use blue; preserve the toolbar."));
    assert!(first.text.contains("Do that."));
    // An unsubmitted or rejected snapshot must not consume any discussion.
    let retry = voice.handoff_context().await.unwrap();
    assert_eq!(retry.text, first.text);
    voice
        .record(
            "later",
            ConversationRole::User,
            "Also use large text.",
            true,
        )
        .await
        .unwrap();
    voice.acknowledge(first).await.unwrap();
    let second = voice.handoff_context().await.unwrap();
    assert_eq!(second.text, "User: Also use large text.");
    voice.acknowledge(second).await.unwrap();
    voice.acknowledge(retry).await.unwrap();
    drop(voice);
    let voice = VoiceTranscript::open(checkpoints, "parent", sink)
        .await
        .unwrap();
    assert!(voice.handoff_context().await.unwrap().text.is_empty());
    assert!(
        voice
            .task_context()
            .await
            .unwrap()
            .contains("preserve the toolbar")
    );
}

#[tokio::test]
async fn handoff_cursor_sends_only_draft_continuations_and_preserves_final_corrections() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3")).unwrap());
    let mut parent = Checkpoint::empty("parent");
    parent.session_context.owner_id = "bot".into();
    checkpoints.save(&parent, &[], None).await.unwrap();
    let mut voice = VoiceTranscript::open(checkpoints, "parent", Arc::new(|_| Ok(())))
        .await
        .unwrap();
    voice
        .record("user", ConversationRole::User, "Use blue 🗣", false)
        .await
        .unwrap();
    voice
        .record(
            "assistant",
            ConversationRole::Assistant,
            "Keep the toolbar",
            false,
        )
        .await
        .unwrap();
    let first = voice.handoff_context().await.unwrap();
    voice.acknowledge(first).await.unwrap();
    voice
        .record("user", ConversationRole::User, " and white", false)
        .await
        .unwrap();
    let second = voice.handoff_context().await.unwrap();
    assert_eq!(second.text, "User (continued):  and white");
    // Final snapshots replace journal deltas after the frozen handoff was prepared.
    voice
        .record("user", ConversationRole::User, "Use blue 🗣 and white", true)
        .await
        .unwrap();
    voice
        .record(
            "assistant",
            ConversationRole::Assistant,
            "Keep the toolbar actions",
            true,
        )
        .await
        .unwrap();
    voice.acknowledge(second).await.unwrap();
    let third = voice.handoff_context().await.unwrap();
    assert_eq!(third.text, "You (voice) (continued):  actions");
    voice.acknowledge(third).await.unwrap();
    assert!(voice.handoff_context().await.unwrap().text.is_empty());
    voice
        .record("correction", ConversationRole::User, "Use red", false)
        .await
        .unwrap();
    let draft = voice.handoff_context().await.unwrap();
    voice.acknowledge(draft).await.unwrap();
    voice
        .record("correction", ConversationRole::User, "Use green", true)
        .await
        .unwrap();
    assert_eq!(
        voice.handoff_context().await.unwrap().text,
        "Correction to earlier voice speech:\nUser: Use green"
    );
    let corrected = voice.handoff_context().await.unwrap();
    voice.acknowledge(corrected).await.unwrap();
    for (id, role) in [
        ("noise", ConversationRole::User),
        ("echo", ConversationRole::Assistant),
    ] {
        voice
            .record(id, role, "discard this draft", false)
            .await
            .unwrap();
        let draft = voice.handoff_context().await.unwrap();
        voice.acknowledge(draft).await.unwrap();
        voice.record(id, role, "", true).await.unwrap();
        let cleared = voice.handoff_context().await.unwrap();
        assert!(cleared.text.contains("[speech discarded]"));
        assert!(!cleared.text.contains("discard this draft"));
        voice.acknowledge(cleared).await.unwrap();
    }
    assert!(voice.cursor.drafts.is_empty());
}

#[tokio::test]
async fn live_previews_coalesce_and_flush_on_deadline_final_and_close() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3")).unwrap());
    let mut parent = Checkpoint::empty("parent");
    parent.session_context.owner_id = "bot".into();
    checkpoints.save(&parent, &[], None).await.unwrap();
    let updates = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&updates);
    let sink: FrontendEventSink = Arc::new(move |event| {
        if matches!(event, FrontendEvent::Preview { .. }) {
            received.lock().unwrap().push(event);
        }
        Ok(())
    });
    let mut voice = VoiceTranscript::open(checkpoints, "parent", sink)
        .await
        .unwrap();
    voice
        .record("user", ConversationRole::User, "one", false)
        .await
        .unwrap();
    let deadline = voice.preview_deadline;
    for _ in 0..20 {
        voice
            .record("user", ConversationRole::User, " word", false)
            .await
            .unwrap();
    }
    assert_eq!(voice.preview_deadline, deadline);
    assert!(updates.lock().unwrap().is_empty());
    // The trailing update must arrive even when speech stops without a final event.
    voice.wait_for_preview().await;
    voice.flush_preview().await.unwrap();
    assert_eq!(updates.lock().unwrap().len(), 1);
    assert!(voice.preview_deadline.is_none());
    voice.flush_preview().await.unwrap();
    assert_eq!(updates.lock().unwrap().len(), 1);
    voice
        .record(
            "user",
            ConversationRole::User,
            "Final corrected words",
            true,
        )
        .await
        .unwrap();
    assert_eq!(updates.lock().unwrap().len(), 2);
    voice
        .record(
            "assistant",
            ConversationRole::Assistant,
            "Unfinished reply",
            false,
        )
        .await
        .unwrap();
    assert_eq!(updates.lock().unwrap().len(), 2);
    voice.finish().await.unwrap();
    let updates = updates.lock().unwrap();
    assert_eq!(updates.len(), 3);
    let FrontendEvent::Preview { events, .. } = updates.last().unwrap() else {
        panic!("preview")
    };
    assert_eq!(events.len(), 2);
    assert!(
        matches!(&events[0].event, EventMsg::Message(message) if message.text == "Final corrected words")
    );
    assert!(
        matches!(&events[1].event, EventMsg::AssistantMessage(message) if message.content[0].text == "Unfinished reply")
    );
}

#[tokio::test]
async fn discarded_speech_stays_cleared_in_history_and_resumed_task_context() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3")).unwrap());
    let mut parent = Checkpoint::empty("parent");
    parent.session_context.owner_id = "bot".into();
    checkpoints.save(&parent, &[], None).await.unwrap();
    let sink: FrontendEventSink = Arc::new(|_| Ok(()));
    let mut voice = VoiceTranscript::open(Arc::clone(&checkpoints), "parent", Arc::clone(&sink))
        .await
        .unwrap();
    for (input_id, role) in [
        ("noise", ConversationRole::User),
        ("reply", ConversationRole::Assistant),
    ] {
        voice
            .record(input_id, role, "safari's", false)
            .await
            .unwrap();
        voice.record(input_id, role, "", true).await.unwrap();
    }
    voice.finish().await.unwrap();
    drop(voice);
    let voice = VoiceTranscript::open(Arc::clone(&checkpoints), "parent", sink)
        .await
        .unwrap();
    let journal = checkpoints
        .event_page(
            voice.session_id(),
            EventPageRequest {
                before_sequence: None,
                limit: PAGE_SIZE,
            },
        )
        .await
        .unwrap();
    assert!(!serde_json::to_string(&journal).unwrap().contains("safari"));
    assert!(!voice.task_context().await.unwrap().contains("safari"));
    let preview = read_preview(checkpoints.as_ref(), "parent", "")
        .await
        .unwrap();
    assert!(!serde_json::to_string(&preview).unwrap().contains("safari"));
}

#[tokio::test]
async fn voice_transcript_is_linked_read_only_and_resumes_without_reusing_message_ids() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("checkpoints.sqlite3");
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(&path).expect("store"));
    let mut parent = Checkpoint::empty("parent");
    parent.session_context.owner_id = "bot".into();
    parent.context = vec![serde_json::json!({"role":"user","content":"private parent task"})];
    checkpoints
        .save(&parent, &parent.context, None)
        .await
        .expect("parent");
    let before = serde_json::to_value(&parent).expect("parent snapshot");
    let updates = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&updates);
    let sink: FrontendEventSink = Arc::new(move |event| {
        received.lock().expect("events").push(event);
        Ok(())
    });
    let mut voice = VoiceTranscript::open(Arc::clone(&checkpoints), "parent", Arc::clone(&sink))
        .await
        .expect("voice");
    let child_id = voice.session_id().to_owned();
    assert!(updates.lock().expect("events").is_empty());
    assert!(
        restore_widget(checkpoints.as_ref(), "parent")
            .await
            .expect("widget")
            .is_none()
    );

    voice
        .record("input-1", ConversationRole::User, "Hel", false)
        .await
        .expect("partial");
    voice
        .record("input-1", ConversationRole::User, "lo", false)
        .await
        .expect("partial");
    let draft = read_preview(checkpoints.as_ref(), "parent", "")
        .await
        .expect("preview");
    let FrontendEvent::Preview { events, .. } = draft else {
        panic!("preview")
    };
    assert_eq!(events.len(), 2);
    let canonical_id = events[0].submission_id.clone();
    assert!(canonical_id.is_some());
    assert_eq!(events[1].submission_id, canonical_id);
    assert!(
        updates
            .lock()
            .expect("events")
            .iter()
            .any(|event| matches!(event,
                FrontendEvent::Widget { item, .. } if item.icon_only
                    && item.symbol.as_ref().is_some_and(|symbol| symbol.as_str() == "voice")
                    && item.action.is_some()
            ))
    );
    voice
        .record("input-1", ConversationRole::User, "Hello", true)
        .await
        .expect("final");
    voice
        .record("output-1", ConversationRole::Assistant, "Hi there", false)
        .await
        .expect("response");
    voice.finish().await.expect("finish partial response");
    drop(voice);
    drop(checkpoints);

    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(&path).expect("reopen store"));
    let mut voice = VoiceTranscript::open(Arc::clone(&checkpoints), "parent", sink)
        .await
        .expect("reopen call");
    assert_eq!(voice.session_id(), child_id);
    assert!(
        restore_widget(checkpoints.as_ref(), "parent")
            .await
            .expect("restored widget")
            .is_some()
    );
    voice
        .record("input-1", ConversationRole::User, "Another call", true)
        .await
        .expect("fresh provider identity");
    let mut events = Vec::new();
    let mut queued = Vec::new();
    let metadata = BTreeMap::new();
    let result = Messages::default()
        .active_command(&mut ActiveCommandContext {
            checkpoints: checkpoints.as_ref(),
            submission_id: "preview-request",
            session_id: "parent",
            metadata: &metadata,
            active_turn_id: "ongoing-parent-work",
            command: COMMAND,
            arguments: "",
            input: None,
            target: None,
            queued_messages: MessageQueue::new(&mut queued),
            events: &mut events,
        })
        .await
        .expect("active preview");
    assert_eq!(result, Some(SubmissionResult::Handled));
    let EventMsg::Frontend(FrontendEvent::Preview { events, .. }) = &events[0] else {
        panic!("preview")
    };
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].submission_id, canonical_id);
    assert_ne!(events[2].submission_id, canonical_id);
    assert!(matches!(&events[0].event, EventMsg::Message(message) if message.text == "Hello"));
    assert!(
        matches!(&events[1].event, EventMsg::AssistantMessage(message) if message.content[0].text == "Hi there")
    );
    assert!(
        read_preview(checkpoints.as_ref(), "other-parent", "")
            .await
            .is_err()
    );
    assert!(
        read_preview(checkpoints.as_ref(), "parent", "0")
            .await
            .is_err()
    );
    let parent_after = checkpoints
        .load("parent")
        .await
        .expect("parent")
        .expect("exists");
    assert_eq!(
        serde_json::to_value(parent_after).expect("parent snapshot"),
        before
    );
    let child = checkpoints
        .load(&child_id)
        .await
        .expect("child")
        .expect("exists");
    assert!(child.context.is_empty());
    assert!(!child.catalog_visible);
    let catalog = checkpoints
        .list_sessions_page(SessionPageRequest {
            owner_id: None,
            cursor: None,
            limit: 10,
        })
        .await
        .expect("catalog");
    assert_eq!(
        catalog
            .sessions
            .iter()
            .find(|session| session.session_id == child_id)
            .expect("child")
            .parent_session_id
            .as_deref(),
        Some("parent")
    );
    checkpoints
        .delete_sessions(&["parent".into()])
        .await
        .expect("delete parent");
    assert!(
        checkpoints
            .load(&child_id)
            .await
            .expect("child deleted")
            .is_none()
    );
}

#[tokio::test]
async fn preview_page_keeps_an_unfinished_message_whole() {
    let directory = tempfile::tempdir().expect("directory");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3")).expect("store"),
    );
    let mut parent = Checkpoint::empty("parent");
    parent.session_context.owner_id = "bot".into();
    checkpoints.save(&parent, &[], None).await.expect("parent");
    let voice = VoiceTranscript::open(Arc::clone(&checkpoints), "parent", Arc::new(|_| Ok(())))
        .await
        .expect("voice");
    for index in 0..PAGE_SIZE + 3 {
        let event = speech_event(
            voice.session_id(),
            "message",
            ConversationRole::User,
            "word ",
            false,
        );
        checkpoints
            .append_event(voice.session_id(), index as i64, &event)
            .await
            .expect("delta");
    }
    let FrontendEvent::Preview { events, next, .. } =
        read_preview(checkpoints.as_ref(), "parent", "")
            .await
            .expect("preview")
    else {
        panic!("preview")
    };
    assert_eq!(events.len(), PAGE_SIZE + 3);
    assert!(next.is_none());
    let snapshot = voice.task_context().await.expect("task snapshot");
    assert_eq!(snapshot, format!("User: {}", "word ".repeat(PAGE_SIZE + 3)));
    checkpoints
        .append_event(
            voice.session_id(),
            200,
            &speech_event(
                voice.session_id(),
                "message",
                ConversationRole::User,
                "Final corrected speech",
                true,
            ),
        )
        .await
        .expect("final prunes deltas");
    assert_eq!(
        voice.task_context().await.expect("updated snapshot"),
        "User: Final corrected speech"
    );
    assert_eq!(snapshot, format!("User: {}", "word ".repeat(PAGE_SIZE + 3)));
}

#[tokio::test]
async fn voice_history_stops_before_expanding_an_oversized_unfinished_prefix() {
    let directory = tempfile::tempdir().expect("directory");
    let checkpoints =
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3")).expect("store");
    let mut child = Checkpoint::empty("voice");
    child.session_context.owner_id = "bot".into();
    checkpoints.save(&child, &[], None).await.expect("child");
    let large = "x".repeat(MAX_MESSAGE_BYTES);
    for index in 0..PAGE_SIZE + 9 {
        let event = speech_event(
            "voice",
            &index.to_string(),
            ConversationRole::User,
            if index < 9 { &large } else { "word " },
            false,
        );
        checkpoints
            .append_event("voice", index as i64, &event)
            .await
            .expect("delta");
    }
    let error = history_page(&checkpoints, "voice", None)
        .await
        .expect_err("bounded history");
    assert!(error.to_string().contains("exceeds its size limit"));
}

#[tokio::test(start_paused = true)]
async fn total_call_duration_and_latest_voice_survive_reopen_and_pagination() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(directory.path().join("calls.sqlite3")).unwrap());
    let mut parent = Checkpoint::empty("parent");
    parent.session_context.owner_id = "bot".into();
    checkpoints.save(&parent, &[], None).await.unwrap();
    let mut total = 0;
    for (voice, seconds) in [("sol", 60), ("cove", 90)] {
        let mut transcript =
            VoiceTranscript::open(Arc::clone(&checkpoints), "parent", Arc::new(|_| Ok(())))
                .await
                .unwrap();
        transcript.start_call(voice).await.unwrap();
        let summary = call_summary(checkpoints.as_ref(), transcript.session_id())
            .await
            .unwrap();
        assert_eq!(summary.duration_ms, total);
        assert!(summary.started_at_ms.is_some());
        tokio::time::advance(Duration::from_secs(seconds)).await;
        transcript.finish().await.unwrap();
        // Finishing twice must not count a call twice.
        transcript.finish().await.unwrap();
        total += seconds * 1_000;
        for before in [None, Some(1)] {
            let FrontendEvent::Preview {
                symbol,
                subtitle,
                duration_ms,
                started_at_ms,
                ..
            } = preview(checkpoints.as_ref(), transcript.session_id(), before)
                .await
                .unwrap()
            else {
                panic!("expected transcript preview")
            };
            assert_eq!(symbol, Some(FrontendSymbol::Custom("voice".into())));
            assert_eq!(subtitle, voice);
            assert_eq!(duration_ms, Some(total));
            assert_eq!(started_at_ms, None);
        }
    }
}
