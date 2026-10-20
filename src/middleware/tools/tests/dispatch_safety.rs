use super::*;

struct ApprovalRequiredTool;

impl Tool for ApprovalRequiredTool {
    fn definition(&self) -> &std::sync::Arc<ToolDefinition> {
        static DEFINITION: std::sync::LazyLock<std::sync::Arc<ToolDefinition>> =
            std::sync::LazyLock::new(|| {
                std::sync::Arc::new(ToolDefinition {
                    name: "approval_required".into(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                })
            });
        &DEFINITION
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call(
        &self,
        _context: ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async { Ok("executed".into()) })
    }
}

struct PermissionEchoTool;

impl Tool for PermissionEchoTool {
    fn definition(&self) -> &std::sync::Arc<ToolDefinition> {
        static DEFINITION: std::sync::LazyLock<std::sync::Arc<ToolDefinition>> =
            std::sync::LazyLock::new(|| {
                std::sync::Arc::new(ToolDefinition {
                    name: "permission_echo".into(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                })
            });
        &DEFINITION
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        let label = arguments["label"]
            .as_str()
            .expect("permission echo label")
            .to_owned();
        Box::pin(
            async move { Ok(format!("{label}:{}", context.permissions.allows_mutation()).into()) },
        )
    }
}

#[tokio::test]
async fn each_call_receives_its_own_permissions() {
    let mut catalog = Catalog::default();
    catalog
        .register(Arc::new(PermissionEchoTool))
        .expect("register permission echo tool");
    let calls = [
        ToolCall {
            call_id: "denied".into(),
            name: "permission_echo".into(),
            arguments: serde_json::json!({"label": "first"}),
        },
        ToolCall {
            call_id: "allowed".into(),
            name: "permission_echo".into(),
            arguments: serde_json::json!({"label": "second"}),
        },
    ];
    let calls = finalize_and_bind(&mut catalog, &calls);

    assert_eq!(
        execute_batch(
            &catalog,
            calls,
            test_sandbox(),
            &test_permissions(&["allowed"]),
            &"turn".into(),
            &"test".into(),
            &crate::protocol::MessageAuthor::User.into()
        )
        .await,
        vec![
            ToolResult {
                call_id: "denied".into(),
                name: "permission_echo".into(),
                output: "first:false".into(),
                is_error: false,
                usage: None,
                handler_executed: true,
                additional_input: Vec::new(),
                events: Vec::new(),
                output_limit: crate::backend::sandbox::default_tool_output_limit(),
            },
            ToolResult {
                call_id: "allowed".into(),
                name: "permission_echo".into(),
                output: "second:true".into(),
                is_error: false,
                usage: None,
                handler_executed: true,
                additional_input: Vec::new(),
                events: Vec::new(),
                output_limit: crate::backend::sandbox::default_tool_output_limit(),
            },
        ]
    );
}

#[tokio::test]
async fn approval_required_handler_cannot_run_without_exact_call_authority() {
    let mut catalog = Catalog::default();
    catalog
        .register(Arc::new(ApprovalRequiredTool))
        .expect("register tool");
    let calls = [ToolCall {
        call_id: "blocked".into(),
        name: "approval_required".into(),
        arguments: serde_json::json!({}),
    }];
    let calls = finalize_and_bind(&mut catalog, &calls);
    let sandbox = Arc::new(crate::backend::sandbox::Sandbox::new(
        Arc::new(crate::backend::sandbox::local::LocalSandbox::new(".").expect("sandbox")),
        crate::backend::sandbox::ApprovalPolicy::Ask,
    ));
    let permissions = SandboxPermissions::restore(
        &"session".parse().expect("session ID"),
        crate::backend::sandbox::SandboxMode::WorkspaceWrite,
        crate::backend::sandbox::NetworkAccess::Allowed,
        ["different-call".into()],
    );

    let result = execute_batch(
        &catalog,
        calls,
        sandbox,
        &permissions,
        &"turn".into(),
        &"test".into(),
        &crate::protocol::MessageAuthor::User.into(),
    )
    .await
    .pop()
    .expect("tool result");

    assert_eq!(
        (
            result.is_error,
            result.handler_executed,
            result.output.text().as_str()
        ),
        (true, false, "tool call is not authorized to mutate state")
    );
}
