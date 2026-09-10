//! Optional browser control over the sandbox's persistent execution channel.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::manifest::MiddlewareManifest;
use super::tools::{ApprovalRequirement, Catalog, Tool, ToolContext, render_tool_event};
use super::{
    Middleware, PromptSection, RuntimeContext, SessionStartContext, SessionStartSource,
    ToolExposureContext,
};
use crate::backend::model::{ToolDefinition, internal_user_message};
use crate::backend::sandbox::{MAX_BINARY_FILE_BYTES, WorkerCommand};
use crate::backend::session_files::SessionFileStore;
use crate::protocol::{
    ContentPart, EventMsg, FrontendBlock, FrontendContribution, ImageDetail, ToolContent,
    ToolResponse,
};
use crate::{BoxFuture, Error, Result};

/// Optional browser control; deployment supplies the runtime and its documentation.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "computer_control",
    label: "Computer control",
    description: "Operate a sandbox browser with persistent JavaScript and native image observations",
    required: false,
    default_enabled: false,
    settings: &[],
};

/// Owns browser tools and observations; execution and approval remain in the sandbox.
pub struct ComputerControl {
    files: SessionFileStore,
    worker: WorkerCommand,
    documentation: PathBuf,
}

impl ComputerControl {
    /// Configures a trusted installed runtime. It is started lazily by an authorized tool call.
    pub fn new(
        files: SessionFileStore,
        worker: WorkerCommand,
        documentation: PathBuf,
    ) -> Result<Self> {
        if !worker.executable.is_absolute() || !documentation.is_absolute() {
            return Err(Error::Config(
                "computer runtime and documentation paths must be absolute".into(),
            ));
        }
        Ok(Self {
            files,
            worker,
            documentation,
        })
    }
}

impl Middleware for ComputerControl {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, runtime: &RuntimeContext) -> Result<()> {
        catalog.register(Arc::new(Evaluate {
            files: self.files.clone(),
            worker: self.worker.clone(),
            session_id: runtime.session_id.clone(),
        }))
    }

    fn prompt_section(&self, _: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(Some(PromptSection::new(format!(
            "Computer control is available. Read {} before using computer_control. Browser state and JavaScript variables survive context compaction. Action failures can retain interpreter state; follow the reported state. An overall evaluation deadline, cancellation, or runtime restart loses interpreter state and may leave an action completed; never repeat it automatically. Child agents have independent browser contexts.",
            self.documentation.display()
        ))))
    }

    fn tool_exposure<'a>(
        &'a self,
        context: &'a mut ToolExposureContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !context.supports_tool_image_input() {
                context.hide(&["computer_control"]);
            }
            Ok(())
        })
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if context.source() == SessionStartSource::Resume {
                context.push_input(internal_user_message("computer_runtime", "This session's execution lifetime restarted. The previous computer interpreter and private temporary files are gone. Prior actions may have completed. Start a fresh browser context and inspect before acting; recorded observations remain accessible by file_id."));
            }
            Ok(())
        })
    }

    fn frontend(&self) -> FrontendContribution {
        FrontendContribution {
            capability: MANIFEST.id.into(),
            ..Default::default()
        }
    }

    fn render(&self, event: &EventMsg, _: &str) -> Option<FrontendBlock> {
        render_tool_event(
            event,
            |name| name == "computer_control",
            |_, _| "Computer".into(),
        )
    }
}

