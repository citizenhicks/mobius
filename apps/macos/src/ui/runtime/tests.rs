use super::{DraftAttachment, PendingMessage, RuntimeState};
use mobius::SessionId;

#[test]
fn unsent_text_recovers_without_replacing_newer_typing_or_duplicating_it() {
    use super::session::recovered_draft;
    for (current, unsent, expected) in [
        ("", "unsent", "unsent"),
        ("new typing", "unsent", "new typing\n\nunsent"),
        ("same text", "same text", "same text"),
        ("new typing", "", "new typing"),
    ] {
        assert_eq!(recovered_draft(current, unsent), expected);
    }
}

#[test]
fn a_confirmed_message_releases_only_the_attachments_it_carried() {
    let key = "draft".to_owned();
    let mut runtime = RuntimeState::default();
    runtime.attachments.insert(
        key.clone(),
        ["attachment", "newer-attachment"]
            .map(|id| DraftAttachment {
                id: id.into(),
                name: id.into(),
                file: None,
                linked: false,
                error: None,
                cancellation: Default::default(),
                cleanup_file_id: None,
            })
            .into(),
    );
    runtime.forget_sent(&PendingMessage {
        draft_key: key.clone(),
        text: "sent".into(),
        attachments: vec!["attachment".into()],
        files: Vec::new(),
        reply: None,
    });
    assert_eq!(runtime.attachments[&key].len(), 1);
    assert_eq!(&*runtime.attachments[&key][0].id, "newer-attachment");
}

#[test]
fn initial_draft_binding_preserves_preconnect_text_and_stays_in_its_session() {
    let mut runtime = RuntimeState::default();
    let mut client = crate::gateway::Client::default();
    let mut drafts = Default::default();
    assert_eq!(
        runtime
            .bind_draft(client.draft_key(""), &mut drafts, "typed offline")
            .as_deref(),
        Some("typed offline")
    );
    client.endpoint = "tcp://127.0.0.1:42000".into();
    runtime.draft_owner = None;
    assert_eq!(
        runtime
            .bind_draft(client.draft_key(""), &mut drafts, "typed offline")
            .as_deref(),
        Some("typed offline")
    );
    assert_eq!(drafts[&client.draft_key("")], "typed offline");
    // Returning from a document leaves the newer composer contents untouched.
    assert!(
        runtime
            .bind_draft(client.draft_key(""), &mut drafts, "typed more")
            .is_none()
    );
    drafts.insert(client.draft_key("other-chat"), "other draft".into());
    assert_eq!(
        runtime
            .bind_draft(client.draft_key("other-chat"), &mut drafts, "typed more")
            .as_deref(),
        Some("other draft")
    );
}

fn ready_session(id: &SessionId) -> mobius_gateway::wire::SessionReadyPayload {
    mobius_gateway::wire::SessionReadyPayload {
        memory: None,
        active_turn_ids: Vec::new(),
        pending_approvals: Vec::new(),
        latest_sequence: 0,
        workspace: Some(mobius_gateway::wire::WorkspaceInfo {
            id: "workspace".into(),
            path: ".".into(),
        }),
        attached_folders: Vec::new(),
        git: None,
        session: std::sync::Arc::new(mobius::protocol::SessionConfiguredEvent {
            session_id: id.share(),
            context: Default::default(),
            model: mobius::protocol::ModelChangedEvent {
                route: "route".into(),
                model: "model".into(),
                reasoning_effort: None,
                model_context_window: None,
            },
        }),
        contributions: Vec::new(),
        widgets: Vec::new(),
        context_limit_tokens: None,
        token_usage: None,
        active_message_delivery: mobius::protocol::ActiveMessageDelivery::Steer,
        run_stats: Default::default(),
        pinned_messages: Default::default(),
    }
}

#[test]
fn session_metadata_and_pin_updates_preserve_live_state() {
    use mobius_gateway::wire::ServerMessage;
    let id = SessionId::new("chat").unwrap();
    let other = SessionId::new("other").unwrap();
    for cursor in [Some(8), None] {
        let history = cursor.map(|before_sequence| mobius_gateway::wire::HistoryCursor {
            session_id: id.share(),
            before_sequence,
        });
        let mut state = crate::gateway::GatewayState {
            selected_session_id: Some(id.share()),
            session: Some(ready_session(&id)),
            history: history.clone(),
            ..Default::default()
        };
        let mut changed = ready_session(&id);
        changed.latest_sequence = 50;
        changed.active_turn_ids.push("running".into());
        state.apply(&mut ServerMessage::SessionChanged { payload: changed });
        state.apply(&mut ServerMessage::SessionMetaChanged {
            session_id: id.share(),
            contributions: Vec::new(),
            context_limit_tokens: Some(1_000),
        });
        let target = mobius::protocol::MessageTarget {
            checkpoint_sequence: 3,
            batch_item_count: std::num::NonZeroUsize::MIN,
        };
        for (id, expected) in [(&other, false), (&id, true)] {
            state.apply(&mut ServerMessage::MessagePinsChanged {
                session_id: id.share(),
                pinned_messages: [target].into(),
            });
            let session = state.session.as_ref().unwrap();
            assert_eq!(session.pinned_messages.contains(&target), expected);
            assert_eq!(state.history, history);
            assert_eq!(session.context_limit_tokens, Some(1_000));
            assert_eq!(session.latest_sequence, 50);
            assert_eq!(session.active_turn_ids, ["running".into()]);
        }
    }
}
