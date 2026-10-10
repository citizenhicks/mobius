//! Gateway sandbox with protected and host-wide command modes.

mod config;
pub use config::ExecutionConfig;

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use mobius::backend::model::provider::{ProviderAuth, providers};
use mobius::backend::sandbox::{
    CommandAuthorization, CommandMode, CommandOutput, CommandOutputSink, NetworkAccess,
    SandboxBackend, SandboxMode, local::LocalSandbox,
};
use mobius::{BoxFuture, Error, Result};

use crate::git::{GIT_ENVIRONMENT, REPOSITORY_LOCAL_GIT_ENVIRONMENT};

const GIT_ARGUMENTS: [&str; 7] = [
    "--no-pager",
    "-c",
    "safe.bareRepository=explicit",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
];
const GATEWAY_CREDENTIAL_ENVIRONMENT: [&str; 5] = [
    "MOBIUS_GATEWAY_TOKEN",
    "MOBIUS_GATEWAY_BEARER_TOKEN",
    "MOBIUS_GATEWAY_TELEMETRY_TOKEN",
    "TUNNEL_TOKEN",
    "TUNNEL_TOKEN_FILE",
];
fn provider_credential_environment() -> impl Iterator<Item = &'static str> {
    providers()
        .iter()
        .filter_map(|provider| match provider.auth() {
            ProviderAuth::ApiKey(environment) => Some(environment),
            ProviderAuth::Browser(_) => None,
        })
}

pub(crate) fn reserved_credential_environment() -> impl Iterator<Item = &'static str> {
    GATEWAY_CREDENTIAL_ENVIRONMENT
        .into_iter()
        .chain(provider_credential_environment())
}

/// Workspace backend that protects gateway state outside full-access commands.
pub struct GatewaySandbox {
    delegate: std::sync::Arc<LocalSandbox>,
    full_access_delegate: std::sync::Arc<LocalSandbox>,
    desktop: Option<std::sync::Arc<crate::computer_runtime::desktop::DesktopControl>>,
    remote_desktop: Option<std::sync::Arc<crate::computer_runtime::remote_desktop::RemoteDesktop>>,
    desktop_use: tokio::sync::Mutex<Option<crate::computer_runtime::remote_desktop::DesktopUse>>,
}

impl GatewaySandbox {
    /// Creates protected and full-access command delegates for a gateway host.
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or a required resource cannot be initialized.
    pub fn new(
        workspace: &Path,
        state_dir: &Path,
        tls_key: Option<&Path>,
        timeout: Duration,
    ) -> Result<Self> {
        Self::build(
            workspace,
            state_dir,
            tls_key,
            timeout,
            None,
            mobius::backend::sandbox::default_tool_output_limit(),
            &[],
        )
    }

    /// Builds delegates with the operator's executable, output, and environment policy.
    /// # Errors
    /// Returns an error for invalid policy or inaccessible sandbox roots.
    pub fn new_configured(
        workspace: &Path,
        state_dir: &Path,
        tls_key: Option<&Path>,
        policy: &ExecutionConfig,
        output_bytes: usize,
        credential_environment: &[&str],
    ) -> Result<Self> {
        policy
            .validate()
            .map_err(|error| Error::Config(error.to_string()))?;
        Self::build(
            workspace,
            state_dir,
            tls_key,
            Duration::from_secs(policy.command_timeout_seconds),
            Some(policy),
            output_bytes,
            credential_environment,
        )
    }

