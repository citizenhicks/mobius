//! Agent-published session files and their frontend presentation.

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::tools::{ApprovalRequirement, Catalog, Tool, ToolContext};
use super::{Middleware, PromptSection, RuntimeContext};
use crate::backend::model::ToolDefinition;
use crate::backend::sandbox::MAX_BINARY_FILE_BYTES;
use crate::backend::session_files::{SessionFileStore, media_type};
use crate::protocol::{EventMsg, FrontendBlock, FrontendContribution};
use crate::{BoxFuture, Error, Result};

mod text {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) send_artifact: crate::middleware::tools::ToolSpec,
        pub(super) default_enabled: bool,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
        pub(super) prompt_main: String,
        pub(super) render_sent_prefix: String,
        pub(super) error_path_not_file: String,
        pub(super) error_path_or_file_id: String,
    }
    crate::embedded_config! { pub(super) static DEFINITION: Definition = include_str!("artifacts.toml"); }
}
super::manifest::middleware_manifest! {
/// Configuration metadata for agent-published files.
    "artifacts", text::DEFINITION, required: false, settings: &[]
}

/// Publishes workspace files as session-bound assistant files.
pub struct Artifacts {
    store: Arc<SessionFileStore>,
}

impl Artifacts {
    #[must_use]
    /// Creates a new instance.
    pub fn new(store: SessionFileStore) -> Self {
        Self {
            store: Arc::new(store),
        }
    }
}

impl Middleware for Artifacts {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        catalog.register(Arc::new(SendArtifact {
            store: Arc::clone(&self.store),
            session_id: runtime.session_id.clone(),
        }))
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(Some(PromptSection::new(
            text::DEFINITION.prompt_main.as_str(),
        )))
    }

    fn frontend(&self, _session_id: &str) -> FrontendContribution {
        FrontendContribution {
            capability: MANIFEST.id.into(),
            ..FrontendContribution::default()
        }
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        let mut block = text::DEFINITION.send_artifact.render(event)?;
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
    store: Arc<SessionFileStore>,
    session_id: String,
}

impl Tool for SendArtifact {
    fn definition(&self) -> ToolDefinition {
        text::DEFINITION.send_artifact.tool.clone()
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
                        .ok_or_else(|| {
                            Error::Tool(text::DEFINITION.error_path_not_file.as_str().into())
                        })?
                        .to_string();
                    let bytes = context
                        .sandbox
                        .read_bytes(&path, MAX_BINARY_FILE_BYTES, &context.permissions)
                        .await?;
                    let media_type = media_type(&name).into();
                    self.store
                        .publish_artifact(&self.session_id, name, media_type, &bytes)
                        .await?
                }
                (None, Some(id)) => {
                    let file = self.store.file_reference(&self.session_id, &id).await?;
                    self.store
                        .publish_reference(&self.session_id, &file)
                        .await?
                }
                _ => {
                    return Err(Error::Tool(
                        text::DEFINITION.error_path_or_file_id.as_str().into(),
                    ));
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sandbox::local::LocalSandbox;
    use crate::backend::sandbox::{ApprovalPolicy, NetworkAccess, Sandbox, SandboxPermissions};

    #[tokio::test]
    async fn send_artifact_publishes_binary_outside_the_upload_list() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("report.xlsx"), [0, 255, 1, 254]).expect("file");
        let state = tempfile::tempdir().expect("state");
        let store = SessionFileStore::new(state.path(), None);
        let tool = SendArtifact {
            store: Arc::new(store.clone()),
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
                .list_files(
                    "session-a",
                    &[crate::backend::session_files::SessionFileOrigin::Upload]
                )
                .await
                .map(|files| files.into_iter().map(|(_, file)| file).collect::<Vec<_>>())
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
}
