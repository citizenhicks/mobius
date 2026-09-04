use super::support::*;
use super::*;

#[test]
fn live_transcript_preview_uses_a_centered_popup_and_new_entries() {
    let mut state = state();
    state.transcript.clear();
    state.open_transcript_preview();
    state.push_entry("new live row".into(), TranscriptTone::Neutral);
    let mut terminal = Terminal::new(TestBackend::new(40, 16)).expect("terminal");

    terminal
        .draw(|frame| view::render_preview(frame, &mut state))
        .expect("preview draw");
    let rendered = terminal.backend().to_string();
    let lines = rendered.lines().collect::<Vec<_>>();

    assert!(rendered.contains("new live row"));
    assert!(
        lines.first().is_some_and(|line| !line.contains('┌')),
        "{rendered}"
    );
    assert!(
        lines.iter().skip(1).any(|line| line.contains('┌')),
        "{rendered}"
    );
}

#[test]
fn snapshot_preview_scrolls_with_the_mouse_wheel() {
    let mut state = state();
    events::handle_gateway_event(
        &mut state,
        recorded(
            EventMsg::ContextCompacted,
            Vec::new(),
            Some(RenderedPreview {
                id: "/root/subagent".into(),
                title: "subagent".into(),
                subtitle: String::new(),
                page_id: "subagent:latest".into(),
                update: mobius::protocol::FrontendPreviewUpdate::Replace,
                events: (0..30)
                    .map(|index| RenderedEvent {
                        recorded_at_ms: i64::from(index),
                        event: EventMsg::Message(MessageEvent {
                            author: MessageAuthor::User,
                            delivery: MessageDelivery::Turn,
                            text: format!("subagent row {index}"),
                            attachments: Vec::new(),
                            reply: None,
                            message_target: None,
                        }),
                        blocks: Vec::new(),
                    })
                    .collect(),
                next: None,
            }),
        ),
    );
    let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("terminal");
    terminal
        .draw(|frame| view::render_preview(frame, &mut state))
        .expect("preview draw");
    let bottom = terminal.backend().to_string();
    assert!(bottom.contains("subagent row 29"), "{bottom}");

    assert!(!state.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(state.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }));
    terminal
        .draw(|frame| view::render_preview(frame, &mut state))
        .expect("scrolled preview draw");
    let scrolled = terminal.backend().to_string();
    assert!(scrolled.contains("subagent row 25"), "{scrolled}");
    assert!(!scrolled.contains("subagent row 29"), "{scrolled}");

    state.handle_mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    terminal
        .draw(|frame| view::render_preview(frame, &mut state))
        .expect("restored preview draw");
    let restored = terminal.backend().to_string();
    assert!(restored.contains("subagent row 29"), "{restored}");
}

#[test]
fn snapshot_preview_prepends_an_older_page_without_losing_the_latest_page() {
    let mut state = state();
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/agent",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["latest"],
            Some(preview_continuation("older")),
        ),
    );
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/agent",
            "older",
            FrontendPreviewUpdate::Prepend,
            &["older"],
            None,
        ),
    );

    let snapshot = snapshot(&state);
    assert_eq!(
        (
            snapshot.id.as_str(),
            snapshot.page_ids.len(),
            preview_messages(snapshot),
            snapshot.next.as_ref(),
        ),
        ("/root/agent", 2, vec!["› older", "› latest"], None,)
    );
}

#[test]
fn snapshot_preview_deduplicates_page_ids() {
    let mut state = state();
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/agent",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["latest"],
            None,
        ),
    );
    for message in ["older", "duplicate"] {
        events::handle_gateway_event(
            &mut state,
            preview_record(
                "/root/agent",
                "older",
                FrontendPreviewUpdate::Prepend,
                &[message],
                None,
            ),
        );
    }

    assert_eq!(
        preview_messages(snapshot(&state)),
        vec!["› older", "› latest"]
    );
}

#[test]
fn snapshot_preview_replace_refreshes_a_reused_latest_page_id() {
    let mut state = state();
    for message in ["stale", "fresh"] {
        events::handle_gateway_event(
            &mut state,
            preview_record(
                "/root/agent",
                "latest",
                FrontendPreviewUpdate::Replace,
                &[message],
                None,
            ),
        );
    }

    assert_eq!(preview_messages(snapshot(&state)), vec!["› fresh"]);
}

#[test]
fn snapshot_preview_does_not_merge_a_page_for_another_preview_id() {
    let mut state = state();
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/agent",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["latest"],
            None,
        ),
    );
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/other",
            "older",
            FrontendPreviewUpdate::Prepend,
            &["wrong preview"],
            None,
        ),
    );

    let snapshot = snapshot(&state);
    assert_eq!(
        (snapshot.id.as_str(), preview_messages(snapshot)),
        ("/root/agent", vec!["› latest"])
    );
}

#[test]
fn snapshot_preview_older_key_submits_the_retained_continuation() {
    let mut state = state();
    let next = preview_continuation("older");
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/agent",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["latest"],
            Some(next.clone()),
        ),
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 10)).expect("terminal");
    terminal
        .draw(|frame| view::render_preview(frame, &mut state))
        .expect("preview draw");
    let rendered = terminal.backend().to_string();
    let action = state.handle_key(
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
        &default_catalog(),
    );
    let retained = snapshot(&state).next.as_ref();

    assert_eq!(
        (rendered.contains("O older"), action, retained),
        (true, UiAction::Submit(next.clone()), Some(&next))
    );
}
