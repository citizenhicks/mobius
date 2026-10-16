use super::support::*;
use super::*;

#[test]
fn another_clients_preview_cannot_open_or_replace_the_requested_preview() {
    let mut state = state();
    state.preview_request_id = Some("preview-request".into());
    let foreign = || {
        let mut record = preview_record(
            "foreign",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["another client"],
            None,
        );
        record.event.submission_id = Some("another-client-request".into());
        record
    };
    events::handle_gateway_event(&mut state, foreign(), true);
    assert!(state.preview.is_none());
    assert_eq!(state.preview_request_id.as_deref(), Some("preview-request"));

    events::handle_gateway_event(
        &mut state,
        preview_record(
            "ours",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["ours"],
            None,
        ),
        true,
    );
    assert!(state.preview_request_id.is_none());
    events::handle_gateway_event(&mut state, foreign(), true);
    assert_eq!(snapshot(&state).id, "ours");
    assert_eq!(preview_messages(snapshot(&state)), ["› ours"]);
}

#[test]
fn snapshot_preview_prepends_an_older_page_without_losing_the_latest_page() {
    let mut state = state();
    state.preview_request_id = Some("preview-request".into());
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/agent",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["latest"],
            Some(preview_continuation("older")),
        ),
        true,
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
        true,
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
fn snapshot_preview_does_not_merge_a_page_for_another_preview_id() {
    let mut state = state();
    state.preview_request_id = Some("preview-request".into());
    events::handle_gateway_event(
        &mut state,
        preview_record(
            "/root/agent",
            "latest",
            FrontendPreviewUpdate::Replace,
            &["latest"],
            None,
        ),
        true,
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
        true,
    );

    let snapshot = snapshot(&state);
    assert_eq!(
        (snapshot.id.as_str(), preview_messages(snapshot)),
        ("/root/agent", vec!["› latest"])
    );
}
