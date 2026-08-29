use super::support::*;
use super::*;

#[test]
fn completed_diff_replaces_the_pending_block_with_a_styled_diff() {
    let mut state = state();
    state.transcript.clear();
    state.apply_block(rendered(FrontendBlock {
        id: Some("turn/patch".into()),
        group: None,
        update: FrontendBlockUpdate::Replace,
        state: FrontendBlockState::Pending,
        role: FrontendBlockRole::Tool,
        title: "Edit note.rs".into(),
        text: String::new(),
        symbol: None,
        format: FrontendBlockFormat::PlainText,
        tone: FrontendTone::Neutral,
        files: Vec::new(),
    }));
    view::live_transcript_lines(&mut state, 0, 80);
    assert_eq!(
        state
            .transcript
            .front()
            .and_then(|entry| entry.rendered.as_ref())
            .map(|(width, _)| *width),
        Some(80)
    );
    state.apply_block(rendered(FrontendBlock {
        id: Some("turn/patch".into()),
        group: None,
        update: FrontendBlockUpdate::Replace,
        state: FrontendBlockState::Complete,
        role: FrontendBlockRole::Tool,
        title: "Edit note.rs".into(),
        text: "--- note.rs\n+++ note.rs\n@@ -1,5 +1,5 @@\n-fn old_name() {}\n+fn new_name() {}\n keep_one();\n-let removed = false;\n keep_two();\n+let added = true;\n keep_three();\n".into(),
        symbol: None,
        format: FrontendBlockFormat::UnifiedDiff,
        tone: FrontendTone::Success,
        files: Vec::new(),
    }));
    assert!(
        state
            .transcript
            .front()
            .is_some_and(|entry| entry.rendered.is_none())
    );

    assert_eq!(
        state.transcript.front().map(|entry| entry.format),
        Some(FrontendBlockFormat::UnifiedDiff)
    );
    let lines = view::live_transcript_lines(&mut state, 0, 80);
    let text = rendered_text(&lines);
    assert!(text.contains("• Edited note.rs (+2 -2)"), "{text}");
    assert!(text.contains("    1 -fn old_name() {}"), "{text}");
    assert!(text.contains("    1 +fn new_name() {}"), "{text}");
    assert!(!text.contains("• Edit note.rs"), "{text}");

    let changed_delete = lines
        .iter()
        .find(|line| rendered_text(std::slice::from_ref(line)).contains("-fn old_name"))
        .expect("changed delete");
    let changed_insert = lines
        .iter()
        .find(|line| rendered_text(std::slice::from_ref(line)).contains("+fn new_name"))
        .expect("changed insert");
    let pure_delete = lines
        .iter()
        .find(|line| rendered_text(std::slice::from_ref(line)).contains("-let removed"))
        .expect("pure delete");
    let pure_insert = lines
        .iter()
        .find(|line| rendered_text(std::slice::from_ref(line)).contains("+let added"))
        .expect("pure insert");

    assert_eq!(
        changed_delete.style.bg,
        Some(current().diff_delete_background())
    );
    assert_eq!(
        changed_insert.style.bg,
        Some(current().diff_add_background())
    );
    assert_eq!(
        pure_delete.style.bg,
        Some(current().diff_delete_background())
    );
    assert_eq!(pure_insert.style.bg, Some(current().diff_add_background()));
    assert!(
        [&changed_delete, &changed_insert, &pure_delete, &pure_insert]
            .into_iter()
            .all(|line| line.width() == 80)
    );
    assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
        span.content == "fn" && span.style.fg != Some(current().color(Role::Text))
    }));

    let narrow_lines = view::live_transcript_lines(&mut state, 0, 40);
    let narrow_insert = narrow_lines
        .iter()
        .find(|line| rendered_text(std::slice::from_ref(line)).contains("+fn new_name"))
        .expect("narrow insert");
    assert_eq!(
        (
            state
                .transcript
                .front()
                .and_then(|entry| entry.rendered.as_ref())
                .map(|(width, _)| *width),
            narrow_insert.width(),
        ),
        (Some(40), 40)
    );
}

