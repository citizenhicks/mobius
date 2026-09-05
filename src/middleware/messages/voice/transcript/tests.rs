use std::sync::Mutex;

use super::*;
use crate::backend::checkpoint::SessionPageRequest;
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::middleware::messages::Messages;
use crate::middleware::{ActiveCommandContext, MessageQueue, Middleware, SubmissionResult};

#[tokio::test]
async fn voice_transcript_is_linked_read_only_and_resumes_without_reusing_message_ids() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("checkpoints.sqlite3");
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(&path).expect("store"));
    let mut parent = Checkpoint::empty("parent");
    parent.session_context.bot_id = "bot".into();
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
    parent.session_context.bot_id = "bot".into();
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
    child.session_context.bot_id = "bot".into();
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
