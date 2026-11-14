//! The approval-owning sandbox middleware.

use crate::SessionId;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use super::ApprovalPolicy;
use super::BackgroundCommandPoll;
use super::DesktopBrowserPage;
use super::MANIFEST;
use super::MAX_BACKGROUND_COMMANDS;
use super::MAX_BINARY_FILE_BYTES;
use super::MAX_FILE_BYTES;
use super::MutationPermit;
use super::OutputLimit;
use super::SandboxAuthorization;
use super::SandboxBackend;
use super::SandboxPermissions;
use super::ToolPermissions;
use super::WorkerCommand;
use super::approval::Approval;
use super::background::BackgroundCommands;
use super::default_tool_output_limit;
use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::middleware::Middleware;
use crate::middleware::PromptSection;
use crate::middleware::RuntimeContext;
use crate::middleware::SessionStartContext;
use crate::middleware::SessionStartSource;
use crate::protocol::EventMsg;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendContribution;
use crate::protocol::ReviewDecision;
use crate::protocol::ToolCall;

/// Approval-owning boundary around one execution backend.
pub struct Sandbox {
    backend: Arc<dyn SandboxBackend>,
    approval: Approval,
    background: BackgroundCommands,
    workers: super::worker::Workers,
    workspace_prompt: Option<Arc<str>>,
    tool_output_limit: usize,
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Sandbox")
            .field("workspace_prompt", &self.workspace_prompt)
            .field("tool_output_limit", &self.tool_output_limit)
            .finish_non_exhaustive()
    }
}

