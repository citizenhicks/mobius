use super::support::*;
use super::*;

#[test]
fn composer_dispatches_commands_and_active_turn_steering() {
    let catalog = default_catalog();
    let mut idle = state();
    idle.input = "/exit".into();
    idle.cursor = idle.input.len();

    assert_eq!(idle.submit_input(&catalog), UiAction::Exit);

    let mut working = state();
    working.start_turn("turn".into());
    working.input = "change direction".into();
    working.cursor = working.input.len();

    assert_eq!(
        working.submit_input(&catalog),
        UiAction::submit(Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::User,
                text: "change direction".into(),
                attachments: Vec::new(),
                reply: None,
                requested_delivery: None,
                target_turn_id: Some("turn".into()),
            },
        })
    );
}

#[test]
fn file_picker_requires_confirmation_before_delete() {
    let file = mobius::protocol::SessionFileReference {
        id: "file-1".into(),
        name: "report.txt".into(),
        size: 5,
        media_type: "text/plain".into(),
    };
    let mut state = state();
    state.open_session_files(vec![mobius::protocol::SessionFileRecord {
        origin: mobius::protocol::SessionFileOrigin::Agent,
        file: file.clone(),
    }]);

    assert_eq!(
        state.handle_key(
            KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
            &default_catalog(),
        ),
        UiAction::ConfirmDeleteFile(file.clone())
    );
    state.confirm_delete_file(file);
    assert_eq!(
        state.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &default_catalog(),
        ),
        UiAction::Gateway(crate::frontend::catalog::GatewayAction::DeleteStoredFile(
            "file-1".into()
        ))
    );
}

#[test]
fn remote_workspace_inventory_does_not_read_the_client_filesystem() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("client-only.rs"), "").expect("client file");
    let mut catalog = catalog(workspace.path());
    catalog.set_workspace_paths(["server-only.rs".into()]);

    assert!(catalog.reference_suggestions('@', "client").is_empty());
    assert_eq!(
        catalog.reference_suggestions('@', "server")[0].value,
        "server-only.rs"
    );
}

#[test]
fn composer_rejects_input_over_the_protocol_limit() {
    let mut state = state();
    state.insert_paste(&"x".repeat(mobius::protocol::MAX_MESSAGE_BYTES));
    state.insert_text("x");

    assert_eq!(
        (
            state.pastes.values().map(String::len).sum::<usize>(),
            state.input_limit_reached,
        ),
        (mobius::protocol::MAX_MESSAGE_BYTES, true)
    );
}

#[test]
fn ctrl_c_aborts_approval_and_restores_draft() {
    let catalog = default_catalog();
    let mut state = state();
    state.input = "draft".into();
    state.cursor = state.input.len();
    state.begin_approval(mobius::protocol::ExecApprovalRequestEvent {
        id: "approval".into(),
        turn_id: "turn".into(),
        calls: Vec::new(),
        reason: "test".into(),
    });
    state.handle_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        &catalog,
    );

    assert_eq!(
        state.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &catalog
        ),
        UiAction::submit(Op::ExecApproval {
            id: "approval".into(),
            decision: ReviewDecision::Abort,
        })
    );
    assert_eq!(state.input, "draft");
}

#[test]
fn active_turn_preserves_attachment_draft_instead_of_steering() {
    let mut state = state();
    state.start_turn("turn".into());
    state.input = "next request".into();
    state.cursor = state.input.len();
    state.attachments.push(attachment("report.pdf"));

    assert_eq!(state.submit_input(&default_catalog()), UiAction::None);
    assert_eq!(state.input, "next request");
    assert_eq!(state.attachments, vec![attachment("report.pdf")]);
}

fn attachment(name: &str) -> mobius::protocol::SessionFileReference {
    mobius::protocol::SessionFileReference {
        id: "3d46beff-7e84-46ea-859a-e66b4614a79b".into(),
        name: name.into(),
        size: 4,
        media_type: if std::path::Path::new(name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        {
            "application/pdf"
        } else {
            "image/png"
        }
        .into(),
    }
}

#[test]
fn queued_approvals_keep_the_draft_until_the_last_decision() {
    let mut state = state();
    state.input = "continue after both approvals".into();
    state.cursor = state.input.len();
    for suffix in ["a", "b"] {
        state.handle_agent_event(
            EventMsg::ExecApprovalRequest(mobius::protocol::ExecApprovalRequestEvent {
                id: format!("approval-{suffix}").into(),
                turn_id: format!("turn-{suffix}").into(),
                calls: Vec::new(),
                reason: format!("@bot-{suffix}: approve work"),
            }),
            Vec::new(),
        );
    }
    for suffix in ["a", "b"] {
        assert_eq!(*state.approval().unwrap().id, format!("approval-{suffix}"));
        let detail = view::live_transcript_lines(&mut state, 0, 100)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(detail.contains(&format!("@bot-{suffix}: approve work")));
        assert!(state.input.is_empty());
        assert_eq!(
            state.handle_key(
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
                &default_catalog()
            ),
            UiAction::None,
        );
        assert_eq!(state.input, "y");
        assert_eq!(
            state.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &default_catalog()
            ),
            UiAction::submit(Op::ExecApproval {
                id: format!("approval-{suffix}").into(),
                decision: ReviewDecision::Approved,
            }),
        );
    }
    assert_eq!(state.input, "continue after both approvals");
    assert!(state.approval().is_none());
    assert!(state.is_working());
}
