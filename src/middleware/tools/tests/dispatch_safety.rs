use super::*;

struct PanickingTool;

struct ApprovalRequiredTool;

impl Tool for PanickingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "panicking".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        panic!("intentional tool panic")
    }
}

impl Tool for ApprovalRequiredTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "approval_required".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { Ok("executed".into()) })
    }
}

struct PermissionEchoTool;

impl Tool for PermissionEchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "permission_echo".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn call<'a>(&'a self, context: ToolContext, arguments: Value) -> BoxFuture<'a, Result<String>> {
        let label = arguments["label"]
            .as_str()
            .expect("permission echo label")
            .to_string();
        Box::pin(async move { Ok(format!("{label}:{}", context.permissions.allows_mutation())) })
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
            &calls,
            test_sandbox(),
            &test_permissions(&["allowed"]),
            "turn",
        )
        .await,
        vec![
            ToolResult {
                call_id: "denied".into(),
                name: "permission_echo".into(),
                output: "first:false".into(),
                is_error: false,
                handler_executed: true,
                additional_input: Vec::new(),
                events: Vec::new(),
            },
            ToolResult {
                call_id: "allowed".into(),
                name: "permission_echo".into(),
                output: "second:true".into(),
                is_error: false,
                handler_executed: true,
                additional_input: Vec::new(),
                events: Vec::new(),
            },
        ]
    );
}

#[tokio::test]
async fn parallel_tool_panic_preserves_call_identity() {
    let mut catalog = Catalog::default();
    catalog
        .register(Arc::new(PanickingTool))
        .expect("register tool");
    let calls = [ToolCall {
        call_id: "call-1".into(),
        name: "panicking".into(),
        arguments: serde_json::json!({}),
    }];
    let calls = finalize_and_bind(&mut catalog, &calls);
    let backend =
        Arc::new(crate::backend::sandbox::local::LocalSandbox::new(".").expect("local sandbox"));
    let sandbox = Arc::new(crate::backend::sandbox::Sandbox::new(
        backend,
        crate::backend::sandbox::ApprovalPolicy::Ask,
    ));
    let permissions = SandboxPermissions::restore(
        "session",
        crate::backend::sandbox::SandboxMode::WorkspaceWrite,
        crate::backend::sandbox::NetworkAccess::Denied,
        Vec::new(),
    );

    assert_eq!(
        execute_batch(&catalog, &calls, sandbox, &permissions, "turn").await,
        vec![ToolResult {
            call_id: "call-1".into(),
            name: "panicking".into(),
            output: "tool panicked".into(),
            is_error: true,
            handler_executed: true,
            additional_input: Vec::new(),
            events: Vec::new(),
        }]
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
        "session",
        crate::backend::sandbox::SandboxMode::WorkspaceWrite,
        crate::backend::sandbox::NetworkAccess::Allowed,
        ["different-call".into()],
    );

    let result = execute_batch(&catalog, &calls, sandbox, &permissions, "turn")
        .await
        .pop()
        .expect("tool result");

    assert_eq!(
        (
            result.is_error,
            result.handler_executed,
            result.output.as_str()
        ),
        (true, false, "tool call is not authorized to mutate state")
    );
}