#[test]
fn completed_tool_keeps_its_detail_inline_and_indents_the_result() {
    let mut state = state();
    state.transcript.clear();
    state.apply_block(rendered(FrontendBlock {
        id: Some("turn/bash".into()),
        group: None,
        update: FrontendBlockUpdate::Replace,
        state: FrontendBlockState::Pending,
        role: FrontendBlockRole::Tool,
        title: "Bash".into(),
        text: "cargo test".into(),
        symbol: None,
        format: FrontendBlockFormat::PlainText,
        tone: FrontendTone::Neutral,
        files: Vec::new(),
    }));
    state.apply_block(rendered(FrontendBlock {
        id: Some("turn/bash".into()),
        group: None,
        update: FrontendBlockUpdate::Append,
        state: FrontendBlockState::Complete,
        role: FrontendBlockRole::Tool,
        title: "Bash".into(),
        text: "ok".into(),
        symbol: None,
        format: FrontendBlockFormat::PlainText,
        tone: FrontendTone::Success,
        files: Vec::new(),
    }));

    assert_eq!(
        rendered_text(&view::live_transcript_lines(&mut state, 0, 80)),
        "• Bash cargo test\n  └ ok"
    );
}

#[test]
fn capability_header_is_live_styled_and_transparent() {
    let catalog = default_catalog();
    let mut state = state();
    state.transcript.clear();
    state.widgets.push((
        ("extensions".into(), "count".into()),
        FrontendWidget {
            id: "count".into(),
            slot: FrontendSlot::Header,
            text: "extensions 2".into(),
            tone: FrontendTone::Neutral,
            symbol: None,
            icon_only: false,
            progress: None,
            content: None,
            action: None,
        },
    ));
    let mut terminal = Terminal::new(TestBackend::new(50, 15)).expect("terminal");
    terminal
        .draw(|frame| view::render(frame, &mut state, &catalog))
        .expect("live pane draw");
    let live_pane = terminal.backend().to_string();
    let extension_cell = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .find(|cell| cell.symbol() == "2")
        .expect("styled capability cell");

    assert!(live_pane.contains("extensions 2"));
    assert_eq!(
        (extension_cell.fg, extension_cell.bg),
        (current().color(Role::Neutral), Color::Reset)
    );
}

#[test]
fn transcript_tail_widgets_render_after_live_output_in_arrival_order() {
    let mut state = state();
    state.transcript.clear();
    state.streaming = "working".into();
    let widget = |id: &str, text: &str| {
        EventMsg::Frontend(FrontendEvent::Widget {
            capability: "messages".into(),
            item: FrontendWidget {
                id: id.into(),
                slot: FrontendSlot::TranscriptTail,
                text: text.into(),
                tone: FrontendTone::Neutral,
                symbol: None,
                icon_only: false,
                progress: None,
                content: None,
                action: None,
            },
        })
    };
    state.handle_agent_event(widget("z-older", "older"), Vec::new());
    state.handle_agent_event(widget("a-newer", "newer"), Vec::new());

    let lines = view::live_transcript_lines(&mut state, 0, 80);

    assert_eq!(
        rendered_text(&lines),
        "• working\n\n┊ Messages\n┊ older\n\n┊ Messages\n┊ newer"
    );

    state.handle_agent_event(
        EventMsg::Frontend(FrontendEvent::RemoveWidget {
            capability: "messages".into(),
            id: "z-older".into(),
        }),
        Vec::new(),
    );
    let lines = view::live_transcript_lines(&mut state, 0, 80);
    assert_eq!(rendered_text(&lines), "• working\n\n┊ Messages\n┊ newer");
}

