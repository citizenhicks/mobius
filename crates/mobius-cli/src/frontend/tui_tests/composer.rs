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
    working.active_turn = Some("turn".into());
    working.input = "change direction".into();
    working.cursor = working.input.len();

    assert_eq!(
        working.submit_input(&catalog),
        UiAction::Submit(Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::User,
                text: "change direction".into(),
                attachments: Vec::new(),
                requested_delivery: None,
                target_turn_id: Some("turn".into()),
            },
        })
    );
}

#[test]
fn alt_enter_uses_the_opposite_active_delivery() {
    let catalog = default_catalog();
    for (configured, expected) in [
        (
            mobius::protocol::ActiveMessageDelivery::Steer,
            mobius::protocol::ActiveMessageDelivery::Queue,
        ),
        (
            mobius::protocol::ActiveMessageDelivery::Queue,
            mobius::protocol::ActiveMessageDelivery::Steer,
        ),
    ] {
        let mut state = state();
        state.active_turn = Some("turn".into());
        state.active_message_delivery = Some(configured);
        state.input = "follow up".into();
        state.cursor = state.input.len();

        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), &catalog,),
            UiAction::Submit(Op::Message {
                message: MessageSubmission {
                    author: MessageAuthor::User,
                    text: "follow up".into(),
                    attachments: Vec::new(),
                    requested_delivery: Some(expected),
                    target_turn_id: Some("turn".into()),
                },
            })
        );
    }
}

#[test]
fn new_and_clear_keep_distinct_terminal_semantics() {
    let catalog = default_catalog();
    let mut new = state();
    new.input = "/new".into();
    new.cursor = new.input.len();
    let mut clear = state();
    clear.input = "/clear".into();
    clear.cursor = clear.input.len();

    assert_eq!(
        (new.submit_input(&catalog), clear.submit_input(&catalog)),
        (UiAction::New, UiAction::Clear)
    );
}

#[test]
fn composer_targets_interrupt_at_the_active_turn() {
    let catalog = default_catalog();
    let mut slash = state();
    slash.active_turn = Some("turn-1".into());
    slash.input = "/interrupt".into();
    slash.cursor = slash.input.len();
    let slash_action = slash.submit_input(&catalog);

    let mut escape = state();
    escape.active_turn = Some("turn-1".into());
    let escape_action =
        escape.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &catalog);

    assert_eq!(
        (slash_action, escape_action),
        (
            UiAction::Submit(Op::Interrupt {
                turn_id: "turn-1".into()
            }),
            UiAction::Submit(Op::Interrupt {
                turn_id: "turn-1".into()
            })
        )
    );
}

#[test]
fn generic_picker_submits_the_selected_operation() {
    let mut state = state();
    state.handle_agent_event(
        EventMsg::Frontend(FrontendEvent::Picker {
            title: "Resume chat".into(),
            options: vec![
                mobius::protocol::FrontendPickerOption {
                    label: "first".into(),
                    description: "older".into(),
                    detail: String::new(),
                    symbol: None,
                    shows_detail: false,
                    op: Op::ResumeSession {
                        session_id: "first".into(),
                    },
                },
                mobius::protocol::FrontendPickerOption {
                    label: "second".into(),
                    description: "newer".into(),
                    detail: String::new(),
                    symbol: None,
                    shows_detail: false,
                    op: Op::ResumeSession {
                        session_id: "second".into(),
                    },
                },
            ],
        }),
        Vec::new(),
    );

    let catalog = default_catalog();
    state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &catalog);

    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &catalog),
        UiAction::Submit(Op::ResumeSession {
            session_id: "second".into(),
        })
    );
}

