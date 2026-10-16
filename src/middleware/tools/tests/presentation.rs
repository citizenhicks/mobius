use super::*;

#[test]
fn observation_capping_preserves_utf8_and_files_within_the_text_budget() {
    use crate::protocol::{ContentPart, SessionFileReference, ToolContent};

    let file = ContentPart::File {
        file: SessionFileReference {
            id: "observation".into(),
            name: "screen.png".into(),
            media_type: "image/png".into(),
            size: 100,
        },
    };
    let output_limit = 60;
    let output = cap_content(
        ToolContent(vec![
            ContentPart::Text {
                text: "é🗣".repeat(20),
            },
            file.clone(),
            ContentPart::Text {
                text: "tail".repeat(20),
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
    let ContentPart::Text { text } = &output.0[0] else {
        panic!("expected text");
    };
    assert!(text.starts_with("é🗣") && text.ends_with("é🗣"), "{text}");
    assert!(text.contains(" of 120 bytes truncated]"), "{text}");
    assert_eq!(capped("🗣é", 1), "");
    assert_eq!(capped("é", 2), "é");
    assert_eq!(output.0[1], file);
    assert_eq!(
        output.0[3],
        ContentPart::Text {
            text: String::new()
        }
    );
}