#[test]
fn nord_transcript_stays_styled_and_transparent_in_chat_and_preview() {
    let catalog = default_catalog();
    let mut state = state();
    state.transcript.clear();
    state.push_entry("λ".into(), TranscriptTone::Warning);
    let mut terminal = Terminal::new(TestBackend::new(40, 16)).expect("terminal");

    terminal
        .draw(|frame| view::render(frame, &mut state, &catalog))
        .expect("chat draw");
    let chat_cell = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .find(|cell| cell.symbol() == "λ")
        .expect("styled chat cell");
    assert_eq!(
        (chat_cell.fg, chat_cell.bg),
        (current().color(Role::Warning), Color::Reset)
    );

    state.open_transcript_preview();
    terminal
        .draw(|frame| view::render_preview(frame, &mut state))
        .expect("preview draw");
    let preview = terminal.backend().buffer();
    let preview_cell = preview
        .content()
        .iter()
        .find(|cell| cell.symbol() == "λ")
        .expect("styled preview cell");

    assert_eq!(
        (preview_cell.fg, preview_cell.bg),
        (current().color(Role::Warning), Color::Reset)
    );
    assert!(preview.content().iter().all(|cell| cell.bg == Color::Reset));
}

#[test]
fn empty_chat_shows_the_agent_card_without_polluting_the_transcript() {
    let mut state = state();
    let card = view::welcome_card(&state);

    assert!(state.transcript.is_empty());
    assert!(!card.contains("⣠⡤⢶"));
    assert!(card.contains("MÖBIUS"));
    assert!(card.contains("model: kimi-k3 · high"));

    state.push("hello", TranscriptTone::User);
    let rendered = rendered_text(&view::live_transcript_lines(&mut state, 0, 80));
    assert!(!rendered.contains("MÖBIUS"));
}

#[test]
fn narrow_terminal_keeps_session_card_and_compact_footer() {
    let catalog = default_catalog();
    let mut state = state();
    state.cwd = "/work/mobius".into();
    let mut terminal = Terminal::new(TestBackend::new(35, 15)).expect("terminal");

    terminal
        .draw(|frame| view::render(frame, &mut state, &catalog))
        .expect("draw");
    let rendered = terminal.backend().to_string();

    assert!(rendered.contains("MÖBIUS"), "{rendered}");
    assert!(!rendered.contains("⣠⡤⢶"), "{rendered}");
    assert!(rendered.contains("model: kimi-k3 · high"), "{rendered}");
    assert!(rendered.contains("kimi-k3 high"), "{rendered}");
    assert!(rendered.contains("╰"), "{rendered}");
}

#[test]
fn block_identity_is_scoped_by_explicit_capability() {
    let block = |title: &str| FrontendBlock {
        id: Some("same-id".into()),
        group: None,
        update: FrontendBlockUpdate::Replace,
        state: FrontendBlockState::Complete,
        role: FrontendBlockRole::Notice,
        title: title.into(),
        text: String::new(),
        symbol: None,
        files: Vec::new(),
        format: FrontendBlockFormat::PlainText,
        tone: FrontendTone::Neutral,
    };
    let mut state = state();
    state.apply_block(RenderedBlock {
        capability: "alpha".into(),
        block: block("Alpha"),
    });
    state.apply_block(RenderedBlock {
        capability: "beta".into(),
        block: block("Beta"),
    });
    state.apply_block(RenderedBlock {
        capability: "alpha".into(),
        block: block("Alpha updated"),
    });

    assert_eq!(
        state
            .transcript
            .iter()
            .map(|entry| entry.title.as_deref())
            .collect::<Vec<_>>(),
        [Some("Alpha updated"), Some("Beta")]
    );
}