#[test]
fn bare_capability_command_opens_all_of_its_popup_surfaces() {
    let refresh = Op::CapabilityCommand {
        capability: "scratchpad".into(),
        command: "scratchpad".into(),
        arguments: "refresh".into(),
        input: None,
        target: None,
    };
    let global_action = Op::CapabilityCommand {
        capability: "scratchpad".into(),
        command: "scratchpad".into(),
        arguments: "forget global global-1".into(),
        input: None,
        target: None,
    };
    let session_action = Op::CapabilityCommand {
        capability: "scratchpad".into(),
        command: "scratchpad".into(),
        arguments: "promote note-1".into(),
        input: None,
        target: None,
    };
    let surface =
        |id: &str, slot: FrontendSlot, title: &str, note: &str, action: Op| FrontendWidget {
            id: id.into(),
            slot,
            text: "Scratchpad".into(),
            tone: FrontendTone::Neutral,
            symbol: Some(FrontendSymbol::Brain),
            icon_only: false,
            progress: None,
            content: Some(FrontendWidgetContent::ActionList {
                title: title.into(),
                items: vec![FrontendActionListItem {
                    id: id.into(),
                    text: note.into(),
                    state: FrontendListItemState::Plain,
                    actions: vec![FrontendAction {
                        id: format!("action:{id}"),
                        label: "Run".into(),
                        symbol: FrontendSymbol::Edit,
                        tone: FrontendTone::Neutral,
                        op: action,
                    }],
                }],
            }),
            action: Some(refresh.clone()),
        };
    let global = surface(
        "navigation",
        FrontendSlot::Navigation,
        "Global scratchpad",
        "Global note",
        global_action.clone(),
    );
    let session = surface(
        "chat_menu",
        FrontendSlot::ChatMenu,
        "Chat scratchpad",
        "Session note",
        session_action.clone(),
    );
    let catalog = UiCatalog::build(
        &[FrontendContribution {
            capability: "scratchpad".into(),
            accepts_file_attachments: false,
            count: None,
            commands: vec![FrontendCommand {
                name: "scratchpad".into(),
                arguments: String::new(),
                description: "manage notes".into(),
                requires_idle: false,
            }],
            widgets: vec![global.clone(), session.clone()],
            references: Vec::new(),
        }],
        &[],
        std::path::Path::new("/missing-mobius-test-workspace"),
    )
    .expect("scratchpad catalog");
    let mut state = state();
    state.widgets.extend([
        (("scratchpad".into(), global.id.clone()), global),
        (("scratchpad".into(), session.id.clone()), session),
    ]);
    state.input = "/scratchpad".into();
    state.cursor = state.input.len();

    assert_eq!(
        state.submit_input(&catalog),
        UiAction::Submit(refresh.clone())
    );
    assert!(state.capability_overlay.is_some());

    let mut terminal = Terminal::new(TestBackend::new(72, 18)).expect("terminal");
    terminal
        .draw(|frame| {
            view::render(frame, &mut state, &catalog);
            crate::frontend::dashboard::render_capability_overlay(
                frame,
                state.capability_overlay.as_mut().expect("scratchpad popup"),
            );
        })
        .expect("scratchpad popup draw");
    let rendered = terminal.backend().to_string();
    assert!(rendered.contains("Global scratchpad"), "{rendered}");
    assert!(rendered.contains("Chat scratchpad"), "{rendered}");

    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &catalog),
        UiAction::Submit(refresh.clone())
    );
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &catalog),
        UiAction::Submit(global_action)
    );
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &catalog),
        UiAction::None
    );
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &catalog),
        UiAction::None
    );
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &catalog),
        UiAction::Submit(refresh)
    );
    assert_eq!(
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &catalog),
        UiAction::Submit(session_action)
    );
}

#[tokio::test]
async fn workspace_reference_menu_inserts_a_file() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join("src")).expect("source directory");
    std::fs::write(workspace.path().join("src/lib.rs"), "").expect("source file");
    let catalog = catalog(workspace.path());
    catalog
        .start_workspace_inventory(true)
        .await
        .expect("workspace inventory");
    let mut state = state();
    state.input = "review @lib".into();
    state.cursor = state.input.len();

    state.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &catalog);

    assert_eq!(state.input, "review src/lib.rs");
}

#[tokio::test]
async fn remote_workspace_inventory_does_not_read_the_client_filesystem() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("client-only.rs"), "").expect("client file");
    let catalog = catalog(workspace.path());
    catalog
        .start_workspace_inventory(false)
        .await
        .expect("disabled workspace inventory");

    assert!(catalog.reference_suggestions('@', "client").is_empty());
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
fn option_backspace_deletes_the_previous_word_and_trailing_space() {
    let mut state = state();
    state.input = "hello   world  ".into();
    state.cursor = state.input.len();

    state.handle_key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT),
        &default_catalog(),
    );

    assert_eq!((state.input.as_str(), state.cursor), ("hello   ", 8));
}

#[test]
fn option_backspace_deletes_a_collapsed_paste_atomically() {
    let mut state = state();
    state.input = "foo.".into();
    state.cursor = state.input.len();
    state.insert_paste("pasted\ncontent");

    state.handle_key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT),
        &default_catalog(),
    );

    assert_eq!((state.input.as_str(), state.pastes.len()), ("foo.", 0));
}

