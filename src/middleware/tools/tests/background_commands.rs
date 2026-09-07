use super::*;

#[test]
fn only_starting_a_background_command_requires_approval() {
    let mut catalog = Catalog::default();
    catalog
        .register(Arc::new(StartCommand))
        .expect("start command");
    catalog
        .register(Arc::new(PollCommand))
        .expect("poll command");
    catalog
        .register(Arc::new(StopCommand))
        .expect("stop command");

    assert!(catalog.requires_approval("start_command"));
    assert!(!catalog.requires_approval("poll_command"));
    assert!(!catalog.requires_approval("stop_command"));
}

#[test]
fn background_output_remains_valid_json_at_its_limit() {
    let rendered = background_output(BackgroundCommandPoll {
        status: crate::backend::sandbox::BackgroundCommandStatus::Running,
        exit_code: None,
        stdout: "\0".repeat(6_000),
        stderr: String::new(),
        truncated: false,
        error: Some("\0".repeat(512)),
    });

    assert!(rendered.len() <= MAX_TOOL_OUTPUT_BYTES);
    let value: Value = serde_json::from_str(&rendered).expect("valid JSON");
    assert_eq!(value["status"], "running");
}
