use super::*;

#[test]
fn tools_do_not_claim_footer_space() {
    assert!(Tools::coding().frontend().widgets.is_empty());
}

#[test]
fn coding_renderer_preserves_patch_diff_blocks() {
    let diff = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let block = Tools::coding()
        .render(
            &EventMsg::ToolCallEnd(crate::protocol::ToolCallEndEvent {
                turn_id: "turn".into(),
                call_id: "call".into(),
                name: "apply_patch".into(),
                output: diff.into(),
                is_error: false,
            }),
            "session",
        )
        .expect("patch rendering");

    assert_eq!(block.format, FrontendBlockFormat::UnifiedDiff);
    assert_eq!(block.update, crate::protocol::FrontendBlockUpdate::Replace);
    assert_eq!(block.text, diff);
}

#[test]
fn coding_renderer_groups_read_lifecycle() {
    let tools = Tools::coding();
    let begin = tools
        .render(
            &EventMsg::ToolCallBegin(crate::protocol::ToolCallBeginEvent {
                turn_id: "turn".into(),
                call_id: "call".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "note.txt"}),
            }),
            "session",
        )
        .expect("read begin rendering");
    let end = tools
        .render(
            &EventMsg::ToolCallEnd(crate::protocol::ToolCallEndEvent {
                turn_id: "turn".into(),
                call_id: "call".into(),
                name: "read_file".into(),
                output: "contents".into(),
                is_error: false,
            }),
            "session",
        )
        .expect("read end rendering");

    assert_eq!(begin.group.as_deref(), Some("read:turn"));
    assert_eq!(end.group.as_deref(), Some("read:turn"));
    let mut text = begin.text;
    end.update.apply(&mut text, &end.text);
    assert_eq!(text, "note.txt\ncontents");
}

#[test]
fn tool_blocks_format_json_before_appending_and_preserve_plain_text_whitespace() {
    assert_eq!(formatted_tool_text(r#"{"a":1}"#), "{\n  \"a\": 1\n}");
    assert_eq!(
        formatted_tool_text("\n  file contents\n"),
        "\n  file contents\n"
    );
    let mut body = "input\n".to_owned();
    crate::protocol::FrontendBlockUpdate::Append.apply(&mut body, "  output\n");
    crate::protocol::FrontendBlockUpdate::Append.apply(&mut body, "");
    assert_eq!(body, "input\n  output\n");
    let json = serde_json::json!({"a": 1, "b": 2, "c": 3, "d": 4});
    let end = Tools::coding()
        .render(
            &EventMsg::ToolCallEnd(crate::protocol::ToolCallEndEvent {
                turn_id: "turn".into(),
                call_id: "call".into(),
                name: "read_file".into(),
                output: json.to_string(),
                is_error: false,
            }),
            "session",
        )
        .expect("end rendering");
    assert_eq!(serde_json::from_str::<Value>(&end.text).unwrap(), json);
}

#[test]
fn generic_tool_renderer_does_not_infer_coding_presentation() {
    let diff = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let block = render_tool_event(
        &EventMsg::ToolCallEnd(crate::protocol::ToolCallEndEvent {
            turn_id: "turn".into(),
            call_id: "call".into(),
            name: "example_tool".into(),
            output: diff.into(),
            is_error: false,
        }),
        |name| name == "example_tool",
        |_, _| "Owned completion".into(),
    )
    .expect("generic rendering");

    assert_eq!(block.format, FrontendBlockFormat::PlainText);
    assert_eq!(block.group, None);
    assert_eq!(block.update, crate::protocol::FrontendBlockUpdate::Append);
    assert_eq!(block.title, "Owned completion");
}

#[test]
fn tool_load_uses_the_standard_tool_presentation() {
    let block = Tools::coding()
        .render(
            &EventMsg::ToolLoad(crate::protocol::ToolLoadEvent {
                turn_id: "turn".into(),
                load_id: "step".into(),
                catalog_revision: "catalog".into(),
                tools: vec!["swarm_post".into(), "swarm_read".into()],
            }),
            "session",
        )
        .expect("tool load rendering");

    assert_eq!(
        (
            block.id.as_deref(),
            block.state,
            block.role,
            block.title.as_str(),
            block.text.as_str(),
            block.tone,
        ),
        (
            Some("turn/step/load"),
            FrontendBlockState::Complete,
            FrontendBlockRole::Tool,
            "Loaded tools",
            "swarm_post\nswarm_read",
            FrontendTone::Success,
        )
    );
}
