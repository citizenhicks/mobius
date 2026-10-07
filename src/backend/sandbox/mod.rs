//! Sandboxed execution and its approval boundary.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::middleware::Middleware;
use crate::middleware::PromptSection;
use crate::middleware::RuntimeContext;
use crate::middleware::SessionStartContext;
use crate::middleware::SessionStartSource;
use crate::middleware::manifest::MiddlewareManifest;
use crate::middleware::manifest::MiddlewareSettingChoice;
use crate::middleware::manifest::MiddlewareSettingChoices;
use crate::middleware::manifest::MiddlewareSettingManifest;
use crate::protocol::EventMsg;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendContribution;
use crate::protocol::FrontendEvent;
use crate::protocol::FrontendTone;
use crate::protocol::ReviewDecision;
use crate::protocol::ToolCall;

mod approval;
mod background;
pub mod local;
mod process_group;
mod worker;
pub use worker::{WorkerCommand, WorkerProcess};

pub(crate) const MAX_FILE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_BINARY_FILE_BYTES: usize = 50 * 1024 * 1024;

mod text {
    use super::{MAX_BACKGROUND_COMMANDS, MAX_TOOL_OUTPUT_BYTES};
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) prompt_workspace: String,
        pub(super) prompt_attached_folders: String,
        pub(super) prompt_temporary_directory: String,
        pub(super) setting_tool_output_bytes_label: String,
        pub(super) setting_tool_output_bytes_description: String,
        pub(super) setting_background_commands_label: String,
        pub(super) setting_background_commands_description: String,
        pub(super) approval_policy_allow_description: String,
        pub(super) approval_policy_allow_label: String,
        pub(super) approval_policy_allow_network_description: String,
        pub(super) approval_policy_allow_network_label: String,
        pub(super) approval_policy_ask_description: String,
        pub(super) approval_policy_ask_label: String,
        pub(super) approval_policy_full_access_description: String,
        pub(super) approval_policy_full_access_label: String,
        pub(super) defaults_approval_policy: String,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
        pub(super) prompt_linux: String,
        pub(super) prompt_macos: String,
        pub(super) prompt_other: String,
        pub(super) setting_approval_policy_description: String,
        pub(super) setting_approval_policy_label: String,
        pub(super) defaults_tool_output_bytes: usize,
        pub(super) defaults_background_commands: usize,
        pub(super) defaults_command_timeout_seconds: u64,
        pub(super) defaults_procfs_mode: super::ProcfsMode,
        pub(super) defaults_shell: String,
        pub(super) defaults_environment: Vec<String>,
        pub(super) setting_tool_output_bytes_step: i64,
        pub(super) setting_background_commands_step: i64,
    }
    pub(super) static DEFINITION: std::sync::LazyLock<Definition> =
        std::sync::LazyLock::new(|| {
            let definition: Definition = crate::config::embedded(include_str!("sandbox.toml"));
            assert!((1..=MAX_TOOL_OUTPUT_BYTES).contains(&definition.defaults_tool_output_bytes));
            assert!(
                (1..=MAX_BACKGROUND_COMMANDS).contains(&definition.defaults_background_commands)
            );
            assert!(definition.defaults_command_timeout_seconds > 0);
            assert!(definition.setting_tool_output_bytes_step > 0);
            assert!(definition.setting_background_commands_step > 0);
            assert!(std::path::Path::new(&definition.defaults_shell).is_absolute());
            definition
        });
}
static SETTINGS: std::sync::LazyLock<Vec<MiddlewareSettingManifest>> =
    std::sync::LazyLock::new(|| {
        vec![
            MiddlewareSettingManifest::Select {
                id: "approval_policy".into(),
                label: text::DEFINITION.setting_approval_policy_label.clone(),
                description: text::DEFINITION.setting_approval_policy_description.clone(),
                choices: MiddlewareSettingChoices::Static(vec![
                    MiddlewareSettingChoice {
                        disables: vec![],
                        value: "ask".into(),
                        label: text::DEFINITION.approval_policy_ask_label.clone(),
                        description: text::DEFINITION.approval_policy_ask_description.clone(),
                        symbol: Some("shield_check".into()),
                        tone: FrontendTone::Neutral,
                    },
                    MiddlewareSettingChoice {
                        disables: vec![],
                        value: "allow".into(),
                        label: text::DEFINITION.approval_policy_allow_label.clone(),
                        description: text::DEFINITION.approval_policy_allow_description.clone(),
                        symbol: Some("shield".into()),
                        tone: FrontendTone::Warning,
                    },
                    MiddlewareSettingChoice {
                        disables: vec![],
                        value: "allow_network".into(),
                        label: text::DEFINITION.approval_policy_allow_network_label.clone(),
                        description: text::DEFINITION
                            .approval_policy_allow_network_description
                            .clone(),
                        symbol: Some("shield_alert".into()),
                        tone: FrontendTone::Warning,
                    },
                    MiddlewareSettingChoice {
                        disables: vec![],
                        value: "full_access".into(),
                        label: text::DEFINITION.approval_policy_full_access_label.clone(),
                        description: text::DEFINITION
                            .approval_policy_full_access_description
                            .clone(),
                        symbol: Some("shield_off".into()),
                        tone: FrontendTone::Error,
                    },
                ]),
                unset_label: None,
                default: Some(text::DEFINITION.defaults_approval_policy.clone()),
                max_bytes: 32,
                composer: true,
            },
            MiddlewareSettingManifest::Integer {
                id: "tool_output_bytes".into(),
                label: text::DEFINITION.setting_tool_output_bytes_label.clone(),
                description: text::DEFINITION
                    .setting_tool_output_bytes_description
                    .clone(),
                min: 1,
                max: Some(
                    i64::try_from(MAX_TOOL_OUTPUT_BYTES).expect("output safety bound must fit"),
                ),
                step: text::DEFINITION.setting_tool_output_bytes_step,
                default: i64::try_from(default_tool_output_limit())
                    .expect("validated output default must fit"),
            },
            MiddlewareSettingManifest::Integer {
                id: "background_commands".into(),
                label: text::DEFINITION.setting_background_commands_label.clone(),
                description: text::DEFINITION
                    .setting_background_commands_description
                    .clone(),
                min: 1,
                max: Some(
                    i64::try_from(MAX_BACKGROUND_COMMANDS).expect("command safety bound must fit"),
                ),
                step: text::DEFINITION.setting_background_commands_step,
                default: i64::try_from(default_background_command_limit())
                    .expect("validated command default must fit"),
            },
        ]
    });

