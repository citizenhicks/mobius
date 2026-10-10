//! Optional computer control over the sandbox's persistent execution channel.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::tools::{ApprovalRequirement, Catalog, Tool, ToolContext, ToolSpec};
use super::{Middleware, PromptSection, RuntimeContext, ToolExposureContext};
use crate::backend::model::ToolDefinition;
use crate::backend::sandbox::ApprovalPolicy;
use crate::backend::sandbox::{MAX_BINARY_FILE_BYTES, NetworkAccess, SandboxMode, WorkerCommand};
use crate::backend::session_files::SessionFileStore;
use crate::protocol::{
    ContentPart, EventMsg, FrontendBlock, FrontendContribution, ImageDetail, ToolContent,
    ToolResponse,
};
use crate::{BoxFuture, Error, Result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    default_enabled: bool,
    manifest_label: String,
    manifest_description: String,
    prompt: String,
    default_timeout_ms: u64,
    computer_control: ToolSpec,
}
crate::embedded_config! { static DEFINITION: Definition = include_str!("computer_control.toml"); }

super::manifest::middleware_manifest! {
/// Optional computer control; deployment supplies the runtime and its documentation.
    "computer_control", DEFINITION, required: false, settings: &[]
}

const MAX_CODE_BYTES: usize = 40_000;
const MAX_TIMEOUT_MS: u64 = 120_000;

/// Reports whether a sandbox approval policy lets computer control act without per-call prompts.
#[must_use]
pub fn supports_approval_policy(policy: ApprovalPolicy) -> bool {
    !matches!(policy, ApprovalPolicy::Ask | ApprovalPolicy::AllowNetwork)
}

/// A trusted installed runtime: its worker launch command and the documentation it serves.
pub struct ComputerRuntime {
    worker: WorkerCommand,
    documentation: PathBuf,
}

impl ComputerRuntime {
    /// Bundles an installed worker with its documentation, independent of chat workspaces.
    /// # Errors
    /// Rejects relative executable or documentation paths.
    pub fn new(worker: WorkerCommand, documentation: PathBuf) -> Result<Self> {
        if !worker.executable.is_absolute() || !documentation.is_absolute() {
            return Err(Error::Config(
                "computer runtime and documentation paths must be absolute".into(),
            ));
        }
        Ok(Self {
            worker,
            documentation,
        })
    }
}

/// Owns computer tools and observations; execution and approval remain in the sandbox.
pub struct ComputerControl(Arc<Evaluate>);

impl ComputerControl {
    /// Configures a trusted installed runtime. It is started lazily by an authorized tool call.
    #[must_use]
    pub fn new(files: SessionFileStore, runtime: Arc<ComputerRuntime>) -> Self {
        Self(Arc::new(Evaluate { files, runtime }))
    }
}

impl Middleware for ComputerControl {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn register(&self, catalog: &mut Catalog, _: &RuntimeContext) -> Result<()> {
        catalog.register(Arc::clone(&self.0) as Arc<dyn Tool>)
    }

    fn prompt_section(&self, _: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(Some(PromptSection::new(DEFINITION.prompt.replace(
            "{documentation}",
            &self.0.runtime.documentation.display().to_string(),
        ))))
    }

    fn tool_exposure<'a>(
        &'a self,
        context: &'a mut ToolExposureContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !context.supports_tool_image_input() {
                context.hide(&[DEFINITION.computer_control.tool.name.as_str()]);
            }
            Ok(())
        })
    }

    fn frontend(&self, _session_id: &str) -> FrontendContribution {
        FrontendContribution {
            capability: MANIFEST.id.into(),
            ..Default::default()
        }
    }

    fn render(&self, event: &EventMsg, _: &str) -> Option<FrontendBlock> {
        DEFINITION.computer_control.render(event)
    }
}