impl Sandbox {
    /// Creates a sandbox with its initial approval policy.
    #[must_use]
    pub fn new(backend: Arc<dyn SandboxBackend>, policy: ApprovalPolicy) -> Self {
        Self {
            backend,
            approval: Approval::new(policy),
            background: BackgroundCommands::default(),
            workers: super::worker::Workers::default(),
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
        scoped.workspace_prompt = self.workspace_prompt.as_ref().map(Arc::clone);
        scoped.tool_output_limit = self.tool_output_limit;
        scoped.background = BackgroundCommands::new(self.background.limit());
        Ok(scoped)
    }

    /// Sets the retained text budget for tool dispatch.
    #[must_use]
    pub const fn tool_output_limit(mut self, limit: OutputLimit) -> Self {
        self.tool_output_limit = limit.0;
        self
    }

    /// Returns the configured tool text budget in UTF-8 bytes.
    #[must_use]
    pub const fn output_limit(&self) -> usize {
        self.tool_output_limit
    }

    /// Sets the maximum tracked background commands per execution boundary, capped at the
    /// supported maximum.
    #[must_use]
    pub fn background_command_limit(mut self, commands: std::num::NonZeroUsize) -> Self {
        self.background = BackgroundCommands::new(commands.get().min(MAX_BACKGROUND_COMMANDS));
        self
    }

    /// Adds the primary workspace and attached folder paths to the model prompt.
    #[must_use]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "public builder signature shared with gateway assembly"
    )]
    #[expect(
        clippy::expect_used,
        reason = "fmt::Write for String never returns an error"
    )]
    pub fn attached_folders(mut self, primary: PathBuf, attached: Vec<PathBuf>) -> Self {
        let mut prompt = super::text::DEFINITION
            .prompt_workspace
            .replace("{path}", &format!("\"{}\"", primary.display()));
        if !attached.is_empty() {
            prompt.push('\n');
            prompt.push_str(&super::text::DEFINITION.prompt_attached_folders);
            for path in attached {
                write!(prompt, "\n- \"{}\"", path.display())
                    .expect("writing to String cannot fail");
            }
        }
        self.workspace_prompt = Some(prompt.into());
        self
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

    /// Writes a UTF-8 file under this call's mutation permit.
    pub fn write<'a>(
        &'a self,
        path: &'a str,
        content: &'a str,
        permit: MutationPermit<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        if content.len() > MAX_FILE_BYTES {
            return Box::pin(async { Err(Error::Sandbox("file exceeds write limit".into())) });
        }
        self.backend.write(path, content, permit.0.sandbox_mode)
    }

    /// Evaluates one authorized request, preserving runtime state until explicit reset or loss.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn evaluate_worker(
        &self,
        command: &WorkerCommand,
        permit: MutationPermit<'_>,
        request: &[u8],
        timeout: std::time::Duration,
        reset: bool,
    ) -> Result<Vec<u8>> {
        self.backend.check_execution()?;
        self.workers
            .evaluate(
                self.backend.as_ref(),
                command,
                permit.0,
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
        self.backend
            .desktop_browser_page(&permissions.session_id)
            .await
    }

    pub(crate) async fn run_command(
        &self,
        command: String,
        permit: MutationPermit<'_>,
        initial_wait: std::time::Duration,
        observer: super::CommandOutputSink,
    ) -> Result<BackgroundCommandPoll> {
        let permissions = permit.0;
        self.background
            .start(
                &permissions.session_id,
                Arc::clone(&self.backend),
                command,
                permissions.sandbox_mode,
                permissions.network_access,
                observer,
            )?
            .wait(&permissions.session_id, initial_wait)
            .await
    }

    /// Reports whether one session has an unfinished background command.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn has_background_commands(&self, session_id: &SessionId) -> Result<bool> {
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

    pub(crate) fn authorize<'a>(
        &self,
        session_id: &SessionId,
        calls: &[ToolCall],
        mutation_call_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<SandboxAuthorization> {
        self.approval
            .authorize(session_id, calls, mutation_call_ids)
    }

    pub(crate) fn resolve_approval(
        &self,
        session_id: &SessionId,
        calls: &[ToolCall],
        approval_call_ids: &[Arc<str>],
        decision: &ReviewDecision,
        permissions: SandboxPermissions,
    ) -> Result<SandboxPermissions> {
        self.approval
            .resolve(session_id, calls, approval_call_ids, decision, permissions)
    }

    pub(crate) async fn session_end(&self, session_id: &SessionId) -> Result<()> {
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

    fn frontend(&self, _session_id: &str) -> FrontendContribution {
        self.approval.frontend()
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        let platform = if cfg!(target_os = "linux") {
            super::text::DEFINITION.prompt_linux.as_str()
        } else if cfg!(target_os = "macos") {
            super::text::DEFINITION.prompt_macos.as_str()
        } else {
            super::text::DEFINITION.prompt_other.as_str()
        };
        let mut prompt = platform.to_owned();
        if let Some(workspace) = &self.workspace_prompt {
            prompt.push_str("\n\n");
            prompt.push_str(workspace);
        }
        if let Some(temp) = self.backend.temporary_directory() {
            prompt.push('\n');
            prompt.push_str(
                &super::text::DEFINITION
                    .prompt_temporary_directory
                    .replace("{path}", &temp.display().to_string()),
            );
        }
        Ok(Some(PromptSection::new(prompt)))
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        Approval::render(event)
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if context.source() == SessionStartSource::Compact {
                return Ok(());
            }
            for event in self.approval.session_start(&context.runtime.session_id)? {
                (context.runtime.frontend)(event)?;
            }
            let sender = context.runtime.sender.clone(); // rules-allow: the listener owns its weak sender handle
            self.background
                .listen(&context.runtime.session_id, background_notices(sender))?;
            Ok(())
        })
    }

    fn session_end<'a>(&'a self, runtime: &'a RuntimeContext) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Self::session_end(self, &runtime.session_id).await })
    }
}

fn background_notices(
    sender: crate::agent::WeakAgentSender,
) -> super::background::BackgroundListener {
    Arc::new(move |command_id, text| {
        let Some(sender) = sender.upgrade() else {
            return;
        };
        let notice = crate::protocol::MessageSubmission {
            author: crate::protocol::MessageAuthor::Source {
                message_id: uuid::Uuid::new_v4().to_string().into(),
                source: crate::protocol::MessageSource::External {
                    source_id: "background-command".into(),
                    event_id: command_id.into(),
                },
                cause_id: None,
                ancestry: Vec::new(),
                handle: "background-command".into(),
                symbol: None,
            },
            text,
            attachments: Vec::new(),
            reply: None,
            requested_delivery: Some(crate::protocol::ActiveMessageDelivery::Steer),
            target_turn_id: None,
        };
        if let Err(error) = sender.send(crate::protocol::Submission::message(notice)) {
            tracing::warn!(%error, command_id, "background command notice was not delivered");
        }
    })
}