/// Configuration and presentation metadata for sandbox approval policy.
pub static MANIFEST: std::sync::LazyLock<MiddlewareManifest> =
    std::sync::LazyLock::new(|| MiddlewareManifest {
        id: "sandbox",
        label: text::DEFINITION.manifest_label.as_str(),
        description: text::DEFINITION.manifest_description.as_str(),
        required: true,
        default_enabled: true,
        required_model_capability: None,
        settings: &SETTINGS,
    });

pub use approval::ApprovalPolicy;
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub use process_group::MACOS_COMMAND_WRAPPER;
#[doc(hidden)]
pub use process_group::ProcessGroupGuard;

use approval::Approval;
pub(crate) use background::BackgroundCommandPoll;
#[cfg(test)]
pub(crate) use background::BackgroundCommandStatus;
use background::BackgroundCommands;

const MAX_TOOL_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_BACKGROUND_COMMANDS: usize = 256;

/// Default foreground shell command deadline in seconds.
#[must_use]
pub fn default_command_timeout_seconds() -> u64 {
    text::DEFINITION.defaults_command_timeout_seconds
}

/// Default retained text budget for command execution and tool dispatch.
#[must_use]
pub fn default_tool_output_limit() -> usize {
    text::DEFINITION.defaults_tool_output_bytes
}

/// Default number of tracked background commands per execution boundary.
#[must_use]
pub fn default_background_command_limit() -> usize {
    text::DEFINITION.defaults_background_commands
}

