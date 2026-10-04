use super::*;
use crate::protocol::FrontendBlockFormat;

#[test]
fn output_capping_keeps_complete_characters_at_both_ends() {
    assert_eq!(capped(&"é🗣".repeat(20), 27), "é🗣…truncated…é🗣");
    assert_eq!(capped("🗣é", 1), "");
    assert_eq!(capped("é", 2), "é");
}

#[test]
fn observation_capping_bounds_all_text_without_dropping_files() {
    use crate::protocol::{ContentPart, SessionFileReference, ToolContent};

    let file = ContentPart::File {
        file: SessionFileReference {
            id: "observation".into(),
            name: "screen.png".into(),
            media_type: "image/png".into(),
            size: 100,
        },
    };
    let output_limit = crate::backend::sandbox::default_tool_output_limit();
    let output = cap_content(
        ToolContent(vec![
            ContentPart::Text {
                text: "é🗣".repeat(output_limit),
            },
            file.clone(),
            ContentPart::Text {
                text: "tail".repeat(output_limit),
            },
            ContentPart::Text {
                text: "no remaining budget".into(),
            },
        ]),
        output_limit,
    );
    let bytes: usize = output
        .0
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.len()),
            _ => None,
        })
        .sum();
    assert!(bytes <= output_limit);
    assert_eq!(output.0[1], file);
    assert_eq!(
        output.0[3],
        ContentPart::Text {
            text: String::new()
        }
    );
}

#[test]
fn tools_do_not_claim_footer_space() {
    assert!(
        Tools::coding(crate::backend::session_files::SessionFileStore::new(
            tempfile::tempdir().expect("files").path(),
            None
        ))
        .frontend()
        .widgets
        .is_empty()
    );
}

#[test]
fn coding_renderer_preserves_patch_diff_blocks() {
    let diff = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let block = Tools::coding(crate::backend::session_files::SessionFileStore::new(
        tempfile::tempdir().expect("files").path(),
        None,
    ))
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
    let tools = Tools::coding(crate::backend::session_files::SessionFileStore::new(
        tempfile::tempdir().expect("files").path(),
        None,
    ));
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
fn view_image_heading_preserves_requested_sources() {
    let block = Tools::coding(crate::backend::session_files::SessionFileStore::new(
        tempfile::tempdir().expect("files").path(),
        None,
    ))
    .render(
        &EventMsg::ToolCallBegin(crate::protocol::ToolCallBeginEvent {
            turn_id: "turn".into(),
            call_id: "call".into(),
            name: "view_image".into(),
            arguments: serde_json::json!({"images": [
                {"path": "images/page.png"},
                {"file_id": "stored-image"}
            ]}),
        }),
        "session",
    )
    .expect("view image heading");

    assert_eq!(block.title, "View image");
    assert_eq!(block.text, "images/page.png\nstored-image");
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
    let end = Tools::coding(crate::backend::session_files::SessionFileStore::new(
        tempfile::tempdir().expect("files").path(),
        None,
    ))
    .render(
        &EventMsg::ToolCallEnd(crate::protocol::ToolCallEndEvent {
            turn_id: "turn".into(),
            call_id: "call".into(),
            name: "read_file".into(),
            output: json.to_string().into(),
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
    let block = Tools::coding(crate::backend::session_files::SessionFileStore::new(
        tempfile::tempdir().expect("files").path(),
        None,
    ))
    .render(
        &EventMsg::ToolLoad(crate::protocol::ToolLoadEvent {
            turn_id: "turn".into(),
            load_id: "step".into(),
            catalog_revision: "catalog".into(),
            tools: vec!["notebook_post".into(), "notebook_read".into()],
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
            "notebook_post\nnotebook_read",
            FrontendTone::Success,
        )
    );
}

#[test]
fn coding_file_links_come_from_owned_arguments_and_keep_bodies_unchanged() {
    let tools = Tools::coding(crate::backend::session_files::SessionFileStore::new(
        tempfile::tempdir().expect("files").path(),
        None,
    ));
    for (name, title, path) in [
        ("read_file", "Read", "src/test #é.rs"),
        ("write_file", "Write", "/workspace/test #é.rs"),
    ] {
        let block = tools
            .render(
                &EventMsg::ToolCallBegin(crate::protocol::ToolCallBeginEvent {
                    turn_id: "turn".into(),
                    call_id: "call".into(),
                    name: name.into(),
                    arguments: serde_json::json!({"path":path,"content":"unchanged"}),
                }),
                "session",
            )
            .expect("file rendering");
        assert_eq!(block.title, title);
        assert_eq!(block.text, path);
        assert_eq!(block.links.len(), 1);
        assert_eq!(block.links[0].label, "test #é.rs");
        let uri = reqwest::Url::parse("file:///")
            .unwrap()
            .join(&block.links[0].href)
            .unwrap();
        let decoded = uri.to_file_path().unwrap();
        assert_eq!(
            decoded.to_string_lossy(),
            if path.starts_with('/') {
                path.into()
            } else {
                format!("/{path}")
            }
        );
        let mut serialized = serde_json::to_value(&block).unwrap();
        assert_eq!(
            serde_json::from_value::<FrontendBlock>(serialized.clone())
                .unwrap()
                .links,
            block.links
        );
        serialized.as_object_mut().unwrap().remove("links");
        assert!(
            serde_json::from_value::<FrontendBlock>(serialized)
                .unwrap()
                .links
                .is_empty()
        );
    }
    let generic = render_tool_event(
        &EventMsg::ToolCallBegin(crate::protocol::ToolCallBeginEvent {
            turn_id: "turn".into(),
            call_id: "other".into(),
            name: "other_tool".into(),
            arguments: serde_json::json!({"path":"unrelated.txt"}),
        }),
        |_| true,
        |_, _| "Other".into(),
    )
    .unwrap();
    assert!(
        generic.links.is_empty(),
        "generic rendering must not infer file semantics"
    );
}

#[test]
fn configured_output_budget_can_exceed_the_default_and_bounds_hook_replacements() {
    let text = "x".repeat(50_000);
    assert_eq!(cap_content(text.clone().into(), 60_000).text(), text);
    let call = ToolCall {
        call_id: "a".into(),
        name: "read".into(),
        arguments: Value::Null,
    };
    let mut result = ToolResult::error(&call, "initial", 8);
    result.replace("éééééé");
    assert_eq!(result.output.text(), "éééé");
}
