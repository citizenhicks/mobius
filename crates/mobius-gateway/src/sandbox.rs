//! Gateway sandbox with protected and host-wide command modes.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(target_os = "linux")]
use std::ffi::OsStr;

use mobius::backend::model::provider::{ProviderAuth, providers};
use mobius::backend::sandbox::{
    CommandAuthorization, CommandMode, CommandOutput, CommandOutputSink, NetworkAccess,
    SandboxBackend, SandboxMode, local::LocalSandbox,
};
use mobius::{BoxFuture, Error, Result};

const GIT_ENVIRONMENT: [(&str, &str); 4] = [
    ("GIT_NO_LAZY_FETCH", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("LC_ALL", "C"),
];
// These variables can redirect Git to an untrusted repository or inject command-scoped settings.
pub(crate) const REPOSITORY_LOCAL_GIT_ENVIRONMENT: [&str; 17] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];
const GIT_ARGUMENTS: [&str; 7] = [
    "--no-pager",
    "-c",
    "safe.bareRepository=explicit",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
];
const GATEWAY_CREDENTIAL_ENVIRONMENT: [&str; 3] =
    ["MOBIUS_GATEWAY_TOKEN", "TUNNEL_TOKEN", "TUNNEL_TOKEN_FILE"];
#[cfg(target_os = "linux")]
const SANDBOX_PROC_ENVIRONMENT: &str = "MOBIUS_GATEWAY_SANDBOX_PROC";

#[cfg(target_os = "linux")]
fn empty_proc_configured(value: Option<&OsStr>) -> Result<bool> {
    match value {
        None => Ok(false),
        Some(value) if value == "private" => Ok(false),
        Some(value) if value == "empty" => Ok(true),
        Some(_) => Err(Error::Config(format!(
            "{SANDBOX_PROC_ENVIRONMENT} must be `private` or `empty`"
        ))),
    }
}

fn provider_credential_environment() -> impl Iterator<Item = &'static str> {
    providers()
        .iter()
        .filter_map(|provider| match provider.auth() {
            ProviderAuth::ApiKey(environment) => Some(environment),
            ProviderAuth::Browser(_) => None,
        })
}

/// Workspace backend that protects gateway state outside full-access commands.
pub struct GatewaySandbox {
    delegate: LocalSandbox,
    full_access_delegate: LocalSandbox,
}

impl GatewaySandbox {
    /// Creates protected and full-access command delegates for a gateway host.
    pub fn new(
        workspace: &Path,
        state_dir: &Path,
        tls_key: Option<&Path>,
        timeout: Duration,
    ) -> Result<Self> {
        if timeout.is_zero() {
            return Err(Error::Config("command timeout must be positive".into()));
        }
        let root = std::fs::canonicalize(workspace)?;
        let state_dir = std::fs::canonicalize(state_dir)?;
        let tls_key = match tls_key {
            Some(path) => std::fs::canonicalize(path)?,
            None => state_dir.clone(),
        };
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
            .deny_read(&tls_key)?;
        let mut full_access_delegate = LocalSandbox::new(&root)?
            .command_timeout(timeout)?
            .share_temporary_directory(&delegate)?;
        #[cfg(target_os = "linux")]
        if empty_proc_configured(std::env::var_os(SANDBOX_PROC_ENVIRONMENT).as_deref())? {
            delegate = delegate.empty_proc();
            full_access_delegate = full_access_delegate.empty_proc();
        }
        for environment in GATEWAY_CREDENTIAL_ENVIRONMENT
            .into_iter()
            .chain(provider_credential_environment())
        {
            delegate = delegate.deny_environment(environment);
            full_access_delegate = full_access_delegate.deny_environment(environment);
        }
        Ok(Self {
            delegate,
            full_access_delegate,
        })
    }

    fn command_delegate(&self, sandbox_mode: SandboxMode) -> &LocalSandbox {
        match sandbox_mode {
            SandboxMode::WorkspaceWrite => &self.delegate,
            SandboxMode::DangerFullAccess => &self.full_access_delegate,
        }
    }