fn validate_tool_output_limit(bytes: usize) -> Result<()> {
    if !(1..=MAX_TOOL_OUTPUT_BYTES).contains(&bytes) {
        return Err(Error::Config(format!(
            "tool output limit must be between 1 and {MAX_TOOL_OUTPUT_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Deny-by-default macOS Seatbelt prelude shared by first-party sandbox backends.
///
/// Backends must append their own filesystem allow rules before using it.
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub const MACOS_SEATBELT_BASE_POLICY: &str = include_str!("seatbelt_base_policy.sbpl");

/// macOS platform services required by commands with approved network access.
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub const MACOS_SEATBELT_NETWORK_POLICY: &str = include_str!("seatbelt_network_policy.sbpl");

/// Whether a sandbox backend permits network access for one command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkAccess {
    #[default]
    /// Selects the denied case.
    Denied,
    /// Selects the allowed case.
    Allowed,
}

/// Whether an operation uses workspace isolation or host-wide access.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    #[default]
    /// Selects the workspace write case.
    WorkspaceWrite,
    /// Selects the danger full access case.
    DangerFullAccess,
}

/// How Linux commands see `/proc` while retaining user and PID namespace isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcfsMode {
    /// Mounts a private proc filesystem for the command's PID namespace.
    Private,
    /// Masks `/proc` with an empty filesystem.
    ///
    /// Select this explicitly only when the host already provides PID isolation.
    Empty,
}

impl Default for ProcfsMode {
    fn default() -> Self {
        text::DEFINITION.defaults_procfs_mode
    }
}

/// Bounded output from a sandboxed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// The exit code.
    pub exit_code: i32,
    /// The stdout.
    pub stdout: String,
    /// The stdout truncated.
    pub stdout_truncated: bool,
    /// The stderr.
    pub stderr: String,
    /// The stderr truncated.
    pub stderr_truncated: bool,
}

/// One byte stream emitted by a sandboxed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStream {
    /// Selects the stdout case.
    Stdout,
    /// Selects the stderr case.
    Stderr,
}

/// Whether command execution has a foreground deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandMode {
    /// Selects the foreground case.
    Foreground,
    /// Selects the background case.
    Background,
}

/// Optional observer for bounded command output consumers.
#[derive(Clone, Default)]
pub struct CommandOutputSink {
    callback: Option<Arc<CommandOutputCallback>>,
}

type CommandOutputCallback = dyn Fn(CommandStream, &[u8]) + Send + Sync;

/// Authorizes one command at its process-launch boundary.
///
/// Returning without invoking the callback denies the launch. Implementations must invoke it
/// only while the authoritative authorization state is held.
pub type CommandAuthorization =
    Arc<dyn Fn(&mut dyn FnMut() -> Result<()>) -> Result<()> + Send + Sync>;

impl CommandOutputSink {
    pub(crate) fn new(callback: impl Fn(CommandStream, &[u8]) + Send + Sync + 'static) -> Self {
        Self {
            callback: Some(Arc::new(callback)),
        }
    }

    /// Publishes one output chunk while the backend continues draining the stream.
    pub fn write(&self, stream: CommandStream, bytes: &[u8]) {
        if let Some(callback) = &self.callback {
            callback(stream, bytes);
        }
    }
}

/// Implements one sandbox execution environment.
///
/// Returned futures are [`Send`] and may be dropped during execution. Backends
/// own cancellation cleanup for the processes and resources they launch;
/// dropping a future must not leave unmanaged commands running. [`Sandbox`]
/// owns approval and background-command tracking, not arbitrary backend cleanup.
pub trait SandboxBackend: Send + Sync {
    /// Checks whether this execution environment is accepting work.
    /// # Errors
    ///
    /// Returns an error while the backend has suspended execution.
    fn check_execution(&self) -> Result<()> {
        Ok(())
    }