#[test]
fn arrow_up_recalls_composer_history_and_ctrl_t_toggles_transcript() {
    let catalog = default_catalog();
    let mut state = state();
    state.remember_composer_input("previous prompt".into());
    state.attachments.push(attachment("draft.png"));

    state.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &catalog);
    assert_eq!(state.input, "previous prompt");
    assert_eq!(state.attachments, vec![attachment("draft.png")]);
    assert!(state.preview.is_none());

    state.handle_key(
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
        &catalog,
    );
    assert!(matches!(
        state.preview.as_ref().map(|preview| &preview.content),
        Some(PreviewContent::LiveTranscript)
    ));

    state.handle_key(
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
        &catalog,
    );
    assert!(state.preview.is_none());
}

#[test]
fn approval_preserves_an_in_progress_draft() {
    let mut state = state();
    state.input = "steer after approval".into();
    state.cursor = state.input.len();
    state.attachments.push(attachment("draft.png"));

    state.handle_agent_event(
        EventMsg::ExecApprovalRequest(mobius::protocol::ExecApprovalRequestEvent {
            id: "approval".into(),
            turn_id: "turn".into(),
            calls: Vec::new(),
            reason: "test".into(),
        }),
        Vec::new(),
    );
    let mut finished_upload = attachment("finished-upload.png");
    finished_upload.id = "471f43e6-6886-483c-bfe4-771db52614c8".into();
    state.attachments.push(finished_upload.clone());

    assert_eq!(
        state.handle_key(
            KeyEvent::new_with_kind(
                KeyCode::Char('y'),
                KeyModifiers::NONE,
                ratatui::crossterm::event::KeyEventKind::Repeat,
            ),
            &default_catalog(),
        ),
        UiAction::None
    );
    assert_eq!(
        state.handle_key(
            KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
            &default_catalog(),
        ),
        UiAction::Submit(Op::ExecApproval {
            id: "approval".into(),
            decision: ReviewDecision::Approved,
        })
    );
    assert_eq!(state.input, "steer after approval");
    assert_eq!(
        state.attachments,
        vec![attachment("draft.png"), finished_upload]
    );
    assert!(state.is_working());
}

#[test]
fn ctrl_or_alt_v_requests_native_clipboard_paste() {
    let catalog = default_catalog();
    for modifiers in [
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ] {
        let mut state = state();
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Char('v'), modifiers), &catalog),
            UiAction::PasteClipboard
        );
    }
}

#[test]
fn attachment_only_submission_is_a_user_turn() {
    let mut state = state();
    state.attachments.push(attachment("photo.png"));

    assert_eq!(
        state.submit_input(&default_catalog()),
        UiAction::Submit(Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::User,
                text: String::new(),
                attachments: vec![attachment("photo.png")],
                requested_delivery: None,
                target_turn_id: None,
            },
        })
    );
    assert!(state.attachments.is_empty());
}

#[test]
fn active_turn_preserves_attachment_draft_instead_of_steering() {
    let mut state = state();
    state.active_turn = Some("turn".into());
    state.input = "next request".into();
    state.cursor = state.input.len();
    state.attachments.push(attachment("report.pdf"));

    assert_eq!(state.submit_input(&default_catalog()), UiAction::None);
    assert_eq!(state.input, "next request");
    assert_eq!(state.attachments, vec![attachment("report.pdf")]);
}

#[test]
fn upload_in_progress_preserves_the_whole_draft() {
    let mut state = state();
    state.input = "describe these".into();
    state.cursor = state.input.len();
    state.attachments.push(attachment("ready.png"));
    state.upload_in_progress = true;

    assert_eq!(state.submit_input(&default_catalog()), UiAction::None);
    assert_eq!(state.input, "describe these");
    assert_eq!(state.attachments, vec![attachment("ready.png")]);
}

#[test]
fn backspace_removes_the_last_attachment_from_an_empty_composer() {
    let mut state = state();
    state.attachments.push(attachment("first.png"));
    state.attachments.push(attachment("second.png"));

    state.handle_key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        &default_catalog(),
    );

    assert_eq!(state.attachments, vec![attachment("first.png")]);
}

#[test]
fn normal_pasted_paths_remain_text() {
    let mut state = state();
    state.insert_paste("/tmp/photo.png");

    assert_eq!(state.input, "/tmp/photo.png");
    assert!(state.attachments.is_empty());
}

fn attachment(name: &str) -> mobius::protocol::SessionFileReference {
    mobius::protocol::SessionFileReference {
        id: "3d46beff-7e84-46ea-859a-e66b4614a79b".into(),
        name: name.into(),
        size: 4,
        media_type: if name.ends_with(".pdf") {
            "application/pdf"
        } else {
            "image/png"
        }
        .into(),
    }
}
