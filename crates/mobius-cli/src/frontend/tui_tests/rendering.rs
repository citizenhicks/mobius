use mobius::middleware::Middleware;
use mobius::middleware::messages::Messages;

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
        content: Default::default(),
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
        content: Default::default(),
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
fn pending_tool_compacts_its_display_without_losing_completed_detail() {
    let mut state = state();
    state.transcript.clear();
    let detail = format!(
        "input 0\ninput 1\ninput 2\n{}\nlast input",
        "x".repeat(MAX_TOOL_DETAIL_BYTES + 1)
    );
    state.apply_block(rendered(FrontendBlock {
        id: Some("turn/bash".into()),
        group: None,
        update: FrontendBlockUpdate::Replace,
        state: FrontendBlockState::Pending,
        role: FrontendBlockRole::Tool,
        title: "Bash".into(),
        text: detail.clone(),
        symbol: None,
        content: Default::default(),
        format: FrontendBlockFormat::PlainText,
        tone: FrontendTone::Neutral,
        files: Vec::new(),
    }));
    assert_eq!(
        rendered_text(&view::live_transcript_lines(&mut state, 0, 80)),
        "• Bash\n  └ input 0\n    input 1\n    input 2\n    …"
    );
    state.apply_block(rendered(FrontendBlock {
        id: Some("turn/bash".into()),
        group: None,
        update: FrontendBlockUpdate::Append,
        state: FrontendBlockState::Complete,
        role: FrontendBlockRole::Tool,
        title: "Bash".into(),
        text: "ok".into(),
        symbol: None,
        content: Default::default(),
        format: FrontendBlockFormat::PlainText,
        tone: FrontendTone::Success,
        files: Vec::new(),
    }));

    let entry = state.transcript.front().expect("tool entry");
    assert_eq!(entry.detail.as_deref(), Some(detail.as_str()));
    assert_eq!(entry.text, "ok");
    let completed = rendered_text(&view::live_transcript_lines(&mut state, 0, 80));
    assert!(completed.contains("last input\n    ok"), "{completed}");
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
    let widget = |id: &str, text: &str, delivery: &str| {
        EventMsg::Frontend(FrontendEvent::Widget {
            capability: "messages".into(),
            item: FrontendWidget {
                id: id.into(),
                slot: FrontendSlot::TranscriptTail,
                text: text.into(),
                tone: FrontendTone::Neutral,
                symbol: Some(FrontendSymbol::Custom(delivery.into())),
                icon_only: false,
                progress: None,
                content: None,
                action: None,
            },
        })
    };
    state.handle_agent_event(widget("z-older", "older", "steer"), Vec::new());
    state.handle_agent_event(widget("a-newer", "newer", "queue"), Vec::new());

    let lines = view::live_transcript_lines(&mut state, 0, 80);

    assert_eq!(
        rendered_text(&lines),
        "• working\n\n┊ Steer\n┊ older\n\n┊ Queue\n┊ newer"
    );

    state.handle_agent_event(
        EventMsg::Frontend(FrontendEvent::RemoveWidget {
            capability: "messages".into(),
            id: "z-older".into(),
        }),
        Vec::new(),
    );
    let lines = view::live_transcript_lines(&mut state, 0, 80);
    assert_eq!(rendered_text(&lines), "• working\n\n┊ Queue\n┊ newer");
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
        content: Default::default(),
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
                content: Default::default(),
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
        content: Default::default(),
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
            reply: None,
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
fn peer_messages_use_activity_rows_in_live_history_and_preview_transcripts() {
    let messages = Messages::default();
    let text = "Check the protocol boundary\n  Preserve this detail.";
    for (handle, symbol) in [
        ("curie", None),
        ("voice agent", Some(FrontendSymbol::Custom("voice".into()))),
    ] {
        let event = EventMsg::Message(MessageEvent {
            author: MessageAuthor::Peer {
                message_id: "message".into(),
                session_id: "session".into(),
                handle: handle.into(),
                symbol,
            },
            delivery: MessageDelivery::Steer,
            text: text.into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        });
        let block = messages.render(&event, "main").expect("peer activity");
        let mut record = recorded(
            event,
            vec![RenderedBlock {
                capability: messages.name().into(),
                block,
            }],
            None,
        );
        record.event.submission_id = Some("submission".into());

        let mut live = state();
        events::handle_gateway_event(&mut live, record.clone(), true);
        let mut history = state();
        events::handle_gateway_history(&mut history, vec![record.clone()]);
        let mut preview = state();
        preview.preview_request_id = Some("preview-request".into());
        let mut page = preview_record("child", "latest", FrontendPreviewUpdate::Replace, &[], None);
        page.preview.as_mut().expect("preview").events = vec![RenderedEvent {
            submission_id: record.event.submission_id,
            recorded_at_ms: record.recorded_at_ms,
            event: record.event.msg,
            blocks: record.blocks,
        }];
        events::handle_gateway_event(&mut preview, page, true);

        let title = format!("Message received from @{handle}");
        for transcript in [
            &live.transcript,
            &history.transcript,
            &snapshot(&preview).transcript,
        ] {
            assert_eq!(transcript.len(), 1);
            let entry = transcript.front().expect("peer activity");
            assert_eq!(
                (entry.title.as_deref(), entry.text.as_str(), entry.role),
                (
                    Some(title.as_str()),
                    text,
                    Some(FrontendBlockRole::Activity)
                )
            );
            assert!(matches!(entry.tone, TranscriptTone::Neutral));
        }
        assert!(
            [&live, &history, &preview]
                .iter()
                .all(|state| state.composer_history.is_empty())
        );
    }
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

#[test]
fn transcript_viewport_matches_full_paragraph_for_unicode_scroll_and_resize() {
    use ratatui::layout::Rect;
    use ratatui::style::Modifier;
    use ratatui::text::Text;
    use ratatui::widgets::{Block, Paragraph, Wrap};

    fn compare_chat(
        actual: &mut TuiState,
        reference: &mut TuiState,
        catalog: &UiCatalog,
        width: u16,
        height: u16,
        scroll: usize,
        label: &str,
    ) {
        actual.transcript_viewport.scroll = scroll;
        reference.transcript_viewport.scroll = scroll;
        let mut actual_terminal =
            Terminal::new(TestBackend::new(width, height)).expect("actual terminal");
        actual_terminal
            .draw(|frame| view::render(frame, actual, catalog))
            .expect("actual draw");

        let area = Rect::new(0, 0, width, height - 5);
        let lines = view::live_transcript_lines(reference, 0, width);
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        let rendered_lines = paragraph.line_count(width);
        reference
            .transcript_viewport
            .update(rendered_lines, usize::from(area.height));
        let paragraph_scroll = reference
            .transcript_viewport
            .effective_scroll()
            .min(usize::from(u16::MAX)) as u16;
        let mut reference_terminal =
            Terminal::new(TestBackend::new(width, height)).expect("reference terminal");
        reference_terminal
            .draw(|frame| {
                frame.render_widget(
                    Block::default().style(current().style(Role::Canvas)),
                    frame.area(),
                );
                frame.render_widget(paragraph.scroll((paragraph_scroll, 0)), area);
            })
            .expect("reference draw");

        for y in 0..area.height {
            for x in 0..area.width {
                assert_eq!(
                    actual_terminal.backend().buffer()[(x, y)],
                    reference_terminal.backend().buffer()[(x, y)],
                    "chat mismatch at ({x}, {y}), {label}"
                );
            }
        }
        assert_eq!(
            actual.transcript_viewport.content_height, reference.transcript_viewport.content_height,
            "chat content height mismatch, {label}"
        );
        assert_eq!(
            actual.transcript_viewport.effective_scroll(),
            reference.transcript_viewport.effective_scroll(),
            "chat scroll mismatch, {label}"
        );
    }

    fn compare_preview(
        actual: &mut TuiState,
        reference: &mut TuiState,
        width: u16,
        height: u16,
        scroll: usize,
        label: &str,
    ) {
        actual
            .preview
            .as_mut()
            .expect("actual preview")
            .viewport
            .scroll = scroll;
        reference
            .preview
            .as_mut()
            .expect("reference preview")
            .viewport
            .scroll = scroll;
        let mut actual_terminal =
            Terminal::new(TestBackend::new(width, height)).expect("actual preview terminal");
        actual_terminal
            .draw(|frame| view::render_preview(frame, actual))
            .expect("actual preview draw");

        let area =
            crate::frontend::dashboard::centered_area(Rect::new(0, 0, width, height), 92, 88);
        let inner = Block::bordered().inner(area);
        let live = matches!(
            reference
                .preview
                .as_ref()
                .expect("reference preview")
                .content,
            PreviewContent::LiveTranscript
        );
        let mut lines = if live {
            view::live_transcript_lines(reference, 0, inner.width)
        } else if let PreviewContent::Snapshot(snapshot) = &mut reference
            .preview
            .as_mut()
            .expect("reference preview")
            .content
        {
            view::transcript_lines(snapshot.transcript.iter_mut(), inner.width, None, false)
        } else {
            Vec::new()
        };
        if lines.is_empty() {
            lines.push(Line::styled(
                "No transcript events.",
                current().style(Role::Muted).add_modifier(Modifier::ITALIC),
            ));
        }
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        let rendered_lines = paragraph.line_count(inner.width);
        let preview = reference.preview.as_mut().expect("reference preview");
        preview
            .viewport
            .update(rendered_lines, usize::from(inner.height));
        let paragraph_scroll = preview
            .viewport
            .effective_scroll()
            .min(usize::from(u16::MAX)) as u16;
        let mut reference_terminal =
            Terminal::new(TestBackend::new(width, height)).expect("reference preview terminal");
        reference_terminal
            .draw(|frame| {
                frame.render_widget(
                    paragraph
                        .style(current().style(Role::Canvas))
                        .scroll((paragraph_scroll, 0)),
                    inner,
                );
            })
            .expect("reference preview draw");

        for y in inner.y..inner.bottom() {
            for x in inner.x..inner.right() {
                assert_eq!(
                    actual_terminal.backend().buffer()[(x, y)],
                    reference_terminal.backend().buffer()[(x, y)],
                    "preview mismatch at ({x}, {y}), {label}"
                );
            }
        }
        let actual_preview = actual.preview.as_ref().expect("actual preview");
        assert_eq!(
            actual_preview.viewport.content_height, preview.viewport.content_height,
            "preview content height mismatch, {label}"
        );
        assert_eq!(
            actual_preview.viewport.effective_scroll(),
            preview.viewport.effective_scroll(),
            "preview scroll mismatch, {label}"
        );
    }

    fn populated_state() -> TuiState {
        let mut state = state();
        state.transcript.clear();
        for index in 0..12 {
            state.apply_block(rendered(FrontendBlock {
                id: Some(format!("entry-{index}")),
                group: Some(format!("group-{}", index / 2)),
                update: FrontendBlockUpdate::Replace,
                state: FrontendBlockState::Complete,
                role: FrontendBlockRole::Notice,
                title: format!("Entry {index}"),
                text: format!("λ界 e\u{301} 👩‍💻 entry {index} {}", "wide ".repeat(12)),
                symbol: None,
                content: Default::default(),
                format: FrontendBlockFormat::PlainText,
                tone: FrontendTone::Neutral,
                files: Vec::new(),
            }));
        }
        state.push("› user e\u{301} 👩‍💻", TranscriptTone::User);
        state.push("- **A long item with enough words to wrap onto the next line**\n\n> a quote with enough words to wrap in a narrow terminal\n\n| Name | Count |\n|---|---:|\n| long table value | 12345 |\n\n```rust\nfn main() {\n\n    println!(\"hello\");\n}\n```", TranscriptTone::Assistant);
        state.streaming = "streaming λ界 ".repeat(8);
        state.reasoning = "reasoning e\u{301} 👩‍💻".into();
        state.widgets.push((
            ("test".into(), "tail".into()),
            FrontendWidget {
                id: "tail".into(),
                slot: FrontendSlot::TranscriptTail,
                text: "tail λ界".into(),
                tone: FrontendTone::Warning,
                symbol: Some(FrontendSymbol::Custom("tail".into())),
                icon_only: false,
                progress: None,
                content: None,
                action: None,
            },
        ));
        state
    }

    let catalog = default_catalog();
    for (width, height, scroll) in [(40, 15, 0), (40, 15, 7), (67, 21, usize::MAX)] {
        compare_chat(
            &mut populated_state(),
            &mut populated_state(),
            &catalog,
            width,
            height,
            scroll,
            "initial chat",
        );
    }

    let mut actual = populated_state();
    let mut reference = populated_state();
    compare_chat(
        &mut actual,
        &mut reference,
        &catalog,
        40,
        15,
        usize::MAX,
        "before resize",
    );
    for state in [&mut actual, &mut reference] {
        state.apply_block(rendered(FrontendBlock {
            id: Some("entry-3".into()),
            group: Some("group-1".into()),
            update: FrontendBlockUpdate::Replace,
            state: FrontendBlockState::Complete,
            role: FrontendBlockRole::Notice,
            title: "Entry 3 updated".into(),
            text: "updated e\u{301} 👩‍💻 content ".repeat(10),
            symbol: None,
            content: Default::default(),
            format: FrontendBlockFormat::PlainText,
            tone: FrontendTone::Success,
            files: Vec::new(),
        }));
    }
    for width in [40, 67] {
        compare_chat(
            &mut actual,
            &mut reference,
            &catalog,
            width,
            21,
            5,
            "same-state update then resize",
        );
    }

    for (width, height, scroll) in [(40, 20, 0), (67, 24, 3), (67, 24, usize::MAX)] {
        let mut live_actual = populated_state();
        let mut live_reference = populated_state();
        live_actual.open_transcript_preview();
        live_reference.open_transcript_preview();
        compare_preview(
            &mut live_actual,
            &mut live_reference,
            width,
            height,
            scroll,
            "live preview",
        );

        let mut snapshot_actual = state();
        let mut snapshot_reference = state();
        for snapshot in [&mut snapshot_actual, &mut snapshot_reference] {
            snapshot.preview_request_id = Some("preview-request".into());
            events::handle_gateway_event(
                snapshot,
                preview_record(
                    "snapshot",
                    "latest",
                    FrontendPreviewUpdate::Replace,
                    &["e\u{301} 👩‍💻 snapshot content ".repeat(8).as_str()],
                    None,
                ),
                true,
            );
        }
        compare_preview(
            &mut snapshot_actual,
            &mut snapshot_reference,
            width,
            height,
            scroll,
            "snapshot preview",
        );
    }
}

#[test]
fn markdown_is_rendered_in_chat_and_the_ctrl_t_transcript() {
    let mut state = state();
    state.transcript.clear();
    state.push(
        "# Result\n\n- [x] **done**\n\n```rust\nfn main() {}\n```",
        TranscriptTone::Assistant,
    );
    let catalog = default_catalog();
    for preview in [false, true] {
        if preview {
            state.handle_key(
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
                &catalog,
            );
            assert!(matches!(
                state.preview.as_ref().unwrap().content,
                PreviewContent::LiveTranscript
            ));
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).expect("terminal");
        terminal
            .draw(|frame| {
                if preview {
                    view::render_preview(frame, &mut state);
                } else {
                    view::render(frame, &mut state, &catalog);
                }
            })
            .expect("draw");
        let text = terminal.backend().to_string();
        assert!(text.contains("Result") && text.contains("- [x] done"));
        assert!(text.contains("│ fn main() {}"));
        assert!(!text.contains("**done**") && !text.contains("```"));
    }
}
