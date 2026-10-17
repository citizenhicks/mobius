use super::*;

struct InterruptibleTool {
    name: &'static str,
    interruptible: bool,
}

impl Tool for InterruptibleTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.into(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    fn cancel_on_input(&self) -> bool {
        self.interruptible
    }

    fn call<'a>(
        &'a self,
        _context: ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async { Ok(String::new().into()) })
    }
}

#[test]
fn only_wholly_cancellable_batches_stop_for_new_input() {
    let mut catalog = Catalog::default();
    for (name, interruptible) in [("wait", true), ("write", false)] {
        catalog
            .register(Arc::new(InterruptibleTool {
                name,
                interruptible,
            }))
            .expect("register tool");
    }
    let call = |name: &str| ToolCall {
        call_id: name.into(),
        name: name.into(),
        arguments: serde_json::json!({}),
    };

    assert!(catalog.cancels_on_input(&[call("wait")]));
    assert!(!catalog.cancels_on_input(&[call("wait"), call("write"),]));
    assert!(!catalog.cancels_on_input(&[]));
}

#[test]
fn only_inspection_tools_declare_read_only() {
    let files = tempfile::tempdir().expect("files");
    let mut catalog = Catalog::default();
    for tool in [
        Arc::new(ReadFile {
            store: crate::backend::session_files::SessionFileStore::new(files.path(), None),
        }) as Arc<dyn Tool>,
        Arc::new(WriteFile),
        Arc::new(ApplyPatch),
        Arc::new(Bash),
        Arc::new(ManageCommand),
    ] {
        catalog.register(tool).expect("register tool");
    }
    catalog.register_search().expect("register tools_search");

    let read_only = ["read_file", TOOLS_SEARCH_NAME];
    for name in read_only {
        assert!(catalog.is_read_only(name), "{name}");
    }
    for name in [
        "write_file",
        "apply_patch",
        "bash",
        "manage_command",
        "unknown",
    ] {
        assert!(!catalog.is_read_only(name), "{name}");
    }
}
