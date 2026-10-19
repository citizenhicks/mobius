use std::sync::Mutex;

use super::*;
use crate::backend::checkpoint::SessionPageRequest;
use crate::backend::checkpoint::sqlite::SqliteCheckpoint;
use crate::middleware::voice::Voice;
use crate::middleware::{ActiveCommandContext, MessageQueue, Middleware, SubmissionResult};

#[tokio::test]
async fn handoff_cursor_advances_only_on_acknowledgement_and_survives_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3")).unwrap());
    let mut parent = Checkpoint::empty("parent");
    std::sync::Arc::make_mut(&mut parent.session_context).owner_id = "bot".into();
    checkpoints
        .save(parent.clone(), Vec::new(), None)
        .await
        .unwrap();
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
async fn finish_retries_failed_persistence_without_losing_or_duplicating_speech() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("checkpoints.sqlite3");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(SqliteCheckpoint::new(&path).unwrap());
    let mut parent = Checkpoint::empty("parent");
    std::sync::Arc::make_mut(&mut parent.session_context).owner_id = "bot".into();
    checkpoints
        .save(parent.clone(), Vec::new(), None)
        .await
        .unwrap();
    let mut voice = VoiceTranscript::open(checkpoints, "parent", Arc::new(|_| Ok(())))
        .await
        .unwrap();
    voice
        .record(
            "speech",
            ConversationRole::Assistant,
            "Keep this draft",
            false,
        )
        .await
        .unwrap();
    let database = rusqlite::Connection::open(&path).unwrap();
    database
        .execute_batch(
            "CREATE TRIGGER fail_voice_finish BEFORE INSERT ON event_journal
             BEGIN SELECT RAISE(ABORT, 'injected voice persistence failure'); END;",
        )
        .unwrap();
    assert!(voice.finish().await.is_err());
    let recording = voice.recordings.get("speech").unwrap();
    assert_eq!(recording.text, "Keep this draft");
    assert!(!recording.complete);
    database
        .execute_batch("DROP TRIGGER fail_voice_finish")
        .unwrap();
    voice.finish().await.unwrap();
    voice.finish().await.unwrap();
    let history = voice
        .checkpoints
        .event_page(
            voice.session_id(),
            EventPageRequest {
                before_sequence: None,
                limit: PAGE_SIZE,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        history
            .events
            .iter()
            .filter(|event| matches!(
                &event.event.msg,
                EventMsg::AssistantMessage(message) if message.content[0].text == "Keep this draft"
            ))
            .count(),
        1
    );
    assert_eq!(
        voice.task_context().await.unwrap(),
        "You (voice): Keep this draft"
    );
}

#[tokio::test]
async fn voice_transcript_is_linked_read_only_and_resumes_without_reusing_message_ids() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("checkpoints.sqlite3");
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(SqliteCheckpoint::new(&path).expect("store"));
    let mut parent = Checkpoint::empty("parent");
    std::sync::Arc::make_mut(&mut parent.session_context).owner_id = "bot".into();
    parent.context = std::sync::Arc::new(
        vec![serde_json::json!({"role":"user","content":"private parent task"})]
            .into_iter()
            .map(std::sync::Arc::new)
            .collect(),
    );
    checkpoints
        .save(parent.clone(), parent.context.to_vec(), None)
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
    assert_eq!(**voice.session_id(), *child_id);
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
    let result = Voice
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
            .find(|session| *session.session_id == *child_id)
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
