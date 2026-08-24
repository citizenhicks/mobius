//! Sandboxed execution and its approval boundary.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::backend::model::ToolCall;
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

mod approval;
mod background;
pub mod local;
mod process_group;

pub(crate) const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_BINARY_FILE_BYTES: usize = 50 * 1024 * 1024;

mod text {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_backend_sandbox_sandbox_text.rs"
    ));
}

const APPROVAL_POLICIES: &[MiddlewareSettingChoice] = &[
    MiddlewareSettingChoice {
        value: "ask",
        label: text::APPROVAL_POLICY_ASK_LABEL,
        description: text::APPROVAL_POLICY_ASK_DESCRIPTION,
        symbol: Some("shield_check"),
        tone: FrontendTone::Neutral,
    },
    MiddlewareSettingChoice {
        value: "allow",
        label: text::APPROVAL_POLICY_ALLOW_LABEL,
        description: text::APPROVAL_POLICY_ALLOW_DESCRIPTION,
        symbol: Some("shield"),
        tone: FrontendTone::Warning,
    },
    MiddlewareSettingChoice {
        value: "allow_network",
        label: text::APPROVAL_POLICY_ALLOW_NETWORK_LABEL,
        description: text::APPROVAL_POLICY_ALLOW_NETWORK_DESCRIPTION,
        symbol: Some("shield_alert"),
        tone: FrontendTone::Warning,
    },
    MiddlewareSettingChoice {
        value: "auto_approve",
        label: text::APPROVAL_POLICY_AUTO_APPROVE_LABEL,
        description: text::APPROVAL_POLICY_AUTO_APPROVE_DESCRIPTION,
        symbol: Some("security_review"),
        tone: FrontendTone::Warning,
    },
    MiddlewareSettingChoice {
        value: "full_access",
        label: text::APPROVAL_POLICY_FULL_ACCESS_LABEL,
        description: text::APPROVAL_POLICY_FULL_ACCESS_DESCRIPTION,
        symbol: Some("shield_off"),
        tone: FrontendTone::Error,
    },
];
const REVIEWER_STRICTNESS: &[MiddlewareSettingChoice] = &[
    MiddlewareSettingChoice {
        value: "relaxed",
        label: text::REVIEWER_STRICTNESS_RELAXED_LABEL,
        description: text::REVIEWER_STRICTNESS_RELAXED_DESCRIPTION,
        symbol: None,
        tone: FrontendTone::Neutral,
    },
    MiddlewareSettingChoice {
        value: "standard",
        label: text::REVIEWER_STRICTNESS_STANDARD_LABEL,
        description: text::REVIEWER_STRICTNESS_STANDARD_DESCRIPTION,
        symbol: None,
        tone: FrontendTone::Neutral,
    },
    MiddlewareSettingChoice {
        value: "strict",
        label: text::REVIEWER_STRICTNESS_STRICT_LABEL,
        description: text::REVIEWER_STRICTNESS_STRICT_DESCRIPTION,
        symbol: None,
        tone: FrontendTone::Neutral,
    },
];
const SETTINGS: &[MiddlewareSettingManifest] = &[
    MiddlewareSettingManifest::Select {
        id: "approval_policy",
        label: text::SETTING_APPROVAL_POLICY_LABEL,
        description: text::SETTING_APPROVAL_POLICY_DESCRIPTION,
        choices: MiddlewareSettingChoices::Static(APPROVAL_POLICIES),
        unset_label: None,
        default: Some(text::DEFAULTS_APPROVAL_POLICY),
        max_bytes: 32,
        composer: true,
    },
    MiddlewareSettingManifest::Select {
        id: "reviewer_model_route",
        label: text::SETTING_REVIEWER_MODEL_ROUTE_LABEL,
        description: text::SETTING_REVIEWER_MODEL_ROUTE_DESCRIPTION,
        choices: MiddlewareSettingChoices::ModelRoutes,
        unset_label: Some(text::SETTING_REVIEWER_MODEL_ROUTE_UNSET_LABEL),
        default: None,
        max_bytes: 4 * 1024,
        composer: false,
    },
    MiddlewareSettingManifest::Select {
        id: "reviewer_strictness",
        label: text::SETTING_REVIEWER_STRICTNESS_LABEL,
        description: text::SETTING_REVIEWER_STRICTNESS_DESCRIPTION,
        choices: MiddlewareSettingChoices::Static(REVIEWER_STRICTNESS),
        unset_label: None,
        default: Some(text::DEFAULTS_REVIEWER_STRICTNESS),
        max_bytes: 16,
        composer: false,
    },
];

