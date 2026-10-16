use super::*;

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
