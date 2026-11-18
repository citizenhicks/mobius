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
    gateway_event(&mut state, foreign(), true);
    assert!(state.preview.is_none());
    assert_eq!(state.preview_request_id.as_deref(), Some("preview-request"));

    gateway_event(
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
    gateway_event(&mut state, foreign(), true);
    assert_eq!(snapshot(&state).id, "ours".into());
    assert_eq!(preview_messages(snapshot(&state)), ["› ours"]);
}

#[test]
fn snapshot_preview_only_prepends_pages_for_the_current_preview() {
    let mut state = state();
    state.preview_request_id = Some("preview-request".into());
    gateway_event(
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
    gateway_event(
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

    let unchanged = snapshot(&state);
    assert_eq!(
        (&*unchanged.id, preview_messages(unchanged)),
        ("/root/agent", vec!["› latest"])
    );
    gateway_event(
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
            &*snapshot.id,
            snapshot.page_ids.len(),
            preview_messages(snapshot),
            snapshot.next.as_ref(),
        ),
        ("/root/agent", 2, vec!["› older", "› latest"], None,)
    );
}
