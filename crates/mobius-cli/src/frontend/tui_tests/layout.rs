use super::support::*;
use super::*;

#[test]
fn chat_transcript_scrolls_with_the_mouse_wheel() {
    let catalog = default_catalog();
    let mut state = state();
    state.transcript.clear();
    for index in 0..30 {
        state.push_entry(format!("chat row {index}"), TranscriptTone::Neutral);
    }
    let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("terminal");
    terminal
        .draw(|frame| view::render(frame, &mut state, &catalog))
        .expect("chat draw");
    assert!(
        terminal.backend().to_string().contains("chat row 29"),
        "{}",
        terminal.backend()
    );

    for _ in 0..2 {
        state.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
    }
    terminal
        .draw(|frame| view::render(frame, &mut state, &catalog))
        .expect("scrolled chat draw");
    let scrolled = terminal.backend().to_string();
    assert!(!scrolled.contains("chat row 29"), "{scrolled}");

    for _ in 0..2 {
        state.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
    }
    terminal
        .draw(|frame| view::render(frame, &mut state, &catalog))
        .expect("restored chat draw");
    assert!(
        terminal.backend().to_string().contains("chat row 29"),
        "{}",
        terminal.backend()
    );
}

#[test]
fn bordered_composer_grows_to_show_wrapped_input() {
    let catalog = default_catalog();
    let mut state = state();
    state.transcript.clear();
    state.input = "first line\nsecond line\nthird line\nfourth line\nfifth line".into();
    state.cursor = state.input.len();
    let mut terminal = Terminal::new(TestBackend::new(30, 16)).expect("terminal");

    terminal
        .draw(|frame| view::render(frame, &mut state, &catalog))
        .expect("draw");
    let rendered = terminal.backend().to_string();

    assert!(rendered.contains("first line"));
    assert!(rendered.contains("fifth line"));
}

#[test]
fn capped_composer_keeps_a_wide_wrapped_cursor_visible() {
    let catalog = default_catalog();
    let mut state = state();
    state.transcript.clear();
    state.input = "界".repeat(200);
    state.cursor = state.input.len();
    let mut terminal = Terminal::new(TestBackend::new(20, 10)).expect("terminal");

    terminal
        .draw(|frame| view::render(frame, &mut state, &catalog))
        .expect("draw");

    assert!(terminal.backend().to_string().contains('█'));
}

#[test]
fn composer_shows_attached_file_metadata_compactly() {
    let catalog = default_catalog();
    let mut state = state();
    state.transcript.clear();
    state
        .attachments
        .push(mobius::protocol::SessionFileReference {
            id: "3d46beff-7e84-46ea-859a-e66b4614a79b".into(),
            name: "photo.png".into(),
            size: 42,
            media_type: "image/png".into(),
        });
    let mut terminal = Terminal::new(TestBackend::new(50, 12)).expect("terminal");

    terminal
        .draw(|frame| view::render(frame, &mut state, &catalog))
        .expect("draw");
    let rendered = terminal.backend().to_string();

    assert!(rendered.contains("[file] photo.png · 42 bytes"));
    assert!(rendered.contains("» █"));
}

#[test]
fn picker_uses_a_centered_popup() {
    let catalog = default_catalog();
    let mut state = state();
    state.handle_agent_event(
        EventMsg::Frontend(FrontendEvent::Picker {
            title: "Resume chat".into(),
            options: vec![mobius::protocol::FrontendPickerOption {
                label: "Earlier chat".into(),
                description: "idle".into(),
                detail: String::new(),
                symbol: None,
                shows_detail: false,
                op: Op::ResumeSession {
                    session_id: "session-a".into(),
                },
            }],
        }),
        Vec::new(),
    );
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("terminal");

    terminal
        .draw(|frame| view::render(frame, &mut state, &catalog))
        .expect("picker draw");
    let rendered = terminal.backend().to_string();
    let lines = rendered.lines().collect::<Vec<_>>();

    assert!(rendered.contains("Resume chat"), "{rendered}");
    assert!(lines.first().is_some_and(|line| !line.contains('┌')));
    assert!(lines.iter().skip(1).any(|line| line.contains('┌')));
}

#[test]
fn session_picker_fills_its_available_rows_and_keeps_selection_visible() {
    let catalog = default_catalog();
    let mut state = state();
    state.handle_agent_event(
        EventMsg::Frontend(FrontendEvent::Picker {
            title: "Resume chat".into(),
            options: (0..50)
                .map(|index| mobius::protocol::FrontendPickerOption {
                    label: format!("Session {index:02}"),
                    description: "idle".into(),
                    detail: String::new(),
                    symbol: None,
                    shows_detail: false,
                    op: Op::ResumeSession {
                        session_id: format!("session-{index}"),
                    },
                })
                .collect(),
        }),
        Vec::new(),
    );
    for height in [8, 20, 40] {
        let area = crate::frontend::dashboard::centered_area(
            ratatui::layout::Rect::new(0, 0, 80, height),
            86,
            70,
        );
        let visible_rows = usize::from(area.height.saturating_sub(3));
        let mut terminal = Terminal::new(TestBackend::new(80, height)).expect("terminal");
        for selected in [0, 49] {
            state.picker.as_mut().expect("session picker").selected = selected;
            terminal
                .draw(|frame| view::render(frame, &mut state, &catalog))
                .expect("draw");
            let rendered = terminal.backend().to_string();
            assert_eq!(
                rendered.matches("Session ").count(),
                visible_rows,
                "{rendered}"
            );
            assert!(
                rendered.contains(&format!("› Session {selected:02}")),
                "{rendered}"
            );
        }
    }
}