struct Evaluate {
    files: SessionFileStore,
    runtime: Arc<ComputerRuntime>,
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

fn default_timeout() -> u64 {
    DEFINITION.default_timeout_ms
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    content: Vec<WorkerPart>,
    is_error: bool,
    #[serde(default, rename = "devtools")]
    _devtools: serde::de::IgnoredAny,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerPart {
    Text { text: String },
    Image { path: String, detail: ImageDetail },
}

impl Tool for Evaluate {
    fn definition(&self) -> ToolDefinition {
        // Tool::definition requires an owned schema to decorate with execution bounds.
        let mut tool = DEFINITION.computer_control.tool.clone();
        let properties = &mut tool.parameters["properties"];
        properties["code"]["maxLength"] = MAX_CODE_BYTES.into();
        properties["timeout_ms"]["minimum"] = 1.into();
        properties["timeout_ms"]["maximum"] = MAX_TIMEOUT_MS.into();
        tool
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
            if args.code.len() > MAX_CODE_BYTES || !(1..=MAX_TIMEOUT_MS).contains(&args.timeout_ms)
            {
                return Err(Error::Tool(
                    "computer code or timeout exceeds its limit".into(),
                ));
            }
            let desktop = if context.permissions.sandbox_mode() == SandboxMode::DangerFullAccess
                && context.permissions.network_access() == NetworkAccess::Allowed
            {
                context
                    .sandbox
                    .desktop_browser_page(&context.permissions)
                    .await?
            } else {
                None
            };
            let request =
                serde_json::to_vec(&serde_json::json!({"code": args.code, "desktop": desktop}))?;
            let output = context
                .sandbox
                .evaluate_worker(
                    &self.runtime.worker,
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
                                .read_bytes(&path, MAX_BINARY_FILE_BYTES, &context.permissions)
                                .await?;
                            self.files
                                .ingest_screenshot(
                                    context.permissions.session_id(),
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

    fn runtime(worker: WorkerCommand) -> Arc<ComputerRuntime> {
        Arc::new(
            ComputerRuntime::new(worker, "/unused/computer-control.md".into())
                .expect("absolute runtime paths"),
        )
    }

    #[test]
    fn installed_runtime_cannot_resolve_from_the_chat_workspace() {
        for (executable, documentation) in [
            ("node", "/installed/computer-control.md"),
            ("/installed/node", "computer-control.md"),
        ] {
            assert!(
                ComputerRuntime::new(
                    WorkerCommand {
                        executable: executable.into(),
                        arguments: Vec::new()
                    },
                    documentation.into(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn schema_bounds_and_heading_come_from_the_owner() {
        let workspace = tempfile::tempdir().expect("workspace");
        let control = ComputerControl::new(
            SessionFileStore::new(workspace.path(), None),
            runtime(WorkerCommand {
                executable: "/unused".into(),
                arguments: Vec::new(),
            }),
        );
        let schema = control.0.definition().parameters;
        assert_eq!(schema["properties"]["code"]["maxLength"], 40_000);
        assert_eq!(schema["properties"]["timeout_ms"]["minimum"], 1);
        assert_eq!(schema["properties"]["timeout_ms"]["maximum"], 120_000);
        let block = control
            .render(
                &EventMsg::ToolCallBegin(crate::protocol::ToolCallBeginEvent {
                    turn_id: "turn".into(),
                    call_id: "call".into(),
                    name: "computer_control".into(),
                    arguments: serde_json::json!({"code": "await page()"}),
                }),
                "session",
            )
            .expect("computer block");
        assert_eq!(
            (block.title.as_str(), block.text.as_str()),
            ("Computer", "")
        );
    }

    #[tokio::test]
    async fn failed_or_cancelled_evaluation_still_fails() {
        for timeout_ms in [1, 10_000] {
            let workspace = tempfile::tempdir().expect("workspace");
            let tool = Evaluate {
                files: SessionFileStore::new(workspace.path(), None),
                runtime: runtime(WorkerCommand {
                    executable: "/usr/bin/python3".into(),
                    arguments: vec!["-c".into(), "import time; time.sleep(30)".into()],
                }),
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
            let result = tokio::time::timeout(
                Duration::from_millis(100),
                tool.call(
                    ToolContext::new(sandbox, permissions, "turn"),
                    serde_json::json!({"code":"unused", "timeout_ms":timeout_ms}),
                ),
            )
            .await;
            if timeout_ms == 1 {
                assert!(matches!(result, Ok(Err(_))), "worker deadline failed");
            } else {
                assert!(result.is_err(), "evaluation was cancelled");
            }
        }
    }

    #[tokio::test]
    async fn browser_selection_requires_full_access_network_and_gateway_availability() {
        let state = tempfile::tempdir().expect("state");
        // Echoes the evaluation request back as the observation's text.
        let echo = "import sys,struct,json; n=struct.unpack('>I',sys.stdin.buffer.read(4))[0]; request=sys.stdin.buffer.read(n).decode(); data=json.dumps({'content':[{'type':'text','text':request}],'is_error':False,'devtools':'http://127.0.0.1:9222'}).encode(); sys.stdout.buffer.write(struct.pack('>I',len(data))+data); sys.stdout.buffer.flush()";
        let tool = Evaluate {
            files: SessionFileStore::new(state.path(), None),
            runtime: runtime(WorkerCommand {
                executable: "/usr/bin/python3".into(),
                arguments: vec!["-c".into(), echo.into()],
            }),
        };
        use crate::backend::sandbox::{
            CommandMode, CommandOutput, CommandOutputSink, DesktopBrowserPage, SandboxBackend,
            WorkerProcess,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct AvailableBrowser {
            requested: Arc<AtomicUsize>,
            available: bool,
        }
        impl SandboxBackend for AvailableBrowser {
            fn desktop_browser_page<'a>(
                &'a self,
                _: &'a str,
            ) -> BoxFuture<'a, Result<Option<DesktopBrowserPage>>> {
                self.requested.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Ok(self.available.then(|| DesktopBrowserPage {
                        endpoint: "http://127.0.0.1:9222".into(),
                        target_id: Some("assigned".into()),
                    }))
                })
            }
            fn start_worker(
                &self,
                command: &WorkerCommand,
                _: SandboxMode,
                _: NetworkAccess,
            ) -> Result<WorkerProcess> {
                WorkerProcess::spawn(
                    tokio::process::Command::new(&command.executable).args(&command.arguments),
                )
            }
            fn read<'a>(&'a self, _: &'a str, _: SandboxMode) -> BoxFuture<'a, Result<String>> {
                Box::pin(async { unreachable!() })
            }
            fn read_bytes<'a>(
                &'a self,
                _: &'a str,
                _: usize,
                _: SandboxMode,
            ) -> BoxFuture<'a, Result<Vec<u8>>> {
                Box::pin(async { unreachable!() })
            }
            fn write<'a>(
                &'a self,
                _: &'a str,
                _: &'a str,
                _: SandboxMode,
            ) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { unreachable!() })
            }
            fn execute<'a>(
                &'a self,
                _: &'a str,
                _: SandboxMode,
                _: NetworkAccess,
                _: CommandMode,
                _: CommandOutputSink,
            ) -> BoxFuture<'a, Result<CommandOutput>> {
                Box::pin(async { unreachable!() })
            }
        }
        let requested = Arc::new(AtomicUsize::new(0));
        for available in [false, true] {
            let sandbox = Arc::new(Sandbox::new(
                Arc::new(AvailableBrowser {
                    requested: requested.clone(),
                    available,
                }),
                ApprovalPolicy::Ask,
            ));
            for mode in [SandboxMode::WorkspaceWrite, SandboxMode::DangerFullAccess] {
                for network in [NetworkAccess::Denied, NetworkAccess::Allowed] {
                    let permissions =
                        SandboxPermissions::restore("session", mode, network, ["call".into()])
                            .for_call("call");
                    let before = requested.load(Ordering::SeqCst);
                    let result = tool
                        .call(
                            ToolContext::new(sandbox.clone(), permissions, "turn"),
                            serde_json::json!({"code":"await getPage()", "reset":true}),
                        )
                        .await
                        .expect("echoed request");
                    let request: Value =
                        serde_json::from_str(&result.content.text()).expect("request JSON");
                    assert_eq!(request["code"], "await getPage()");
                    let eligible =
                        mode == SandboxMode::DangerFullAccess && network == NetworkAccess::Allowed;
                    assert_eq!(
                        requested.load(Ordering::SeqCst) - before,
                        usize::from(eligible)
                    );
                    assert_eq!(!request["desktop"].is_null(), eligible && available);
                    if eligible && available {
                        assert_eq!(request["desktop"]["target_id"], "assigned");
                    }
                }
            }
            sandbox
                .session_end("session")
                .await
                .expect("worker cleanup");
        }
    }

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
            files: SessionFileStore::new(state.path(), None),
            runtime: runtime(WorkerCommand {
                executable: "/usr/bin/python3".into(),
                arguments: vec!["-c".into(),
                    "import sys,struct; n=struct.unpack('>I',sys.stdin.buffer.read(4))[0]; sys.stdin.buffer.read(n); data=sys.argv[1].encode(); sys.stdout.buffer.write(struct.pack('>I',len(data))+data); sys.stdout.buffer.flush()".into(),
                    output.to_string()],
            }),
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
