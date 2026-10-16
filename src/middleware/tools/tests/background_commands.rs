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
async fn bash_caps_output_and_restricts_poll_and_stop_to_its_owner() {
    let sandbox = Arc::new(
        Sandbox::new(
            Arc::new(crate::backend::sandbox::local::LocalSandbox::new(".").expect("sandbox")),
            crate::backend::sandbox::ApprovalPolicy::Ask,
        )
        .tool_output_limit(512)
        .expect("output limit"),
    );
    let response = Bash
        .call(
            command_context(&sandbox, "owner", true),
            serde_json::json!({"command": "printf head; printf error-head >&2; i=0; while [ $i -lt 500 ]; do printf '\\001'; printf '\\001' >&2; i=$((i+1)); done; printf tail; printf error-tail >&2; sleep 60"}),
        )
        .await
        .expect("bash");
    let text = response.content.text();
    assert!(text.len() <= 512, "{} bytes", text.len());
    let output: Value = serde_json::from_str(&text).expect("complete JSON after escaping");
    assert_eq!(output["status"], "running");
    assert_eq!(output["truncated"], true);
    for (stream, head, tail) in [
        ("stdout", "head", "tail"),
        ("stderr", "error-head", "error-tail"),
    ] {
        let text = output[stream].as_str().expect("stream text");
        assert!(text.starts_with(head) && text.ends_with(tail), "{text:?}");
    }
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
