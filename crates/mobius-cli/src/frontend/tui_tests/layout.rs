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