    /// Creates an independent temporary area and execution lifetime for a child agent.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    fn isolated_execution(&self) -> Result<Arc<dyn SandboxBackend>> {
        Err(Error::Sandbox(
            "backend does not support isolated child execution".into(),
        ))
    }

    /// Reports the private temporary path visible to command and file tools.
    fn temporary_directory(&self) -> Option<PathBuf> {
        None
    }

    /// Launches a persistent framed runtime inside this backend's execution boundary.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    fn start_worker(
        &self,
        _command: &WorkerCommand,
        _sandbox_mode: SandboxMode,
        _network_access: NetworkAccess,
    ) -> Result<WorkerProcess> {
        Err(Error::Sandbox(
            "persistent execution is unavailable on this backend".into(),
        ))
    }

    /// Opens an authorized host-service channel for one worker evaluation.
    /// Dropping the channel ends its authority; requests are never replayed.
    fn worker_connection<'a>(
        &'a self,
        _session_id: &'a str,
        _sandbox_mode: SandboxMode,
        _network_access: NetworkAccess,
    ) -> BoxFuture<'a, Result<tokio::io::DuplexStream>> {
        Box::pin(async {
            Err(Error::Sandbox(
                "native desktop control is unavailable".into(),
            ))
        })
    }

    /// Finds the gateway-owned browser page assigned to this execution, if available.
    /// # Errors
    ///
    /// Returns an error if the backend cannot assign a page or has suspended execution.
    fn desktop_browser_page<'a>(
        &'a self,
        _session_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<DesktopBrowserPage>>> {
        Box::pin(async { Ok(None) })
    }

    /// Reads a UTF-8 file under the requested isolation.
    fn read<'a>(
        &'a self,
        path: &'a str,
        sandbox_mode: SandboxMode,
    ) -> BoxFuture<'a, Result<String>>;

    /// Reads one binary file through a single bounded open handle under the requested isolation.
    fn read_bytes<'a>(
        &'a self,
        path: &'a str,
        max_bytes: usize,
        sandbox_mode: SandboxMode,
    ) -> BoxFuture<'a, Result<Vec<u8>>>;

    /// Writes a UTF-8 file under the requested isolation.
    fn write<'a>(
        &'a self,
        path: &'a str,
        content: &'a str,
        sandbox_mode: SandboxMode,
    ) -> BoxFuture<'a, Result<()>>;

    /// Runs a shell command and forwards drained output under the requested isolation.
    fn execute<'a>(
        &'a self,
        command: &'a str,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
        mode: CommandMode,
        output: CommandOutputSink,
    ) -> BoxFuture<'a, Result<CommandOutput>>;

    /// Runs a shell command only when authorization launches it atomically.
    ///
    /// Backends without an atomic launch boundary fail closed.
    fn execute_authorized<'a>(
        &'a self,
        _command: &'a str,
        _sandbox_mode: SandboxMode,
        _network_access: NetworkAccess,
        _mode: CommandMode,
        _output: CommandOutputSink,
        _authorization: &'a CommandAuthorization,
    ) -> BoxFuture<'a, Result<Option<CommandOutput>>> {
        Box::pin(async { Ok(None) })
    }
}

/// A browser owned outside the worker and its assigned page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopBrowserPage {
    /// A loopback browser endpoint or a scoped local Unix page endpoint.
    pub endpoint: String,
    /// The exact remote page target; local Unix endpoints expose one page only.
    pub target_id: Option<String>,
}

/// Approval-owning boundary around one execution backend.
pub struct Sandbox {
    backend: Arc<dyn SandboxBackend>,
    approval: Approval,
    background: BackgroundCommands,
    workers: worker::Workers,
    workspace_prompt: Option<String>,
    tool_output_limit: usize,
}

impl Sandbox {
    /// Creates a sandbox with its initial approval policy.
    #[must_use]
    pub fn new(backend: Arc<dyn SandboxBackend>, policy: ApprovalPolicy) -> Self {
        Self {
            backend,
            approval: Approval::new(policy),
            background: BackgroundCommands::default(),
            workers: worker::Workers::default(),
            workspace_prompt: None,
            tool_output_limit: default_tool_output_limit(),
        }
    }

