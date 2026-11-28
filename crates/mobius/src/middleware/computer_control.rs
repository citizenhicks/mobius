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
    ContentPart, EventMsg, FrontendBlock, ImageDetail, ToolContent, ToolResponse,
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
    requires: Vec<crate::protocol::FrontendSettingRequirement>,
}
crate::embedded_config! { static DEFINITION: Definition = include_str!("computer_control.toml") => limit_schema; }

super::manifest::middleware_manifest! {
/// Optional computer control; deployment supplies the runtime and its documentation.
    "computer_control", DEFINITION, required: false, settings: &[], requires: &DEFINITION.requires
}

const MAX_CODE_BYTES: usize = 40_000;
const MAX_TIMEOUT_MS: u64 = 120_000;

fn limit_schema(definition: &mut Definition) {
    super::tools::limit_schema(
        &mut definition.computer_control.tool,
        [
            ("/properties/code", "maxLength", MAX_CODE_BYTES.into()),
            ("/properties/timeout_ms", "minimum", 1.into()),
            ("/properties/timeout_ms", "maximum", MAX_TIMEOUT_MS.into()),
        ],
    );
}

/// Reports whether a sandbox approval policy lets computer control act without per-call prompts.
#[must_use]
pub fn supports_approval_policy(policy: ApprovalPolicy) -> bool {
    !matches!(policy, ApprovalPolicy::Ask | ApprovalPolicy::AllowNetwork)
}

/// A trusted installed runtime: its worker launch command and the documentation it serves.
#[derive(Debug)]
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
            return Err(Error::Invalid(
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

impl std::fmt::Debug for ComputerControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ComputerControl")
            .field("runtime", &self.0.runtime)
            .finish_non_exhaustive()
    }
}

impl ComputerControl {
    /// Configures a trusted installed runtime. It is started lazily by an authorized tool call.
    #[must_use]
    pub fn new(files: impl Into<Arc<SessionFileStore>>, runtime: Arc<ComputerRuntime>) -> Self {
        Self(Arc::new(Evaluate {
            files: files.into(),
            runtime,
        }))
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

    fn render(&self, event: &EventMsg, _: &str) -> Option<FrontendBlock> {
        DEFINITION.computer_control.render(event)
    }
}

struct Evaluate {
    files: Arc<SessionFileStore>,
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
    fn definition(&self) -> &Arc<ToolDefinition> {
        &DEFINITION.computer_control.tool
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call(&self, context: ToolContext, arguments: Value) -> BoxFuture<'_, Result<ToolResponse>> {
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
                    context.permissions.mutation()?,
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
            Ok(ToolResponse::Completed {
                content: ToolContent(content),
                is_error,
            })
        })
    }
}

#[cfg(test)]
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod tests;
