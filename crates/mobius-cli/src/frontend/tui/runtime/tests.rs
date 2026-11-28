use super::*;
use mobius::protocol::{
    ActiveMessageDelivery, Event, EventMsg, ModelChangedEvent, SessionConfiguredEvent,
    SessionContext,
};
use mobius_gateway::wire::{SessionReadyPayload, WorkspaceInfo};

#[test]
fn rejected_message_keeps_newer_input_and_recovers_the_failed_draft() {
    use mobius::protocol::SubmissionRejectedEvent;

    let mut state = TuiState::default();
    state.pending_message_drafts.insert(
        "message-1".into(),
        InputDraft {
            text: "original request".into(),
            cursor: "original request".len(),
            pastes: Default::default(),
            attachments: vec![SessionFileReference {
                id: "file-1".into(),
                name: "notes.txt".into(),
                size: 12,
                media_type: "text/plain".into(),
            }],
        },
    );
    state.composer.text = "new draft".into();
    state.composer.cursor = state.composer.text.len();
    state.composer_history.push_back("older request".into());

    settle_message_draft(
        &mut state,
        &Event {
            submission_id: Some("message-1".into()),
            msg: EventMsg::SubmissionRejected(SubmissionRejectedEvent {
                message: "try again".into(),
            }),
        },
    );

    assert_eq!(state.composer.text, "new draft");
    assert!(state.pending_message_drafts.is_empty());
    state.composer_history_up();
    assert_eq!(state.composer.text, "older request");
    state.composer_history_down();
    assert_eq!(state.composer.text, "new draft");
    state.recover_saved_message();
    assert_eq!(state.composer.text, "original request");
    assert_eq!(state.composer.attachments[0].name, "notes.txt");
    state.recover_saved_message();
    assert_eq!(state.composer.text, "new draft");
    assert!(state.composer.attachments.is_empty());
}

#[test]
fn disconnect_keeps_unconfirmed_message_available_without_claiming_it_failed() {
    let mut state = TuiState::default();
    state.pending_message_drafts.insert(
        "message-1".into(),
        InputDraft {
            text: "possibly delivered".into(),
            cursor: 0,
            pastes: Default::default(),
            attachments: Vec::new(),
        },
    );

    state.recover_unconfirmed_messages();

    assert!(state.pending_message_drafts.is_empty());
    assert_eq!(state.composer.text, "possibly delivered");
    assert!(
        state
            .transcript
            .iter()
            .any(|entry| entry.text.contains("delivery unconfirmed"))
    );
}

#[tokio::test]
async fn downloads_publish_only_when_complete_without_overwriting() {
    let directory = tempfile::tempdir().expect("download directory");
    let existing = directory.path().join("report.txt");
    std::fs::write(&existing, b"keep").expect("existing file");
    let pending = |state: &mut TuiState| {
        let (output, staged) = NamedTempFile::new_in(directory.path())
            .expect("staged download")
            .into_parts();
        state.file_download = Some(super::super::PendingFileDownload {
            request_id: "request".into(),
            session_id: mobius::SessionId::new("session").expect("session"),
            file: SessionFileReference {
                id: "file".into(),
                name: "report.txt".into(),
                size: 2,
                media_type: "text/plain".into(),
            },
            directory: directory.path().to_path_buf(),
            output: tokio::fs::File::from_std(output),
            staged,
            preview: Vec::new(),
            preview_truncated: false,
        });
    };
    let entries = || {
        std::fs::read_dir(directory.path())
            .expect("entries")
            .count()
    };

    let mut abandoned = TuiState::default();
    pending(&mut abandoned);
    write_file_chunk(&mut abandoned, b"partial")
        .await
        .expect("valid chunk");
    drop(abandoned);
    assert_eq!(entries(), 1);

    let mut state = TuiState::default();
    pending(&mut state);
    write_file_chunk(&mut state, b"ok")
        .await
        .expect("valid chunk");
    finish_file_download(&mut state).await;
    assert!(state.file_download.is_none());
    assert_eq!(std::fs::read(&existing).expect("existing file"), b"keep");
    assert_eq!(
        std::fs::read(directory.path().join("report-1.txt")).expect("saved file"),
        b"ok"
    );
    assert_eq!(entries(), 2);
}

#[test]
fn stale_session_opened_does_not_reload_or_settle_new_creation() {
    let mut pending = Some(PendingSessionCreation {
        request_id: "create-new".into(),
        clear: true,
    });
    let stale = ServerMessage::SessionOpened {
        request_id: "create-old".into(),
        payload: session_payload("session-old-response"),
    };

    let session_opened = match &stale {
        ServerMessage::SessionOpened { request_id, .. } => {
            matches_pending_session_creation(request_id, pending.as_ref())
        }
        _ => false,
    };
    let mut session = session_payload("session-current");
    let mut gateway =
        crate::frontend::fixtures::ready(vec![crate::frontend::fixtures::bot("bot-a")]);
    let catalog = UiCatalog::build(&[], Some(std::path::Path::new("/tmp"))).expect("catalog");
    let mut state = TuiState::new(
        &catalog,
        Some("/tmp".into()),
        ModelInfo::default(),
        Arc::default(),
        String::new(),
    );

    assert!(!session_opened);
    assert!(!settle_session_creation(&stale, &mut pending));
    assert!(
        handle_server_message(
            stale,
            &mut gateway,
            &mut session,
            &mut state,
            session_opened,
        )
        .is_none()
    );
    assert_eq!(&*session.session.session_id, "session-current");
    assert_eq!(
        pending.as_ref().map(|request| &*request.request_id),
        Some("create-new")
    );
}

fn session_payload(session_id: &str) -> SessionReadyPayload {
    SessionReadyPayload {
        active_turn_ids: Vec::new(),
        pending_approvals: Vec::new(),
        latest_sequence: 0,
        attached_folders: Vec::new(),
        workspace: Some(WorkspaceInfo {
            id: "workspace".into(),
            path: "/tmp".into(),
        }),
        git: None,
        session: Arc::new(SessionConfiguredEvent {
            session_id: mobius::SessionId::new(session_id).expect("session"),
            context: Arc::new(SessionContext {
                owner_id: "bot-a".into(),
                ..SessionContext::default()
            }),
            model: ModelChangedEvent {
                route: Arc::default(),
                model: Arc::default(),
                reasoning_effort: None,
                model_context_window: None,
            },
        }),
        contributions: Vec::new(),
        widgets: Vec::new(),
        context_limit_tokens: None,
        token_usage: None,
        active_message_delivery: ActiveMessageDelivery::Steer,
        run_stats: Arc::default(),
        pinned_messages: Default::default(),
        memory: None,
    }
}
