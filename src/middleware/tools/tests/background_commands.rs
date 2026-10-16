use super::*;

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