    /// Creates a child execution boundary with independent temporary files and workers.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn isolated_execution(&self) -> Result<Self> {
        let mut scoped = Self::new(self.backend.isolated_execution()?, self.approval_policy());
        scoped.workspace_prompt = self.workspace_prompt.clone();
        scoped.tool_output_limit = self.tool_output_limit;
        scoped.background = BackgroundCommands::new(self.background.limit());
        Ok(scoped)
    }

    /// Sets the retained text budget for tool dispatch.
    /// # Errors
    /// Returns an error for a zero budget or one above the internal safety bound.
    pub fn tool_output_limit(mut self, bytes: usize) -> Result<Self> {
        validate_tool_output_limit(bytes)?;
        self.tool_output_limit = bytes;
        Ok(self)
    }

    /// Returns the configured tool text budget in UTF-8 bytes.
    #[must_use]
    pub const fn output_limit(&self) -> usize {
        self.tool_output_limit
    }

    /// Sets the maximum tracked background commands per execution boundary.
    /// # Errors
    /// Returns an error outside the supported range.
    pub fn background_command_limit(mut self, commands: usize) -> Result<Self> {
        if !(1..=MAX_BACKGROUND_COMMANDS).contains(&commands) {
            return Err(Error::Config(format!(
                "background command limit must be between 1 and {MAX_BACKGROUND_COMMANDS}"
            )));
        }
        self.background = BackgroundCommands::new(commands);
        Ok(self)
    }

    /// Adds the primary workspace and attached folder paths to the model prompt.
    #[must_use]
    pub fn attached_folders(mut self, primary: PathBuf, attached: Vec<PathBuf>) -> Self {
        let mut prompt = text::DEFINITION
            .prompt_workspace
            .replace("{path}", &format!("{primary:?}"));
        if !attached.is_empty() {
            prompt.push('\n');
            prompt.push_str(&text::DEFINITION.prompt_attached_folders);
            for path in attached {
                prompt.push_str(&format!("\n- {path:?}"));
            }
        }
        self.workspace_prompt = Some(prompt);
        self
    }

    pub(crate) fn platform_prompt() -> &'static str {
        if cfg!(target_os = "linux") {
            text::DEFINITION.prompt_linux.as_str()
        } else if cfg!(target_os = "macos") {
            text::DEFINITION.prompt_macos.as_str()
        } else {
            text::DEFINITION.prompt_other.as_str()
        }
    }

    pub(crate) const fn approval_policy(&self) -> ApprovalPolicy {
        self.approval.policy()
    }

    /// Reads a UTF-8 file.
    pub fn read<'a>(
        &'a self,
        path: &'a str,
        permissions: &'a ToolPermissions,
    ) -> BoxFuture<'a, Result<String>> {
        self.backend.read(path, permissions.sandbox_mode)
    }

    /// Reads one bounded binary file.
    pub fn read_bytes<'a>(
        &'a self,
        path: &'a str,
        max_bytes: usize,
        permissions: &'a ToolPermissions,
    ) -> BoxFuture<'a, Result<Vec<u8>>> {
        if max_bytes == 0 || max_bytes > MAX_BINARY_FILE_BYTES {
            return Box::pin(async {
                Err(Error::Sandbox(format!(
                    "binary file read size must be 1–{MAX_BINARY_FILE_BYTES} bytes"
                )))
            });
        }
        self.backend
            .read_bytes(path, max_bytes, permissions.sandbox_mode)
    }

    /// Writes a UTF-8 file when this call has mutation authority.
    pub fn write<'a>(
        &'a self,
        path: &'a str,
        content: &'a str,
        permissions: &'a ToolPermissions,
    ) -> BoxFuture<'a, Result<()>> {
        if !permissions.mutation {
            return Box::pin(async {
                Err(Error::Sandbox(
                    "tool call is not authorized to mutate the workspace".into(),
                ))
            });
        }
        if content.len() > MAX_FILE_BYTES {
            return Box::pin(async { Err(Error::Sandbox("file exceeds write limit".into())) });
        }
        self.backend.write(path, content, permissions.sandbox_mode)
    }

    /// Runs a command when this call has mutation authority.
    pub fn execute<'a>(
        &'a self,
        command: &'a str,
        permissions: &'a ToolPermissions,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        if !permissions.mutation {
            return Box::pin(async {
                Err(Error::Sandbox(
                    "tool call is not authorized to execute commands".into(),
                ))
            });
        }
        self.backend.execute(
            command,
            permissions.sandbox_mode,
            permissions.network_access,
            CommandMode::Foreground,
            CommandOutputSink::default(),
        )
    }

    /// Evaluates one authorized request, preserving runtime state until explicit reset or loss.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn evaluate_worker(
        &self,
        command: &WorkerCommand,
        permissions: &ToolPermissions,
        request: &[u8],
        timeout: std::time::Duration,
        reset: bool,
    ) -> Result<Vec<u8>> {
        self.backend.check_execution()?;
        self.workers
            .evaluate(
                self.backend.as_ref(),
                command,
                permissions,
                request,
                timeout,
                reset,
            )
            .await
    }

    /// The gateway-owned browser page assigned to this call's execution, if available.
    /// # Errors
    ///
    /// Returns an error if the backend cannot assign a page or has suspended execution.
    pub async fn desktop_browser_page(
        &self,
        permissions: &ToolPermissions,
    ) -> Result<Option<DesktopBrowserPage>> {
        self.backend.check_execution()?;
        self.backend
            .desktop_browser_page(&permissions.session_id)
            .await
    }

    pub(crate) async fn run_command(
        &self,
        command: String,
        permissions: &ToolPermissions,
        initial_wait: std::time::Duration,
    ) -> Result<BackgroundCommandPoll> {
        if !permissions.mutation {
            return Err(Error::Sandbox(
                "tool call is not authorized to execute commands".into(),
            ));
        }
        self.background
            .start(
                &permissions.session_id,
                Arc::clone(&self.backend),
                command,
                permissions.sandbox_mode,
                permissions.network_access,
            )?
            .wait(&permissions.session_id, initial_wait)
            .await
    }

    /// Reports whether one session has an unfinished background command.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn has_background_commands(&self, session_id: &str) -> Result<bool> {
        self.background.has_owner(session_id)
    }

    pub(crate) async fn poll_background(
        &self,
        id: &str,
        permissions: &ToolPermissions,
    ) -> Result<BackgroundCommandPoll> {
        self.background.poll(&permissions.session_id, id).await
    }

    pub(crate) async fn stop_background(
        &self,
        id: &str,
        permissions: &ToolPermissions,
    ) -> Result<BackgroundCommandPoll> {
        self.background.stop(&permissions.session_id, id).await
    }

    pub(crate) fn frontend(&self) -> FrontendContribution {
        self.approval.frontend()
    }

    pub(crate) fn render(&self, event: &EventMsg) -> Option<FrontendBlock> {
        self.approval.render(event)
    }

    pub(crate) fn session_start(&self, session_id: &str) -> Result<Vec<FrontendEvent>> {
        self.approval.session_start(session_id)
    }

    pub(crate) fn authorize<'a>(
        &self,
        session_id: &str,
        calls: &[ToolCall],
        mutation_call_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<SandboxAuthorization> {
        self.approval
            .authorize(session_id, calls, mutation_call_ids)
    }

    pub(crate) fn resolve_approval(
        &self,
        session_id: &str,
        calls: &[ToolCall],
        approval_call_ids: &[String],
        decision: &ReviewDecision,
        permissions: SandboxPermissions,
    ) -> Result<SandboxPermissions> {
        self.approval
            .resolve(session_id, calls, approval_call_ids, decision, permissions)
    }

    pub(crate) async fn session_end(&self, session_id: &str) -> Result<()> {
        let approval = self.approval.session_end(session_id);
        let background = self.background.shutdown(session_id).await;
        self.workers.shutdown(session_id).await;
        approval.and(background)
    }
}

