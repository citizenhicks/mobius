use super::support::*;
use super::*;

#[test]
fn capped_composer_keeps_a_wide_wrapped_cursor_visible() {
    let catalog = default_catalog();
    let mut state = state();
    state.transcript.clear();
    state.composer.text = "界".repeat(200);
    state.composer.cursor = state.composer.text.len();
    let mut terminal = Terminal::new(TestBackend::new(20, 10)).expect("terminal");

    terminal
        .draw(|frame| view::render(frame, &mut state, &catalog))
        .expect("draw");

    assert!(terminal.backend().to_string().contains('█'));
}