    fn build(
        workspace: &Path,
        state_dir: &Path,
        tls_key: Option<&Path>,
        timeout: Duration,
        policy: Option<&ExecutionConfig>,
        output_bytes: usize,
        credential_environment: &[&str],
    ) -> Result<Self> {
        if timeout.is_zero() {
            return Err(Error::Config("command timeout must be positive".into()));
        }
        let root = std::fs::canonicalize(workspace)?;
        let state_dir = std::fs::canonicalize(state_dir)?;
        let tls_key = tls_key.map(std::fs::canonicalize).transpose()?;
        let tls_key = tls_key.as_deref().unwrap_or(&state_dir);
        if root.starts_with(&state_dir) || state_dir.starts_with(&root) {
            return Err(Error::Config(
                "gateway state directory and chat workspace must not overlap".into(),
            ));
        }
        if tls_key.starts_with(&root) {
            return Err(Error::Config(
                "TLS private key must be stored outside every chat workspace".into(),
            ));
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        return Err(Error::Config(
            "gateway command sandbox supports macOS and Linux only".into(),
        ));

        let mut delegate = LocalSandbox::new(&root)?
            .command_timeout(timeout)?
            .deny_read(&state_dir)?
            .deny_read(tls_key)?;
        let mut full_access_delegate = LocalSandbox::new(&root)?
            .command_timeout(timeout)?
            .share_temporary_directory(&delegate)?;
        delegate = configure_execution(delegate, policy, output_bytes, credential_environment)?;
        full_access_delegate = configure_execution(
            full_access_delegate,
            policy,
            output_bytes,
            credential_environment,
        )?;
        for environment in reserved_credential_environment() {
            delegate = delegate.deny_environment(environment);
            full_access_delegate = full_access_delegate.deny_environment(environment);
        }
        Ok(Self {
            delegate: std::sync::Arc::new(delegate),
            full_access_delegate: std::sync::Arc::new(full_access_delegate),
            desktop: None,
            remote_desktop: None,
            desktop_use: tokio::sync::Mutex::new(None),
        })
    }

    pub(crate) fn with_desktop(
        mut self,
        desktop: std::sync::Arc<crate::computer_runtime::desktop::DesktopControl>,
    ) -> Self {
        self.desktop = Some(desktop);
        self
    }

    pub(crate) fn with_remote_desktop(
        mut self,
        remote_desktop: std::sync::Arc<crate::computer_runtime::remote_desktop::RemoteDesktop>,
    ) -> Self {
        self.remote_desktop = Some(remote_desktop);
        self
    }

    fn check_execution(&self) -> Result<()> {
        if let Some(desktop) = &self.remote_desktop {
            desktop.check_execution()?;
        }
        Ok(())
    }

    async fn use_desktop(&self) {
        if let Some(remote) = self
            .remote_desktop
            .as_ref()
            .filter(|remote| remote.available())
        {
            let mut desktop_use = self.desktop_use.lock().await;
            if desktop_use.is_none() {
                *desktop_use = Some(remote.acquire_use().await);
            }
        }
    }

    pub(crate) async fn release_desktop_use(&self) {
        self.desktop_use.lock().await.take();
    }

    #[cfg(test)]
    pub(crate) async fn retain_desktop_use_for_test(&self) {
        let remote = self.remote_desktop.as_ref().expect("desktop owner");
        *self.desktop_use.lock().await = Some(remote.acquire_use().await);
    }

    fn isolated_backend(&self) -> Result<Self> {
        self.check_execution()?;
        let delegate = self.delegate.isolated_execution()?;
        let full_access_delegate = self
            .full_access_delegate
            .isolated_execution()?
            .share_temporary_directory(&delegate)?;
        Ok(Self {
            delegate: std::sync::Arc::new(delegate),
            full_access_delegate: std::sync::Arc::new(full_access_delegate),
            // Isolated execution shares desktop devices while giving each backend its own execution state.
            desktop: self.desktop.clone(),
            remote_desktop: self.remote_desktop.as_ref().map(std::sync::Arc::clone),
            desktop_use: tokio::sync::Mutex::new(None),
        })
    }

    async fn execution_lease(&self) -> Result<Option<tokio::sync::OwnedRwLockReadGuard<()>>> {
        match &self.remote_desktop {
            Some(desktop) => desktop.execution_lease().await.map(Some),
            None => Ok(None),
        }
    }

    async fn execution_cancelled(&self) {
        match &self.remote_desktop {
            Some(desktop) => desktop.execution_cancelled().await,
            None => std::future::pending().await,
        }
    }

    fn command_delegate(&self, sandbox_mode: SandboxMode) -> &std::sync::Arc<LocalSandbox> {
        match sandbox_mode {
            SandboxMode::WorkspaceWrite => &self.delegate,
            SandboxMode::DangerFullAccess => &self.full_access_delegate,
        }
    }

    pub(crate) fn deny_read_paths(
        mut self,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self> {
        let mut delegate = std::sync::Arc::try_unwrap(self.delegate).map_err(|_| {
            Error::Config("sandbox paths must be configured before execution".into())
        })?;
        for path in paths {
            delegate = delegate.deny_read(path)?;
        }
        self.delegate = std::sync::Arc::new(delegate);
        Ok(self)
    }

    pub(crate) fn allow_read_roots(
        mut self,
        roots: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self> {
        let mut delegate = std::sync::Arc::try_unwrap(self.delegate).map_err(|_| {
            Error::Config("sandbox roots must be configured before execution".into())
        })?;
        for root in roots {
            delegate = delegate.allow_read_root(root)?;
        }
        self.delegate = std::sync::Arc::new(delegate);
        Ok(self)
    }

    pub(crate) fn allow_attached_folders(
        mut self,
        roots: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self> {
        let mut delegate = std::sync::Arc::try_unwrap(self.delegate).map_err(|_| {
            Error::Config("sandbox roots must be configured before execution".into())
        })?;
        for root in roots {
            delegate = delegate.allow_workspace_root(root)?;
        }
        self.delegate = std::sync::Arc::new(delegate);
        Ok(self)
    }

    pub(crate) async fn execute_git(&self, args: &[&str]) -> Result<CommandOutput> {
        let _execution = self.execution_lease().await?;
        let mut arguments = GIT_ARGUMENTS.to_vec();
        arguments.extend_from_slice(args);
        tokio::select! {
            _ = self.execution_cancelled() => Err(execution_held()),
            result = self.delegate.execute_read_only_with_environment_removals(
                "git",
                &arguments,
                &GIT_ENVIRONMENT,
                &REPOSITORY_LOCAL_GIT_ENVIRONMENT,
            ) => result,
        }
    }

    pub(crate) async fn read_workspace_range(
        &self,
        path: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<(Vec<u8>, Option<u64>)> {
        let execution = self.execution_lease().await?;
        let delegate = std::sync::Arc::clone(&self.delegate);
        let path = path.to_owned();
        tokio::spawn(async move {
            let _execution = execution;
            delegate.read_range(&path, offset, max_bytes).await
        })
        .await
        .map_err(|error| Error::Sandbox(format!("file reader failed: {error}")))?
    }

    pub(crate) async fn remove(&self, path: &str) -> Result<()> {
        let execution = self.execution_lease().await?;
        let delegate = std::sync::Arc::clone(&self.delegate);
        let path = path.to_owned();
        tokio::spawn(async move {
            let _execution = execution;
            delegate.remove_file(&path).await
        })
        .await
        .map_err(|error| Error::Sandbox(format!("file deletion failed: {error}")))?
    }

    pub(crate) async fn switch_git_branch(&self, branch: &str) -> Result<CommandOutput> {
        let _execution = self.execution_lease().await?;
        let mut arguments = GIT_ARGUMENTS.to_vec();
        arguments.extend_from_slice(&[
            "switch",
            "--no-guess",
            "--no-recurse-submodules",
            "--",
            branch,
        ]);
        tokio::select! {
            _ = self.execution_cancelled() => Err(execution_held()),
            result = self.delegate.execute_git_mutation_with_environment_removals(
                &arguments,
                &GIT_ENVIRONMENT,
                &REPOSITORY_LOCAL_GIT_ENVIRONMENT,
            ) => result,
        }
    }
}

fn configure_execution(
    mut sandbox: LocalSandbox,
    policy: Option<&ExecutionConfig>,
    output_bytes: usize,
    credential_environment: &[&str],
) -> Result<LocalSandbox> {
    sandbox = sandbox.command_output_limit(output_bytes)?;
    if let Some(policy) = policy {
        #[cfg(target_os = "linux")]
        {
            sandbox = sandbox.procfs_mode(policy.procfs_mode);
        }
        if let Some(shell) = &policy.shell_executable {
            sandbox = sandbox.shell_executable(shell)?;
        }
        if let Some(bubblewrap) = &policy.bubblewrap_executable {
            sandbox = sandbox.bubblewrap_executable(bubblewrap)?;
        }
    }
    for name in credential_environment {
        sandbox = sandbox.deny_environment(*name);
    }
    Ok(sandbox)
}

fn execution_held() -> Error {
    Error::Sandbox("execution is held while the user controls the desktop".into())
}

impl SandboxBackend for GatewaySandbox {
    fn check_execution(&self) -> Result<()> {
        Self::check_execution(self)
    }

    fn worker_connection<'a>(
        &'a self,
        session_id: &'a str,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
    ) -> BoxFuture<'a, Result<tokio::io::DuplexStream>> {
        Box::pin(async move {
            self.check_execution()?;
            if sandbox_mode != SandboxMode::DangerFullAccess {
                return Err(Error::Sandbox(
                    "desktop control requires the Bot's Full access sandbox policy".into(),
                ));
            }
            if network_access == NetworkAccess::Allowed
                && let Some(remote) = self
                    .remote_desktop
                    .as_ref()
                    .filter(|remote| remote.browser_available())
            {
                self.use_desktop().await;
                let native = self.desktop.as_ref().map_or_else(
                    || {
                        std::sync::Arc::new(
                            crate::computer_runtime::desktop::DesktopControl::default(),
                        )
                    },
                    std::sync::Arc::clone,
                );
                return remote.connect(session_id, native);
            }
            self.desktop
                .as_ref()
                .ok_or_else(|| Error::Sandbox("native desktop runtime is unavailable".into()))?
                .connect(session_id)
        })
    }

    fn desktop_browser_page<'a>(
        &'a self,
        session_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<mobius::backend::sandbox::DesktopBrowserPage>>> {
        Box::pin(async move {
            self.check_execution()?;
            self.use_desktop().await;
            match &self.remote_desktop {
                Some(remote) => remote
                    .page(session_id)
                    .await
                    .map_err(|error| Error::Sandbox(error.to_string())),
                None => Ok(None),
            }
        })
    }

    fn start_worker(
        &self,
        command: &mobius::backend::sandbox::WorkerCommand,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
    ) -> Result<mobius::backend::sandbox::WorkerProcess> {
        self.check_execution()?;
        self.command_delegate(sandbox_mode)
            .start_worker(command, sandbox_mode, network_access)
    }

    fn isolated_execution(&self) -> Result<std::sync::Arc<dyn SandboxBackend>> {
        Ok(std::sync::Arc::new(self.isolated_backend()?))
    }

    fn temporary_directory(&self) -> Option<PathBuf> {
        Some(self.delegate.temporary_directory().into())
    }

    fn read<'a>(
        &'a self,
        path: &'a str,
        sandbox_mode: SandboxMode,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let execution = self.execution_lease().await?;
            let delegate = std::sync::Arc::clone(self.command_delegate(sandbox_mode));
            let path = path.to_owned();
            tokio::spawn(async move {
                let _execution = execution;
                delegate.read(&path, sandbox_mode).await
            })
            .await
            .map_err(|error| Error::Sandbox(format!("file reader failed: {error}")))?
        })
    }