/// Configuration and presentation metadata for sandbox approval policy.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "sandbox",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: true,
    default_enabled: true,
    settings: SETTINGS,
};

pub use approval::ApprovalPolicy;
pub use approval::ApprovalReviewerConfig;
pub use approval::ApprovalStrictness;
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
    Denied,
    Allowed,
}

/// Whether a command uses workspace isolation or host-wide access.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    #[default]
    WorkspaceWrite,
    DangerFullAccess,
}

/// Bounded output from a sandboxed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stdout_truncated: bool,
    pub stderr: String,
    pub stderr_truncated: bool,
}

/// One byte stream emitted by a sandboxed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStream {
    Stdout,
    Stderr,
}

/// Whether command execution has a foreground deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandMode {
    Foreground,
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
pub trait SandboxBackend: Send + Sync {
    /// Reads a UTF-8 file.
    fn read<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<String>>;

    /// Reads one binary file through a single bounded open handle.
    fn read_bytes<'a>(&'a self, path: &'a str, max_bytes: usize) -> BoxFuture<'a, Result<Vec<u8>>>;

    /// Writes a UTF-8 file.
    fn write<'a>(&'a self, path: &'a str, content: &'a str) -> BoxFuture<'a, Result<()>>;

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

/// Approval-owning boundary around one execution backend.
pub struct Sandbox {
    backend: Arc<dyn SandboxBackend>,
    approval: Approval,
    background: BackgroundCommands,
}

impl Sandbox {
    /// Creates a sandbox with its initial approval policy.
    #[must_use]
    pub fn new(backend: Arc<dyn SandboxBackend>, policy: ApprovalPolicy) -> Self {
        Self {
            backend,
            approval: Approval::new(policy),
            background: BackgroundCommands::default(),
        }
    }

    pub(crate) fn platform_prompt() -> &'static str {
        if cfg!(target_os = "linux") {
            text::PROMPT_LINUX
        } else if cfg!(target_os = "macos") {
            text::PROMPT_MACOS
        } else {
            text::PROMPT_OTHER
        }
    }

    pub(crate) const fn approval_policy(&self) -> ApprovalPolicy {
        self.approval.policy()
    }

    /// Configures the isolated model reviewer used by automatic approval.
    #[must_use]
    pub fn approval_reviewer(mut self, reviewer: ApprovalReviewerConfig) -> Self {
        self.approval = self.approval.with_reviewer(reviewer);
        self
    }

    /// Reads a UTF-8 file.
    pub fn read<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<String>> {
        self.backend.read(path)
    }

    /// Reads one bounded binary file.
    pub fn read_bytes<'a>(
        &'a self,
        path: &'a str,
        max_bytes: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>>> {
        if max_bytes == 0 || max_bytes > MAX_BINARY_FILE_BYTES {
            return Box::pin(async {
                Err(Error::Sandbox(format!(
                    "binary file read size must be 1–{MAX_BINARY_FILE_BYTES} bytes"
                )))
            });
        }
        self.backend.read_bytes(path, max_bytes)
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
        self.backend.write(path, content)
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

    pub(crate) fn start_background(
        &self,
        command: String,
        permissions: &ToolPermissions,
    ) -> Result<String> {
        if !permissions.mutation {
            return Err(Error::Sandbox(
                "tool call is not authorized to execute commands".into(),
            ));
        }
        self.background.start(
            &permissions.session_id,
            Arc::clone(&self.backend),
            command,
            permissions.sandbox_mode,
            permissions.network_access,
        )
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

    pub(crate) fn authorize(
        &self,
        session_id: &str,
        calls: &[ToolCall],
        mutation_call_ids: &[String],
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
        Ok(Some(PromptSection::new(Sandbox::platform_prompt())))
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
    pub(crate) fn allows_mutation(&self) -> bool {
        self.mutation
    }
}

pub(crate) enum SandboxAuthorization {
    Execute(SandboxPermissions),
    Review(SandboxReview),
    Approval {
        request: SandboxApprovalRequest,
        permissions: SandboxPermissions,
    },
}

pub(crate) struct SandboxReview {
    pub(crate) request: SandboxApprovalRequest,
    pub(crate) reviewer: ApprovalReviewerConfig,
    pub(crate) permissions: SandboxPermissions,
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
        let feature = MANIFEST.feature(&[]);
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
        assert_eq!(options[3].symbol, Some(FrontendSymbol::SecurityReview));
        assert_eq!(options[4].symbol, Some(FrontendSymbol::ShieldOff));
        assert_eq!(options[4].tone, FrontendTone::Error);
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