impl Middleware for Sandbox {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn frontend(&self) -> FrontendContribution {
        Sandbox::frontend(self)
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        let mut prompt = Sandbox::platform_prompt().to_owned();
        if let Some(workspace) = &self.workspace_prompt {
            prompt.push_str("\n\n");
            prompt.push_str(workspace);
        }
        if let Some(temp) = self.backend.temporary_directory() {
            prompt.push('\n');
            prompt.push_str(
                &text::DEFINITION
                    .prompt_temporary_directory
                    .replace("{path}", &temp.display().to_string()),
            );
        }
        Ok(Some(PromptSection::new(prompt)))
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        Sandbox::render(self, event)
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if context.source() == SessionStartSource::Compact {
                return Ok(());
            }
            for event in Sandbox::session_start(self, &context.runtime.session_id)? {
                (context.runtime.frontend)(event)?;
            }
            Ok(())
        })
    }

    fn session_end<'a>(&'a self, runtime: &'a RuntimeContext) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Sandbox::session_end(self, &runtime.session_id).await })
    }
}

/// Batch authority issued by the sandbox approval policy.
#[derive(Debug)]
pub(crate) struct SandboxPermissions {
    session_id: String,
    sandbox_mode: SandboxMode,
    network_access: NetworkAccess,
    mutation_call_ids: BTreeSet<String>,
}