    fn read_bytes<'a>(
        &'a self,
        path: &'a str,
        max_bytes: usize,
        sandbox_mode: SandboxMode,
    ) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            let execution = self.execution_lease().await?;
            let delegate = std::sync::Arc::clone(self.command_delegate(sandbox_mode));
            let path = path.to_owned();
            tokio::spawn(async move {
                let _execution = execution;
                delegate.read_bytes(&path, max_bytes, sandbox_mode).await
            })
            .await
            .map_err(|error| Error::Sandbox(format!("file reader failed: {error}")))?
        })
    }

    fn write<'a>(
        &'a self,
        path: &'a str,
        content: &'a str,
        sandbox_mode: SandboxMode,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let execution = self.execution_lease().await?;
            let delegate = std::sync::Arc::clone(self.command_delegate(sandbox_mode));
            let path = path.to_owned();
            let content = content.to_owned();
            tokio::spawn(async move {
                let _execution = execution;
                delegate.write(&path, &content, sandbox_mode).await
            })
            .await
            .map_err(|error| Error::Sandbox(format!("file writer failed: {error}")))?
        })
    }

    fn execute<'a>(
        &'a self,
        script: &'a str,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
        mode: CommandMode,
        output: CommandOutputSink,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        Box::pin(async move {
            let _execution = self.execution_lease().await?;
            tokio::select! {
                _ = self.execution_cancelled() => Err(execution_held()),
                result = self.command_delegate(sandbox_mode).execute(script, sandbox_mode, network_access, mode, output) => result,
            }
        })
    }

    fn execute_authorized<'a>(
        &'a self,
        script: &'a str,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
        mode: CommandMode,
        output: CommandOutputSink,
        authorization: &'a CommandAuthorization,
    ) -> BoxFuture<'a, Result<Option<CommandOutput>>> {
        Box::pin(async move {
            let _execution = self.execution_lease().await?;
            tokio::select! {
                _ = self.execution_cancelled() => Err(execution_held()),
                result = self.command_delegate(sandbox_mode).execute_authorized(
                    script,
                    sandbox_mode,
                    network_access,
                    mode,
                    output,
                    authorization,
                ) => result,
            }
        })
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const WORKSPACE_GIT_TEST_CHILD: &str = "MOBIUS_GATEWAY_WORKSPACE_GIT_TEST_CHILD";
    const WORKSPACE_GIT_TEST_NAME: &str =
        "sandbox::tests::workspace_git_inherits_home_config_and_ignores_repository_redirects";

    #[tokio::test]
    async fn isolated_backend_keeps_its_own_desktop_use_until_the_last_cleanup_reference_drops() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let remote =
            std::sync::Arc::new(crate::computer_runtime::remote_desktop::RemoteDesktop::new(
                state.path(),
                true,
                crate::computer_runtime::ComputerConfig::default(),
            ));
        let root =
            GatewaySandbox::new(workspace.path(), state.path(), None, Duration::from_secs(5))
                .unwrap()
                .with_remote_desktop(std::sync::Arc::clone(&remote));
        root.retain_desktop_use_for_test().await;
        let child = std::sync::Arc::new(root.isolated_backend().unwrap());
        assert!(child.desktop_use.lock().await.is_none());
        child.retain_desktop_use_for_test().await;
        assert_eq!(remote.consumer_count(), 2);
        root.release_desktop_use().await;
        let cleanup = std::sync::Arc::clone(&child);
        drop(child);
        assert_eq!(remote.consumer_count(), 1);
        drop(cleanup);
        assert_eq!(remote.consumer_count(), 0);
    }

    #[test]
    fn every_provider_api_key_environment_is_hidden_from_commands() {
        assert_eq!(
            provider_credential_environment().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "ANTHROPIC_API_KEY",
                "DEEPSEEK_API_KEY",
                "MISTRAL_API_KEY",
                "MOONSHOT_API_KEY",
                "OPENAI_API_KEY",
                "OPENROUTER_API_KEY",
            ])
        );
    }

    #[tokio::test]
    async fn gateway_credentials_are_hidden_from_commands_and_workers() {
        const TEST: &str =
            "sandbox::tests::gateway_credentials_are_hidden_from_commands_and_workers";
        const BEARER: &str = "MOBIUS_GATEWAY_BEARER_TOKEN";
        const TELEMETRY: &str = "MOBIUS_GATEWAY_TELEMETRY_TOKEN";
        if std::env::var(BEARER).as_deref() != Ok("synthetic-admission-secret")
            || std::env::var(TELEMETRY).as_deref() != Ok("synthetic-telemetry-secret")
        {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([TEST, "--exact"])
                .env(BEARER, "synthetic-admission-secret")
                .env(TELEMETRY, "synthetic-telemetry-secret")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let sandbox =
            GatewaySandbox::new(workspace.path(), state.path(), None, Duration::from_secs(5))
                .unwrap();
        for mode in [SandboxMode::WorkspaceWrite, SandboxMode::DangerFullAccess] {
            for command_mode in [CommandMode::Foreground, CommandMode::Background] {
                let output = sandbox
                .execute(
                    r#"printf '%s' "${MOBIUS_GATEWAY_BEARER_TOKEN+present}${MOBIUS_GATEWAY_TELEMETRY_TOKEN+present}""#,
                    mode,
                    NetworkAccess::Denied,
                    command_mode,
                    CommandOutputSink::default(),
                )
                .await
                .unwrap();
                assert_eq!(output.exit_code, 0, "{}", output.stderr);
                assert!(
                    output.stdout.is_empty(),
                    "gateway credential reached a tool process"
                );
            }
        }
    }

    #[test]
    fn automatic_git_arguments_pin_repository_execution_policy() {
        assert_eq!(
            GIT_ARGUMENTS,
            [
                "--no-pager",
                "-c",
                "safe.bareRepository=explicit",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ]
        );
    }

    #[test]
    fn construction_rejects_both_state_workspace_overlap_directions() {
        let workspace_parent = tempfile::tempdir().expect("workspace parent");
        let state_inside = workspace_parent.path().join("state");
        std::fs::create_dir(&state_inside).expect("nested state");
        let state_parent = tempfile::tempdir().expect("state parent");
        let workspace_inside = state_parent.path().join("workspace");
        std::fs::create_dir(&workspace_inside).expect("nested workspace");

        let state_inside_error = match GatewaySandbox::new(
            workspace_parent.path(),
            &state_inside,
            None,
            Duration::from_secs(5),
        ) {
            Ok(_) => panic!("state inside workspace must fail"),
            Err(error) => error,
        };
        let workspace_inside_error = match GatewaySandbox::new(
            &workspace_inside,
            state_parent.path(),
            None,
            Duration::from_secs(5),
        ) {
            Ok(_) => panic!("workspace inside state must fail"),
            Err(error) => error,
        };

        assert!(state_inside_error.to_string().contains("must not overlap"));
        assert!(
            workspace_inside_error
                .to_string()
                .contains("must not overlap")
        );
    }

    #[test]
    fn construction_rejects_a_tls_key_inside_the_chat_workspace() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("state");
        let private_key = workspace.path().join("private-key.pem");
        std::fs::write(&private_key, "private key").expect("private key");

        let error = match GatewaySandbox::new(
            workspace.path(),
            state.path(),
            Some(&private_key),
            Duration::from_secs(5),
        ) {
            Ok(_) => panic!("workspace TLS key must fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("outside every chat workspace"));
    }

    #[test]
    fn gateway_state_cannot_be_added_as_a_read_root() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("state");
        let sandbox =
            GatewaySandbox::new(workspace.path(), state.path(), None, Duration::from_secs(5))
                .expect("gateway sandbox");

        let result = sandbox.allow_read_roots([state.path().to_path_buf()]);

        assert!(result.is_err());
    }

    #[test]
    fn gateway_state_cannot_be_added_as_an_attached_folder() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("state");
        let sandbox =
            GatewaySandbox::new(workspace.path(), state.path(), None, Duration::from_secs(5))
                .expect("gateway sandbox");

        let result = sandbox.allow_attached_folders([state.path().to_path_buf()]);

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn configured_read_roots_allow_absolute_file_reads() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("state");
        let resources = tempfile::tempdir().expect("resources");
        let resource = resources.path().join("SKILL.md");
        std::fs::write(&resource, "instructions").expect("resource");
        let resource = std::fs::canonicalize(resource).expect("canonical resource");
        let sandbox =
            GatewaySandbox::new(workspace.path(), state.path(), None, Duration::from_secs(5))
                .expect("gateway sandbox")
                .allow_read_roots([resources.path().to_path_buf()])
                .expect("read root");

        let content = sandbox
            .read(
                resource.to_str().expect("UTF-8 resource path"),
                SandboxMode::WorkspaceWrite,
            )
            .await
            .expect("resource read");

        assert_eq!(content, "instructions");
    }

    #[tokio::test]
    async fn protected_commands_cannot_read_gateway_state_or_tls_key() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("state");
        let credentials = tempfile::tempdir().expect("credentials");
        let outside = tempfile::tempdir().expect("outside");
        let tls_key = credentials.path().join("private-key.pem");
        let initialized = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(workspace.path())
            .status()
            .expect("initialize Git repository");
        assert!(initialized.success());
        std::fs::write(state.path().join("sentinel"), "gateway-secret").expect("state sentinel");
        std::fs::write(&tls_key, "tls-secret").expect("TLS key");
        symlink(state.path(), workspace.path().join("state-link")).expect("state symlink");
        symlink(&tls_key, workspace.path().join("tls-link")).expect("TLS key symlink");
        let sandbox = GatewaySandbox::new(
            workspace.path(),
            state.path(),
            Some(&tls_key),
            Duration::from_secs(5),
        )
        .expect("gateway sandbox");

        for (label, mode, network_access) in [
            ("foreground", CommandMode::Foreground, NetworkAccess::Denied),
            (
                "background",
                CommandMode::Background,
                NetworkAccess::Allowed,
            ),
        ] {
            let outside_target = outside.path().join(label);
            let script = format!(
                "touch .git/{label}; touch {} || true; cat {}/sentinel || true; cat {} || true; cat state-link/sentinel || true; cat tls-link || true; printf changed > {}/sentinel || true; printf changed > {} || true; printf changed > state-link/sentinel || true; printf changed > tls-link || true; kill -0 {} && printf gateway-process-visible || true",
                outside_target.display(),
                state.path().display(),
                tls_key.display(),
                state.path().display(),
                tls_key.display(),
                std::process::id()
            );
            let output = sandbox
                .execute(
                    &script,
                    SandboxMode::WorkspaceWrite,
                    network_access,
                    mode,
                    CommandOutputSink::default(),
                )
                .await
                .expect("sandboxed command");

            assert_eq!(output.exit_code, 0, "{}", output.stderr);
            assert!(workspace.path().join(".git").join(label).is_file());
            assert!(!output.stdout.contains("gateway-secret"));
            assert!(!output.stdout.contains("tls-secret"));
            assert!(!output.stdout.contains("gateway-process-visible"));
            assert!(!outside_target.is_file());
            assert_eq!(
                std::fs::read_to_string(state.path().join("sentinel")).expect("state sentinel"),
                "gateway-secret"
            );
            assert_eq!(
                std::fs::read_to_string(&tls_key).expect("TLS key"),
                "tls-secret"
            );
        }
    }

    #[tokio::test]
    async fn full_access_commands_can_read_gateway_state_and_tls_key() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("state");
        let credentials = tempfile::tempdir().expect("credentials");
        let tls_key = credentials.path().join("private-key.pem");
        std::fs::write(state.path().join("sentinel"), "gateway-secret").expect("state sentinel");
        std::fs::write(&tls_key, "tls-secret").expect("TLS key");
        let sandbox = GatewaySandbox::new(
            workspace.path(),
            state.path(),
            Some(&tls_key),
            Duration::from_secs(5),
        )
        .expect("gateway sandbox");
        let script = format!(
            "printf '%s:%s' \"$(cat {}/sentinel)\" \"$(cat {})\"",
            state.path().display(),
            tls_key.display()
        );

        let output = sandbox
            .execute(
                &script,
                SandboxMode::DangerFullAccess,
                NetworkAccess::Allowed,
                CommandMode::Foreground,
                CommandOutputSink::default(),
            )
            .await
            .expect("full-access command");

        assert_eq!(output.exit_code, 0, "{}", output.stderr);
        assert_eq!(output.stdout, "gateway-secret:tls-secret");
    }

    #[tokio::test]
    async fn full_access_file_operations_use_the_host_wide_delegate() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("state");
        let outside = tempfile::tempdir().expect("outside");
        let secret = state.path().join("sentinel");
        let target = outside.path().join("written.txt");
        std::fs::write(&secret, "gateway-secret").expect("state sentinel");
        let sandbox =
            GatewaySandbox::new(workspace.path(), state.path(), None, Duration::from_secs(5))
                .expect("gateway sandbox");
        let secret = secret.to_str().expect("UTF-8 secret path");
        let target = target.to_str().expect("UTF-8 target path");

        assert!(
            sandbox
                .read(secret, SandboxMode::WorkspaceWrite)
                .await
                .is_err()
        );
        assert_eq!(
            sandbox
                .read(secret, SandboxMode::DangerFullAccess)
                .await
                .expect("full access read"),
            "gateway-secret"
        );
        assert!(
            sandbox
                .write(target, "blocked", SandboxMode::WorkspaceWrite)
                .await
                .is_err()
        );
        sandbox
            .write(target, "written", SandboxMode::DangerFullAccess)
            .await
            .expect("full access write");
        assert_eq!(
            std::fs::read_to_string(target).expect("outside file"),
            "written"
        );
    }

    #[tokio::test]
    async fn binary_reads_preserve_workspace_file_bytes() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("state");
        let expected = [0, 159, 255, 10];
        std::fs::write(workspace.path().join("report.bin"), expected).expect("binary file");
        let sandbox =
            GatewaySandbox::new(workspace.path(), state.path(), None, Duration::from_secs(5))
                .expect("gateway sandbox");

        let actual = sandbox
            .read_bytes("report.bin", expected.len(), SandboxMode::WorkspaceWrite)
            .await
            .expect("read binary file");

        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn commands_inherit_the_host_home() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("state");
        let sandbox =
            GatewaySandbox::new(workspace.path(), state.path(), None, Duration::from_secs(5))
                .expect("gateway sandbox");

        let output = sandbox
            .execute(
                r#"printf '%s' "$HOME""#,
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Denied,
                CommandMode::Foreground,
                CommandOutputSink::default(),
            )
            .await
            .expect("sandboxed command");

        assert_eq!(output.stdout, std::env::var("HOME").expect("host HOME"));
    }

    #[tokio::test]
    async fn workspace_git_inherits_home_config_and_ignores_repository_redirects() {
        if std::env::var_os(WORKSPACE_GIT_TEST_CHILD).is_none() {
            let workspace = tempfile::tempdir().expect("workspace");
            let state = tempfile::tempdir().expect("state");
            let redirected = tempfile::tempdir().expect("redirected repository");
            let home = workspace.path().join("home");
            std::fs::create_dir(&home).expect("home");
            for repository in [workspace.path(), redirected.path()] {
                let mut command = std::process::Command::new("git");
                command.args(["init", "--quiet"]).current_dir(repository);
                for name in REPOSITORY_LOCAL_GIT_ENVIRONMENT {
                    command.env_remove(name);
                }
                assert!(
                    command
                        .status()
                        .expect("initialize Git repository")
                        .success(),
                    "failed to initialize {}",
                    repository.display()
                );
            }
            std::fs::write(
                home.join(".gitconfig"),
                "[mobius]\n\tworkspaceMarker = inherited\n",
            )
            .expect("global Git config");

            let output = std::process::Command::new(
                std::env::current_exe().expect("locate gateway test binary"),
            )
            .args([WORKSPACE_GIT_TEST_NAME, "--exact", "--nocapture"])
            .env(WORKSPACE_GIT_TEST_CHILD, "1")
            .env("MOBIUS_GATEWAY_WORKSPACE_GIT_TEST_ROOT", workspace.path())
            .env("MOBIUS_GATEWAY_WORKSPACE_GIT_TEST_STATE", state.path())
            .env("HOME", home)
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("XDG_CONFIG_HOME")
            .env("GIT_DIR", redirected.path().join(".git"))
            .env("GIT_WORK_TREE", redirected.path())
            .env("GIT_INDEX_FILE", redirected.path().join(".git/index"))
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "mobius.workspaceMarker")
            .env("GIT_CONFIG_VALUE_0", "redirected")
            .output()
            .expect("run inherited Git environment test");
            assert!(
                output.status.success(),
                "child failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }

        let workspace = PathBuf::from(
            std::env::var_os("MOBIUS_GATEWAY_WORKSPACE_GIT_TEST_ROOT").expect("test workspace"),
        );
        let state = PathBuf::from(
            std::env::var_os("MOBIUS_GATEWAY_WORKSPACE_GIT_TEST_STATE").expect("test state"),
        );
        let sandbox = GatewaySandbox::new(&workspace, &state, None, Duration::from_secs(5))
            .expect("gateway sandbox");

        let configured = sandbox
            .execute_git(&["config", "--get", "mobius.workspaceMarker"])
            .await
            .expect("read inherited global Git config");
        assert_eq!(configured.exit_code, 0, "{}", configured.stderr);
        assert_eq!(configured.stdout.trim(), "inherited");

        let repository = sandbox
            .execute_git(&["rev-parse", "--show-toplevel"])
            .await
            .expect("resolve workspace repository");
        assert_eq!(repository.exit_code, 0, "{}", repository.stderr);
        assert_eq!(
            PathBuf::from(repository.stdout.trim()),
            std::fs::canonicalize(workspace).expect("canonical workspace")
        );
    }
}
