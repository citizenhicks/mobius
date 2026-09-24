use super::*;

#[test]
fn only_launching_a_command_requires_approval() {
    let mut catalog = Catalog::default();
    catalog.register(Arc::new(Bash)).expect("bash");
    catalog
        .register(Arc::new(ManageCommand))
        .expect("manage command");

    assert!(catalog.requires_approval("bash"));
    assert!(!catalog.requires_approval("manage_command"));
}

#[test]
fn background_output_remains_valid_json_at_its_limit() {
    let rendered = background_output(BackgroundCommandPoll {
        command_id: Some(uuid::Uuid::nil().to_string()),
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
    assert_eq!(value["command_id"], uuid::Uuid::nil().to_string());
}

fn command_context(sandbox: &Arc<Sandbox>, owner: &str, authorized: bool) -> ToolContext {
    ToolContext::new(
        Arc::clone(sandbox),
        SandboxPermissions::restore(
            owner,
            crate::backend::sandbox::SandboxMode::WorkspaceWrite,
            crate::backend::sandbox::NetworkAccess::Denied,
            authorized.then(|| "command".into()),
        )
        .for_call("command"),
        "turn",
    )
}

#[tokio::test]
async fn bash_returns_completed_output_and_rejects_unauthorized_launches() {
    let sandbox = test_sandbox();
    let command = serde_json::json!({"command": "printf done; printf warning >&2; exit 7"});
    assert!(
        Bash.call(command_context(&sandbox, "owner", false), command.clone())
            .await
            .is_err()
    );
    assert!(
        !sandbox
            .has_background_commands("owner")
            .expect("no unauthorized command")
    );

    let response = Bash
        .call(command_context(&sandbox, "owner", true), command)
        .await
        .expect("bash");
    let output: Value = serde_json::from_str(&response.content.text()).expect("command output");
    assert_eq!(output["status"], "exited");
    assert_eq!(output["stdout"], "done");
    assert_eq!(output["stderr"], "warning");
    assert_eq!(output["exit_code"], 7);
    assert!(output["command_id"].is_null());
    assert!(
        !sandbox
            .has_background_commands("owner")
            .expect("completed command removed")
    );
}

#[tokio::test]
async fn bash_yields_a_command_that_only_its_owner_can_poll_and_stop() {
    let sandbox = test_sandbox();
    let response = Bash
        .call(
            command_context(&sandbox, "owner", true),
            serde_json::json!({"command": "printf first; sleep 60"}),
        )
        .await
        .expect("bash");
    let output: Value = serde_json::from_str(&response.content.text()).expect("running command");
    assert_eq!(output["status"], "running");
    assert_eq!(output["stdout"], "first");
    let id = output["command_id"].as_str().expect("running command ID");

    for action in ["poll", "stop"] {
        assert!(
            ManageCommand
                .call(
                    command_context(&sandbox, "other", false),
                    serde_json::json!({"command_id": id, "action": action}),
                )
                .await
                .is_err()
        );
    }
    assert!(
        ManageCommand
            .call(
                command_context(&sandbox, "owner", false),
                serde_json::json!({"command_id": id, "action": "restart"}),
            )
            .await
            .is_err()
    );

    for (action, expected) in [("poll", "running"), ("stop", "stopped")] {
        let response = ManageCommand
            .call(
                command_context(&sandbox, "owner", false),
                serde_json::json!({"command_id": id, "action": action}),
            )
            .await
            .expect("manage owned command without new approval");
        let output: Value = serde_json::from_str(&response.content.text()).expect("command output");
        assert_eq!(output["status"], expected);
        assert_eq!(output["stdout"], "");
    }
    assert!(
        !sandbox
            .has_background_commands("owner")
            .expect("stopped command removed")
    );
    assert!(
        ManageCommand
            .call(
                command_context(&sandbox, "owner", false),
                serde_json::json!({"command_id": id, "action": "poll"}),
            )
            .await
            .is_err()
    );
}
