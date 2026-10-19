use super::support::*;
use super::*;

#[test]
fn gateway_history_prepends_earlier_messages() {
    let mut state = state();
    state.push("newest", TranscriptTone::Assistant);

    events::handle_gateway_history(
        &mut state,
        vec![recorded(
            EventMsg::Message(MessageEvent {
                author: MessageAuthor::User,
                delivery: MessageDelivery::Turn,
                text: "oldest".into(),
                attachments: Vec::new(),
                reply: None,
                message_target: None,
            }),
            Vec::new(),
            None,
        )],
    );

    assert_eq!(
        state
            .transcript
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["› oldest", "newest"]
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
        let paragraph_scroll = reference.transcript_viewport.effective_scroll();
        let paragraph_scroll = u16::try_from(paragraph_scroll).unwrap_or(u16::MAX);
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
        let paragraph_scroll = preview.viewport.effective_scroll();
        let paragraph_scroll = u16::try_from(paragraph_scroll).unwrap_or(u16::MAX);
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
                role: FrontendBlockRole::Notice,
                title: format!("Entry {index}"),
                text: format!("λ界 e\u{301} 👩‍💻 entry {index} {}", "wide ".repeat(12)),
                ..Default::default()
            }));
        }
        state.push("› user e\u{301} 👩‍💻", TranscriptTone::User);
        state.push("- **A long item with enough words to wrap onto the next line**\n\n> a quote with enough words to wrap in a narrow terminal\n\n| Name | Count |\n|---|---:|\n| long table value | 12345 |\n\n```rust\nfn main() {\n\n    println!(\"hello\");\n}\n```", TranscriptTone::Assistant);
        state.streaming = "streaming λ界 ".repeat(8);
        state.reasoning = "reasoning e\u{301} 👩‍💻".into();
        state.widgets.push((
            "test".into(),
            Arc::new(FrontendWidget {
                id: "tail".into(),
                slot: FrontendSlot::TranscriptTail,
                text: "tail λ界".into(),
                tone: FrontendTone::Warning,
                symbol: Some(FrontendSymbol::Custom("tail".into())),
                icon_only: false,
                progress: None,
                content: None,
                action: None,
            }),
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
            role: FrontendBlockRole::Notice,
            title: "Entry 3 updated".into(),
            text: "updated e\u{301} 👩‍💻 content ".repeat(10),
            tone: FrontendTone::Success,
            ..Default::default()
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