    pub(crate) fn allow_read_roots(
        mut self,
        roots: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self> {
        for root in roots {
            self.delegate = self.delegate.allow_read_root(root)?;
        }
        Ok(self)
    }

    pub(crate) fn allow_attached_folders(
        mut self,
        roots: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self> {
        for root in roots {
            self.delegate = self.delegate.allow_workspace_root(root)?;
        }
        Ok(self)
    }

    pub(crate) async fn execute_git(&self, args: &[&str]) -> Result<CommandOutput> {
        let mut arguments = GIT_ARGUMENTS.to_vec();
        arguments.extend_from_slice(args);
        self.delegate
            .execute_read_only_with_environment_removals(
                "git",
                &arguments,
                &GIT_ENVIRONMENT,
                &REPOSITORY_LOCAL_GIT_ENVIRONMENT,
            )
            .await
    }

    pub(crate) async fn read_workspace_range(
        &self,
        path: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<(Vec<u8>, Option<u64>)> {
        self.delegate.read_range(path, offset, max_bytes).await
    }

    pub(crate) async fn switch_git_branch(&self, branch: &str) -> Result<CommandOutput> {
        let mut arguments = GIT_ARGUMENTS.to_vec();
        arguments.extend_from_slice(&[
            "switch",
            "--no-guess",
            "--no-recurse-submodules",
            "--",
            branch,
        ]);
        self.delegate
            .execute_git_mutation_with_environment_removals(
                &arguments,
                &GIT_ENVIRONMENT,
                &REPOSITORY_LOCAL_GIT_ENVIRONMENT,
            )
            .await
    }
}

impl SandboxBackend for GatewaySandbox {
    fn start_worker(
        &self,
        command: &mobius::backend::sandbox::WorkerCommand,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
    ) -> Result<mobius::backend::sandbox::WorkerProcess> {
        self.command_delegate(sandbox_mode)
            .start_worker(command, sandbox_mode, network_access)
    }

    fn isolated_execution(&self) -> Result<std::sync::Arc<dyn SandboxBackend>> {
        let delegate = self.delegate.isolated_execution()?;
        let full_access_delegate = self
            .full_access_delegate
            .isolated_execution()?
            .share_temporary_directory(&delegate)?;
        Ok(std::sync::Arc::new(Self {
            delegate,
            full_access_delegate,
        }))
    }

    fn temporary_directory(&self) -> Option<PathBuf> {
        Some(self.delegate.temporary_directory().into())
    }

    fn read<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<String>> {
        self.delegate.read(path)
    }

    fn read_bytes<'a>(&'a self, path: &'a str, max_bytes: usize) -> BoxFuture<'a, Result<Vec<u8>>> {
        self.delegate.read_bytes(path, max_bytes)
    }

    fn write<'a>(&'a self, path: &'a str, content: &'a str) -> BoxFuture<'a, Result<()>> {
        self.delegate.write(path, content)
    }

    fn execute<'a>(
        &'a self,
        script: &'a str,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
        mode: CommandMode,
        output: CommandOutputSink,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        self.command_delegate(sandbox_mode).execute(
            script,
            sandbox_mode,
            network_access,
            mode,
            output,
        )
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
        self.command_delegate(sandbox_mode).execute_authorized(
            script,
            sandbox_mode,
            network_access,
            mode,
            output,
            authorization,
        )
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const WORKSPACE_GIT_TEST_CHILD: &str = "MOBIUS_GATEWAY_WORKSPACE_GIT_TEST_CHILD";
    const WORKSPACE_GIT_TEST_NAME: &str =
        "sandbox::tests::workspace_git_inherits_home_config_and_ignores_repository_redirects";

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_proc_environment_accepts_only_private_or_empty() {
        use std::os::unix::ffi::OsStrExt as _;

        assert!(!empty_proc_configured(None).expect("default procfs"));
        assert!(!empty_proc_configured(Some(OsStr::new("private"))).expect("private procfs"));
        assert!(empty_proc_configured(Some(OsStr::new("empty"))).expect("empty proc"));
        for invalid in [
            OsStr::new(""),
            OsStr::new("hidden"),
            OsStr::from_bytes(&[0xff]),
        ] {
            let error = empty_proc_configured(Some(invalid)).expect_err("invalid proc mode");
            assert!(error.to_string().contains(SANDBOX_PROC_ENVIRONMENT));
        }
    }

    #[test]
    fn every_provider_api_key_environment_is_hidden_from_commands() {
        assert_eq!(
            provider_credential_environment().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "ANTHROPIC_API_KEY",
                "DEEPSEEK_API_KEY",
                "MOONSHOT_API_KEY",
                "OPENAI_API_KEY",
                "OPENROUTER_API_KEY",
            ])
        );
    }

    #[test]
    fn gateway_bearer_environment_is_hidden_from_commands() {
        assert_eq!(
            GATEWAY_CREDENTIAL_ENVIRONMENT,
            ["MOBIUS_GATEWAY_TOKEN", "TUNNEL_TOKEN", "TUNNEL_TOKEN_FILE"]
        );
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
            .read(resource.to_str().expect("UTF-8 resource path"))
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
    async fn binary_reads_preserve_workspace_file_bytes() {
        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("state");
        let expected = [0, 159, 255, 10];
        std::fs::write(workspace.path().join("report.bin"), expected).expect("binary file");
        let sandbox =
            GatewaySandbox::new(workspace.path(), state.path(), None, Duration::from_secs(5))
                .expect("gateway sandbox");

        let actual = sandbox
            .read_bytes("report.bin", expected.len())
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