impl SandboxPermissions {
    fn new(
        session_id: impl Into<String>,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
        mutation_call_ids: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            sandbox_mode,
            network_access,
            mutation_call_ids: mutation_call_ids.into_iter().collect(),
        }
    }

    pub(crate) fn restore(
        session_id: impl Into<String>,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
        mutation_call_ids: impl IntoIterator<Item = String>,
    ) -> Self {
        Self::new(session_id, sandbox_mode, network_access, mutation_call_ids)
    }

    pub(crate) fn sandbox_mode(&self) -> SandboxMode {
        self.sandbox_mode
    }

    pub(crate) fn network_access(&self) -> NetworkAccess {
        self.network_access
    }

    pub(crate) fn mutation_call_ids(&self) -> Vec<String> {
        self.mutation_call_ids.iter().cloned().collect()
    }

    pub(crate) fn for_call(&self, call_id: &str) -> ToolPermissions {
        ToolPermissions {
            session_id: self.session_id.clone(),
            sandbox_mode: self.sandbox_mode,
            network_access: self.network_access,
            mutation: self.mutation_call_ids.contains(call_id),
        }
    }

    fn allow_mutations(&mut self, call_ids: impl IntoIterator<Item = String>) {
        self.mutation_call_ids.extend(call_ids);
    }
}

/// Opaque authority attached to exactly one tool call.
pub struct ToolPermissions {
    session_id: String,
    sandbox_mode: SandboxMode,
    network_access: NetworkAccess,
    mutation: bool,
}

impl ToolPermissions {
    pub(crate) fn sandbox_mode(&self) -> SandboxMode {
        self.sandbox_mode
    }

    pub(crate) fn network_access(&self) -> NetworkAccess {
        self.network_access
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn allows_mutation(&self) -> bool {
        self.mutation
    }
}

pub(crate) enum SandboxAuthorization {
    Execute(SandboxPermissions),
    Approval {
        request: SandboxApprovalRequest,
        permissions: SandboxPermissions,
    },
}

pub(crate) struct SandboxApprovalRequest {
    pub(crate) id: String,
    pub(crate) reason: String,
    pub(crate) call_ids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::sandbox::local::LocalSandbox;
    use crate::protocol::{FrontendSettingKind, FrontendSymbol};

    #[test]
    fn approval_policy_advertises_its_composer_presentation() {
        let feature = MANIFEST.feature(Default::default());
        let setting = feature
            .settings
            .iter()
            .find(|setting| setting.composer)
            .expect("composer setting");
        let FrontendSettingKind::Select { options, .. } = &setting.kind else {
            panic!("composer setting must be a select");
        };

        assert_eq!(setting.id, "approval_policy");
        assert_eq!(options[0].symbol, Some(FrontendSymbol::ShieldCheck));
        assert_eq!(options[3].symbol, Some(FrontendSymbol::ShieldOff));
        assert_eq!(options[3].tone, FrontendTone::Error);
    }

    #[test]
    fn workspace_prompt_names_primary_and_attached_folders() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sandbox = Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("backend")),
            ApprovalPolicy::Ask,
        )
        .attached_folders(
            PathBuf::from("/primary workspace"),
            vec![PathBuf::from("/attached\nworkspace")],
        );

        assert_eq!(
            sandbox.workspace_prompt.as_deref(),
            Some(
                "Primary workspace cwd: \"/primary workspace\".\nAttached writable folders (use absolute paths):\n- \"/attached\\nworkspace\""
            )
        );
    }

    #[tokio::test]
    async fn mutation_fails_closed_without_per_call_authority() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sandbox = Sandbox::new(
            Arc::new(LocalSandbox::new(workspace.path()).expect("backend")),
            ApprovalPolicy::Ask,
        );
        let permissions = SandboxPermissions::new(
            "session",
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Allowed,
            Vec::new(),
        );

        assert!(
            sandbox
                .write("blocked.txt", "blocked", &permissions.for_call("call"))
                .await
                .is_err()
        );
        assert!(!workspace.path().join("blocked.txt").exists());
    }
}