struct Evaluate {
    files: SessionFileStore,
    worker: WorkerCommand,
    session_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluateArgs {
    code: String,
    #[serde(default)]
    reset: bool,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

const fn default_timeout() -> u64 {
    30_000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    content: Vec<WorkerPart>,
    is_error: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerPart {
    Text { text: String },
    Image { path: String, detail: ImageDetail },
}

impl Tool for Evaluate {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition { name: "computer_control".into(), description: "Evaluate JavaScript in this session's persistent sandbox browser. Read the installed documentation first. Await every action. Use emitImage(path) for screenshots. Reset explicitly after state loss; never automatically repeat an uncertain action.".into(), parameters: serde_json::json!({
            "type":"object", "properties":{
                "code":{"type":"string", "maxLength":40000},
                "reset":{"type":"boolean", "description":"Discard the interpreter and create a new browser context before this evaluation."},
                "timeout_ms":{"type":"integer", "minimum":1, "maximum":120000}
            }, "required":["code"], "additionalProperties":false
        }) }
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call<'a>(
        &'a self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolResponse>> {
        Box::pin(async move {
            let args: EvaluateArgs = serde_json::from_value(arguments)?;
            if args.code.len() > 40_000 || args.timeout_ms == 0 || args.timeout_ms > 120_000 {
                return Err(Error::Tool(
                    "computer code or timeout exceeds its limit".into(),
                ));
            }
            let request = serde_json::to_vec(&serde_json::json!({"code":args.code}))?;
            let output = context
                .sandbox
                .evaluate_worker(
                    &self.worker,
                    &context.permissions,
                    &request,
                    Duration::from_millis(args.timeout_ms),
                    args.reset,
                )
                .await?;
            let observation: Observation = serde_json::from_slice(&output)?;
            if observation.content.len() > 64
                || observation
                    .content
                    .iter()
                    .filter(|part| matches!(part, WorkerPart::Image { .. }))
                    .count()
                    > 16
            {
                return Err(Error::Tool(
                    "computer observation exceeds capture limits; actions already executed".into(),
                ));
            }
            let mut content = Vec::new();
            let mut is_error = observation.is_error;
            for part in observation.content {
                match part {
                    WorkerPart::Text { text } => content.push(ContentPart::Text { text }),
                    WorkerPart::Image { path, detail } => {
                        let image = async {
                            let bytes = context
                                .sandbox
                                .read_bytes(&path, MAX_BINARY_FILE_BYTES)
                                .await?;
                            self.files
                                .ingest_image(
                                    &self.session_id,
                                    "screenshot.png".into(),
                                    bytes,
                                    detail,
                                )
                                .await
                        }
                        .await;
                        match image {
                            Ok(image) => content.push(ContentPart::Image { image }),
                            Err(error) => {
                                is_error = true;
                                content.push(ContentPart::Text {
                                    text: format!("Image observation unavailable: {error}. Earlier actions may have completed."),
                                });
                            }
                        }
                    }
                }
            }
            Ok(ToolResponse {
                content: ToolContent(content),
                is_error,
            })
        })
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use crate::backend::sandbox::{
        ApprovalPolicy, NetworkAccess, Sandbox, SandboxMode, SandboxPermissions,
        local::LocalSandbox,
    };

    #[tokio::test]
    async fn unreadable_failure_image_preserves_the_original_error() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("state");
        let output = serde_json::json!({
            "content": [
                {"type":"text", "text":"Failure: action_timeout; session state: retained. Original error: missing button"},
                {"type":"image", "path":workspace.path().join("missing.png"), "detail":"auto"}
            ],
            "is_error": true
        });
        let tool = Evaluate {
            files: SessionFileStore::new(state.path()),
            worker: WorkerCommand {
                executable: "/usr/bin/python3".into(),
                arguments: vec!["-c".into(),
                    "import sys,struct; n=struct.unpack('>I',sys.stdin.buffer.read(4))[0]; sys.stdin.buffer.read(n); data=sys.argv[1].encode(); sys.stdout.buffer.write(struct.pack('>I',len(data))+data); sys.stdout.buffer.flush()".into(),
                    output.to_string()],
            },
            session_id: "session".into(),
        };
        let sandbox = Arc::new(Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("sandbox")),
            ApprovalPolicy::Ask,
        ));
        let permissions = SandboxPermissions::restore(
            "session",
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            ["call".into()],
        )
        .for_call("call");
        let result = tool
            .call(
                ToolContext::new(sandbox, permissions, "turn"),
                serde_json::json!({"code":"unused"}),
            )
            .await
            .expect("failed observation still returned");
        assert!(result.is_error);
        assert!(
            result
                .content
                .text()
                .contains("Original error: missing button")
        );
        assert!(
            result
                .content
                .text()
                .contains("Image observation unavailable")
        );
    }
}