#[test]
fn gateway_history_preserves_child_diff_rendering() {
    let mut state = state();
    let message = EventMsg::AssistantMessage(mobius::protocol::AssistantMessageEvent {
        session_id: "session".into(),
        turn_id: "turn".into(),
        model_step_id: "step".into(),
        content: vec![ModelStepContent {
            output_index: 0,
            part_index: 0,
            phase: ModelStepContentPhase::FinalAnswer,
            text: "changed the file".into(),
            annotations: Vec::new(),
        }],
        message_target: None,
    });
    events::handle_gateway_history(
        &mut state,
        vec![recorded(
            message,
            vec![rendered(FrontendBlock {
                id: None,
                group: None,
                update: FrontendBlockUpdate::Replace,
                state: FrontendBlockState::Complete,
                role: FrontendBlockRole::Artifact,
                title: String::new(),
                text: "--- a/file\n+++ b/file\n-old\n+new".into(),
                symbol: None,
                format: FrontendBlockFormat::UnifiedDiff,
                tone: FrontendTone::Neutral,
                files: Vec::new(),
            })],
            None,
        )],
    );

    let entry = state.transcript.back().expect("rendered history entry");
    assert_eq!(entry.format, FrontendBlockFormat::UnifiedDiff);
    assert_eq!(entry.text, "--- a/file\n+++ b/file\n-old\n+new");
}

#[test]
fn session_file_block_renders_download_metadata_as_plain_text() {
    let mut state = state();
    state.transcript.clear();
    state.apply_block(rendered(FrontendBlock {
        id: Some("artifacts/turn/file".into()),
        group: None,
        update: FrontendBlockUpdate::Replace,
        state: FrontendBlockState::Complete,
        role: FrontendBlockRole::Artifact,
        title: "Sent report.xlsx".into(),
        text: String::new(),
        symbol: None,
        format: FrontendBlockFormat::PlainText,
        tone: FrontendTone::Success,
        files: vec![mobius::protocol::SessionFileReference {
            id: "file-a".into(),
            name: "report.xlsx".into(),
            size: 42,
            media_type: "application/octet-stream".into(),
        }],
    }));

    assert_eq!(
        state.transcript.front().map(|entry| entry.text.as_str()),
        Some("[file] report.xlsx · application/octet-stream · 42 bytes")
    );
    assert_eq!(
        state
            .transcript
            .front()
            .and_then(|entry| entry.title.as_deref()),
        Some("Sent report.xlsx")
    );
}

#[test]
fn sent_attachment_only_message_is_visible_in_the_transcript() {
    let mut state = state();
    state.transcript.clear();
    state.handle_agent_event(
        EventMsg::Message(MessageEvent {
            author: MessageAuthor::User,
            delivery: MessageDelivery::Turn,
            text: String::new(),
            attachments: vec![mobius::protocol::SessionFileReference {
                id: "3d46beff-7e84-46ea-859a-e66b4614a79b".into(),
                name: "photo.png".into(),
                size: 42,
                media_type: "image/png".into(),
            }],
            message_target: None,
        }),
        Vec::new(),
    );

    assert_eq!(
        state.transcript.front().map(|entry| entry.text.as_str()),
        Some("› [file] photo.png · 42 bytes")
    );
}

#[test]
fn peer_message_renders_its_handle_without_entering_composer_history() {
    let mut state = state();
    state.transcript.clear();
    state.handle_agent_event(
        EventMsg::Message(MessageEvent {
            author: MessageAuthor::Peer {
                message_id: "message".into(),
                session_id: "session".into(),
                handle: "curie".into(),
            },
            delivery: MessageDelivery::Steer,
            text: "Check the protocol boundary".into(),
            attachments: Vec::new(),
            message_target: None,
        }),
        Vec::new(),
    );

    assert_eq!(
        (
            state.transcript.front().map(|entry| entry.text.as_str()),
            state.composer_history.len(),
        ),
        (Some("@curie › Check the protocol boundary"), 0)
    );
}

#[test]
fn transcript_text_strips_terminal_control_characters() {
    let mut state = state();
    state.push("unsafe \u{1b}[31mred\u{1b}[0m", TranscriptTone::Warning);

    assert_eq!(
        state.transcript.back().expect("entry").text,
        "unsafe [31mred[0m"
    );
}
