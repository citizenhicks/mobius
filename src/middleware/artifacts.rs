//! Agent-published session files and their frontend presentation.

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::manifest::MiddlewareManifest;
use super::tools::{
    ApprovalRequirement, Catalog, Tool, ToolContext, labeled_tool_heading, render_tool_event,
};
use super::{Middleware, PromptSection, RuntimeContext};
use crate::backend::model::ToolDefinition;
use crate::backend::sandbox::MAX_BINARY_FILE_BYTES;
use crate::backend::session_files::SessionFileStore;
use crate::protocol::{EventMsg, FrontendBlock, FrontendContribution};
use crate::{BoxFuture, Error, Result};

mod text {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) default_enabled: bool,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
        pub(super) prompt_main: String,
        pub(super) render_send: String,
        pub(super) render_sent_prefix: String,
        pub(super) tool_send_artifact_description: String,
        pub(super) tool_send_artifact_parameter_path_description: String,
    }
    pub(super) static DEFINITION: std::sync::LazyLock<Definition> =
        std::sync::LazyLock::new(|| {
            toml::from_str(include_str!("artifacts.toml"))
                .expect("bundled artifacts definition must be valid")
        });
}
/// Configuration metadata for agent-published files.
pub static MANIFEST: std::sync::LazyLock<MiddlewareManifest> =
    std::sync::LazyLock::new(|| MiddlewareManifest {
        id: "artifacts",
        label: text::DEFINITION.manifest_label.as_str(),
        description: text::DEFINITION.manifest_description.as_str(),
        required: false,
        default_enabled: text::DEFINITION.default_enabled,
        required_model_capability: None,
        settings: &[],
    });

/// Publishes workspace files as session-bound assistant files.
pub struct Artifacts {
    store: SessionFileStore,
}

impl Artifacts {
    #[must_use]
    /// Creates a new instance.
    pub fn new(store: SessionFileStore) -> Self {
        Self { store }
    }
}

impl Middleware for Artifacts {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        catalog.register(Arc::new(SendArtifact {
            store: self.store.clone(),
            session_id: runtime.session_id.clone(),
        }))
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(Some(PromptSection::new(
            text::DEFINITION.prompt_main.as_str(),
        )))
    }

    fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: MANIFEST.id.into(),
            ..FrontendContribution::default()
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        let mut block = render_tool_event(
            event,
            |name| name == "send_artifact",
            |name, arguments| {
                if matches!(event, EventMsg::ToolCallEnd(_)) {
                    name.into()
                } else {
                    labeled_tool_heading(text::DEFINITION.render_send.as_str(), "path", arguments)
                }
            },
        )?;
        let EventMsg::ToolCallEnd(result) = event else {
            return Some(block);
        };
        if result.is_error {
            return Some(block);
        }
        let Some(file) = result.output.files().next().cloned() else {
            return Some(block);
        };
        block.update = crate::protocol::FrontendBlockUpdate::Replace;
        block.state = crate::protocol::FrontendBlockState::Complete;
        block.role = crate::protocol::FrontendBlockRole::Artifact;
        block.title = format!(
            "{}{}",
            text::DEFINITION.render_sent_prefix.as_str(),
            file.name
        );
        block.text.clear();
        block.content = Default::default();
        block.files = vec![file];
        Some(block)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArtifactArgs {
    path: Option<String>,
    file_id: Option<String>,
}

struct SendArtifact {
    store: SessionFileStore,
    session_id: String,
}

impl Tool for SendArtifact {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "send_artifact".into(),
            description: text::DEFINITION.tool_send_artifact_description.clone(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_id": {"type":"string", "description":"Authorized stored file ID to publish without copying bytes."},
                    "path": {
                        "type": "string",
                        "description": text::DEFINITION.tool_send_artifact_parameter_path_description.as_str()
                    }
                },
                "oneOf": [{"required":["path"]},{"required":["file_id"]}],
                "additionalProperties": false
            }),
        }
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call<'a>(
        &'a self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: SendArtifactArgs = serde_json::from_value(arguments)?;
            let file = match (arguments.path, arguments.file_id) {
                (Some(path), None) => {
                    let name = Path::new(&path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or_else(|| Error::Tool("artifact path must name one file".into()))?
                        .to_string();
                    let bytes = context
                        .sandbox
                        .read_bytes(&path, MAX_BINARY_FILE_BYTES, &context.permissions)
                        .await?;
                    self.store
                        .publish_artifact(
                            &self.session_id,
                            name.clone(),
                            media_type(&name).into(),
                            &bytes,
                        )
                        .await?
                }
                (None, Some(id)) => {
                    let file = self.store.file_reference(&self.session_id, &id).await?;
                    self.store
                        .publish_reference(&self.session_id, &file)
                        .await?
                }
                _ => return Err(Error::Tool("provide exactly one of path or file_id".into())),
            };
            Ok(crate::protocol::ToolResponse {
                content: crate::protocol::ToolContent(vec![crate::protocol::ContentPart::File {
                    file,
                }]),
                is_error: false,
            })
        })
    }
}

fn media_type(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("csv") => "text/csv",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SessionFileReference;
    use std::collections::BTreeMap;

    use crate::backend::checkpoint::{CheckpointStore, sqlite::SqliteCheckpoint};
    use crate::backend::sandbox::local::LocalSandbox;
    use crate::backend::sandbox::{ApprovalPolicy, NetworkAccess, Sandbox, SandboxPermissions};
    use crate::protocol::{SessionContext, ToolCallEndEvent};

    #[test]
    fn one_middleware_registers_tools_for_multiple_sessions() {
        let state = tempfile::tempdir().expect("state");
        let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
            SqliteCheckpoint::new(state.path().join("checkpoints.sqlite3"))
                .expect("checkpoint store"),
        );
        let middleware = Artifacts::new(SessionFileStore::new(state.path()));
        let runtime = RuntimeContext {
            sender: crate::agent::test_sender(),
            checkpoints,
            session_id: "session-a".into(),
            model_route: "model".into(),
            model: "model".into(),
            approval_policy: crate::backend::sandbox::ApprovalPolicy::Ask,
            session_context: SessionContext::default(),
            metadata: BTreeMap::new(),
            role: crate::agent::AgentRole::Main,
            frontend: Arc::new(|_| Ok(())),
        };
        let mut second_runtime = runtime.clone();
        second_runtime.session_id = "session-b".into();

        let mut first = Catalog::default();
        middleware
            .register(&mut first, &runtime)
            .expect("first session catalog");
        let mut second = Catalog::default();
        middleware
            .register(&mut second, &second_runtime)
            .expect("second session catalog");

        assert_eq!(
            first.registered_definitions(),
            second.registered_definitions()
        );
    }

    #[tokio::test]
    async fn send_artifact_publishes_binary_outside_the_upload_list() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("report.xlsx"), [0, 255, 1, 254]).expect("file");
        let state = tempfile::tempdir().expect("state");
        let store = SessionFileStore::new(state.path());
        let tool = SendArtifact {
            store: store.clone(),
            session_id: "session-a".into(),
        };
        let sandbox = Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("local sandbox")),
            ApprovalPolicy::Ask,
        ));
        let permissions = SandboxPermissions::restore(
            "session-a",
            crate::backend::sandbox::SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            ["call".into()],
        )
        .for_call("call");

        let output = tool
            .call(
                ToolContext::new(sandbox, permissions, "turn"),
                serde_json::json!({"path": "report.xlsx"}),
            )
            .await
            .expect("send artifact");
        let file = output.content.files().next().expect("published file");

        assert_eq!(tool.approval(), ApprovalRequirement::Always);
        assert!(
            store
                .list_uploads("session-a")
                .await
                .expect("uploads")
                .is_empty()
        );
        assert_eq!(
            store
                .read_chunk("session-a", &file.id, 0, 16)
                .await
                .expect("read")
                .data,
            [0, 255, 1, 254]
        );
    }

    #[test]
    fn successful_send_replaces_the_pending_block_with_a_file() {
        let state = tempfile::tempdir().expect("state");
        let middleware = Artifacts::new(SessionFileStore::new(state.path()));
        let file = SessionFileReference {
            id: uuid::Uuid::new_v4().to_string(),
            name: "diagram.svg".into(),
            size: 4,
            media_type: "image/svg+xml".into(),
        };
        let output = crate::protocol::ToolContent(vec![crate::protocol::ContentPart::File {
            file: file.clone(),
        }]);

        let block = middleware
            .render(
                &EventMsg::ToolCallEnd(ToolCallEndEvent {
                    turn_id: "turn".into(),
                    call_id: "call".into(),
                    name: "send_artifact".into(),
                    output,
                    is_error: false,
                }),
                "session-a",
            )
            .expect("block");

        assert_eq!(block.update, crate::protocol::FrontendBlockUpdate::Replace);
        assert_eq!(block.title, "Sent diagram.svg");
        assert!(block.text.is_empty());
        assert_eq!(block.files, [file]);
    }
}
