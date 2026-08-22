use std::path::{Component, Path};
use std::process::Stdio;
use std::time::Duration;

use mobius::backend::sandbox::CommandOutput;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;
use url::Url;

use crate::sandbox::{GatewaySandbox, REPOSITORY_LOCAL_GIT_ENVIRONMENT};
use crate::wire::{GitDiffScope, GitStatus, MAX_FRAME_BYTES};

use super::Rejection;

// JSON can expand control bytes sixfold; one eighth leaves room for frame metadata.
const MAX_GIT_DIFF_BYTES: usize = MAX_FRAME_BYTES / 8;
const TRUNCATION_NOTE: &[u8] = b"[diff truncated]\n";
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CREDENTIAL_TARGET_BYTES: usize = 2 * 1024;
const MAX_CREDENTIAL_USERNAME_BYTES: usize = 512;
const MAX_CREDENTIAL_TOKEN_BYTES: usize = 16 * 1024;
const MAX_CREDENTIAL_OUTPUT_BYTES: usize = 32 * 1024;

pub(super) async fn probe_credential(
    target: &str,
) -> std::result::Result<Option<String>, Rejection> {
    let target = credential_target(target)?;
    credential_fill(&target, None).await
}

pub(super) async fn approve_credential(
    target: &str,
    username: &str,
    token: &str,
) -> std::result::Result<String, Rejection> {
    let target = credential_target(target)?;
    let username = credential_field(username, MAX_CREDENTIAL_USERNAME_BYTES, "username")?;
    let token = credential_field(token, MAX_CREDENTIAL_TOKEN_BYTES, "token")?;
    let input = credential_input(&target, Some(username), Some(token));
    if !run_credential("approve", &input).await? {
        return Err(credential_error(
            "the host Git credential helper rejected the credential",
        ));
    }
    credential_fill(&target, Some(username))
        .await?
        .ok_or_else(|| {
            credential_error("the host has no usable Git credential helper for this HTTPS target")
        })
}

async fn credential_fill(
    target: &str,
    username: Option<&str>,
) -> std::result::Result<Option<String>, Rejection> {
    let mut command = credential_command("fill");
    command.stdout(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| credential_error("the host Git command is unavailable"))?;
    let result = tokio::time::timeout(GIT_TIMEOUT, async {
        write_credential_input(&mut child, &credential_input(target, username, None)).await?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| credential_error("failed to read the host Git credential response"))?;
        let mut output = Vec::with_capacity(MAX_CREDENTIAL_OUTPUT_BYTES + 1);
        stdout
            .take((MAX_CREDENTIAL_OUTPUT_BYTES + 1) as u64)
            .read_to_end(&mut output)
            .await
            .map_err(|_| credential_error("failed to read the host Git credential response"))?;
        if output.len() > MAX_CREDENTIAL_OUTPUT_BYTES {
            output.fill(0);
            let _ = child.kill().await;
            return Err(credential_error(
                "the host Git credential helper returned invalid data",
            ));
        }
        let status = child
            .wait()
            .await
            .map_err(|_| credential_error("the host Git credential command failed"))?;
        if !status.success() {
            output.fill(0);
            return Ok(None);
        }
        let username = parse_credential_username(target, &output);
        output.fill(0);
        username.map(Some)
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            Err(timeout())
        }
    }
}

async fn run_credential(operation: &str, input: &[u8]) -> std::result::Result<bool, Rejection> {
    let mut command = credential_command(operation);
    command.stdout(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|_| credential_error("the host Git command is unavailable"))?;
    let result = tokio::time::timeout(GIT_TIMEOUT, async {
        write_credential_input(&mut child, input).await?;
        child
            .wait()
            .await
            .map_err(|_| credential_error("the host Git credential command failed"))
    })
    .await;
    match result {
        Ok(status) => Ok(status?.success()),
        Err(_) => {
            let _ = child.kill().await;
            Err(timeout())
        }
    }
}

fn credential_command(operation: &str) -> Command {
    let mut command = Command::new("git");
    command
        .args([
            "--no-pager",
            "-c",
            "safe.bareRepository=explicit",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "credential.interactive=never",
            "credential",
            operation,
        ])
        .current_dir("/")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/usr/bin/false")
        .env("SSH_ASKPASS", "/usr/bin/false")
        .env("GCM_INTERACTIVE", "Never")
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for name in REPOSITORY_LOCAL_GIT_ENVIRONMENT {
        command.env_remove(name);
    }
    command
}

async fn write_credential_input(
    child: &mut tokio::process::Child,
    input: &[u8],
) -> std::result::Result<(), Rejection> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| credential_error("failed to open the host Git credential input"))?;
    stdin
        .write_all(input)
        .await
        .map_err(|_| credential_error("failed to send the credential to host Git"))?;
    drop(stdin);
    Ok(())
}

fn credential_target(value: &str) -> std::result::Result<String, Rejection> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_CREDENTIAL_TARGET_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(invalid_credential("enter a valid HTTPS Git host or URL"));
    }
    let candidate = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    let url = Url::parse(&candidate)
        .map_err(|_| invalid_credential("enter a valid HTTPS Git host or URL"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_credential(
            "Git credentials require an HTTPS host or URL without embedded credentials, query, or fragment",
        ));
    }
    Ok(url.into())
}

fn credential_field<'a>(
    value: &'a str,
    max_bytes: usize,
    label: &str,
) -> std::result::Result<&'a str, Rejection> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(invalid_credential(format!("enter a valid Git {label}")));
    }
    Ok(value)
}

fn credential_input(target: &str, username: Option<&str>, token: Option<&str>) -> Vec<u8> {
    let mut input = format!("url={target}\n").into_bytes();
    if let Some(username) = username {
        input.extend_from_slice(b"username=");
        input.extend_from_slice(username.as_bytes());
        input.push(b'\n');
    }
    if let Some(token) = token {
        input.extend_from_slice(b"password=");
        input.extend_from_slice(token.as_bytes());
        input.push(b'\n');
    }
    input.push(b'\n');
    input
}

fn parse_credential_username(
    target: &str,
    output: &[u8],
) -> std::result::Result<String, Rejection> {
    if output.is_empty() || output.contains(&b'\0') || output.contains(&b'\r') {
        return Err(invalid_credential_output());
    }
    let record = if let Some(end) = output.windows(2).position(|bytes| bytes == b"\n\n") {
        if end + 2 != output.len() {
            return Err(invalid_credential_output());
        }
        &output[..end]
    } else {
        output.strip_suffix(b"\n").unwrap_or(output)
    };
    let target = Url::parse(target).map_err(|_| invalid_credential_output())?;
    let expected_host = match target.port() {
        Some(port) => format!(
            "{}:{port}",
            target.host_str().ok_or_else(invalid_credential_output)?
        ),
        None => target
            .host_str()
            .ok_or_else(invalid_credential_output)?
            .to_owned(),
    };
    let expected_path = target.path().trim_start_matches('/').as_bytes();
    let mut protocol = None;
    let mut host = None;
    let mut path = None;
    let mut username = None;
    let mut password = None;
    for line in record.split(|byte| *byte == b'\n') {
        let Some(separator) = line.iter().position(|byte| *byte == b'=') else {
            return Err(invalid_credential_output());
        };
        let (key, value) = (&line[..separator], &line[separator + 1..]);
        if key.is_empty() {
            return Err(invalid_credential_output());
        }
        match key {
            b"protocol" => set_credential_field(&mut protocol, value)?,
            b"host" => set_credential_field(&mut host, value)?,
            b"path" => set_credential_field(&mut path, value)?,
            b"username" => set_credential_field(&mut username, value)?,
            b"password" => set_credential_field(&mut password, value)?,
            b"url" | b"credential" => return Err(invalid_credential_output()),
            _ => {}
        }
    }
    if protocol != Some(b"https".as_slice())
        || host != Some(expected_host.as_bytes())
        || path.is_some_and(|path| path.strip_prefix(b"/").unwrap_or(path) != expected_path)
        || password.is_none_or(|password| {
            password.is_empty() || password.len() > MAX_CREDENTIAL_TOKEN_BYTES
        })
    {
        return Err(invalid_credential_output());
    }
    let username = username.ok_or_else(invalid_credential_output)?;
    if username.is_empty() || username.len() > MAX_CREDENTIAL_USERNAME_BYTES {
        return Err(invalid_credential_output());
    }
    let username = std::str::from_utf8(username).map_err(|_| invalid_credential_output())?;
    if username.chars().any(char::is_control) {
        return Err(invalid_credential_output());
    }
    Ok(username.to_owned())
}

fn set_credential_field<'a>(
    field: &mut Option<&'a [u8]>,
    value: &'a [u8],
) -> std::result::Result<(), Rejection> {
    if field.replace(value).is_some() {
        return Err(invalid_credential_output());
    }
    Ok(())
}

pub(super) async fn status(sandbox: &GatewaySandbox) -> Option<GitStatus> {
    tokio::time::timeout(GIT_TIMEOUT, status_inner(sandbox))
        .await
        .ok()?
        .ok()
}

pub(super) async fn switch_branch(
    sandbox: &GatewaySandbox,
    branch: &str,
) -> std::result::Result<(), Rejection> {
    tokio::time::timeout(GIT_TIMEOUT, switch_branch_inner(sandbox, branch))
        .await
        .map_err(|_| timeout())?
}

async fn status_inner(sandbox: &GatewaySandbox) -> std::result::Result<GitStatus, Rejection> {
    let (current, branches) = tokio::join!(
        output(sandbox, &["branch", "--show-current"]),
        output(
            sandbox,
            &["for-each-ref", "--format=%(refname)", "refs/heads/"]
        )
    );
    let current = successful_output(current?, "reading the current Git branch failed")?;
    let branches = successful_output(branches?, "listing local Git branches failed")?;
    let mut branches = String::from_utf8_lossy(&branches)
        .lines()
        .map(|branch| branch.strip_prefix("refs/heads/").map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(invalid_branch_output)?;
    branches.sort_unstable();
    branches.dedup();
    Ok(GitStatus {
        current_branch: String::from_utf8_lossy(&current).trim().into(),
        branches,
    })
}

async fn switch_branch_inner(
    sandbox: &GatewaySandbox,
    branch: &str,
) -> std::result::Result<(), Rejection> {
    if !status_inner(sandbox)
        .await?
        .branches
        .iter()
        .any(|candidate| candidate == branch)
    {
        return Err(unknown_branch());
    }
    let output = sandbox
        .switch_git_branch(branch)
        .await
        .map_err(error_rejection)?;
    if output.exit_code == 0 {
        Ok(())
    } else {
        Err(failure("switching Git branches failed", &output.stderr))
    }
}

pub(super) async fn diff(
    sandbox: &GatewaySandbox,
    workspace: &Path,
    scope: GitDiffScope,
) -> std::result::Result<String, Rejection> {
    tokio::time::timeout(GIT_TIMEOUT, diff_inner(sandbox, workspace, scope))
        .await
        .map_err(|_| timeout())?
}

async fn diff_inner(
    sandbox: &GatewaySandbox,
    workspace: &Path,
    scope: GitDiffScope,
) -> std::result::Result<String, Rejection> {
    let repository = output(sandbox, &["rev-parse", "--is-inside-work-tree"]).await?;
    if repository.exit_code != 0 {
        if repository.stderr.contains("not a git repository") {
            return Ok(String::new());
        }
        return Err(failure(
            "checking the Git workspace failed",
            &repository.stderr,
        ));
    }
    if repository.stdout != "true\n" {
        return Ok(String::new());
    }

    let mut diff = match scope {
        GitDiffScope::Staged => successful_output(
            output(
                sandbox,
                &[
                    "diff",
                    "--cached",
                    "--no-ext-diff",
                    "--no-color",
                    "--no-textconv",
                    "--",
                ],
            )
            .await?,
            "staged git diff failed",
        )?,
        GitDiffScope::Unstaged => successful_output(
            output(
                sandbox,
                &["diff", "--no-ext-diff", "--no-color", "--no-textconv", "--"],
            )
            .await?,
            "unstaged git diff failed",
        )?,
        GitDiffScope::Committed => {
            let head = output(sandbox, &["rev-parse", "--verify", "--quiet", "HEAD"]).await?;
            if head.exit_code != 0 {
                Vec::new()
            } else {
                successful_output(
                    output(
                        sandbox,
                        &[
                            "show",
                            "--format=",
                            "--no-ext-diff",
                            "--no-color",
                            "--no-textconv",
                            "HEAD",
                            "--",
                        ],
                    )
                    .await?,
                    "committed git diff failed",
                )?
            }
        }
    };

    if scope == GitDiffScope::Unstaged {
        append_untracked(sandbox, workspace, &mut diff).await?;
    }

    truncate_diff(&mut diff, MAX_GIT_DIFF_BYTES);
    Ok(String::from_utf8_lossy(&diff).into_owned())
}

async fn append_untracked(
    sandbox: &GatewaySandbox,
    workspace: &Path,
    diff: &mut Vec<u8>,
) -> std::result::Result<(), Rejection> {
    let untracked = successful_output(
        output(
            sandbox,
            &["ls-files", "--others", "--exclude-standard", "-z", "--"],
        )
        .await?,
        "listing untracked files failed",
    )?;
    for path in untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        if diff.len() >= MAX_GIT_DIFF_BYTES {
            break;
        }
        let path = std::str::from_utf8(path).map_err(|_| invalid_path())?;
        let relative = Path::new(path);
        if !safe_path(relative) {
            return Err(invalid_path());
        }
        let metadata = match tokio::fs::symlink_metadata(workspace.join(relative)).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error_rejection(error)),
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        let patch = untracked_diff(sandbox, path).await?;
        if !is_binary_diff(&patch) {
            append_diff(diff, &patch);
        }
    }
    Ok(())
}

async fn output(
    sandbox: &GatewaySandbox,
    args: &[&str],
) -> std::result::Result<CommandOutput, Rejection> {
    sandbox.execute_git(args).await.map_err(error_rejection)
}

async fn untracked_diff(
    sandbox: &GatewaySandbox,
    path: &str,
) -> std::result::Result<Vec<u8>, Rejection> {
    let output = output(
        sandbox,
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--no-textconv",
            "--no-index",
            "--",
            "/dev/null",
            path,
        ],
    )
    .await?;
    if matches!(output.exit_code, 0 | 1) {
        Ok(output.stdout.into_bytes())
    } else {
        Err(failure("untracked git diff failed", &output.stderr))
    }
}

fn successful_output(
    output: CommandOutput,
    failure_message: &str,
) -> std::result::Result<Vec<u8>, Rejection> {
    if output.exit_code == 0 {
        Ok(output.stdout.into_bytes())
    } else {
        Err(failure(failure_message, &output.stderr))
    }
}

fn append_diff(target: &mut Vec<u8>, patch: &[u8]) {
    if patch.is_empty() {
        return;
    }
    if !target.is_empty() && !target.ends_with(b"\n") {
        target.push(b'\n');
    }
    target.extend_from_slice(patch);
}

/// Keep an oversized diff on a whole-line boundary within the gateway frame budget.
fn truncate_diff(diff: &mut Vec<u8>, max_bytes: usize) {
    if diff.len() <= max_bytes {
        return;
    }
    let content_bytes = max_bytes.saturating_sub(TRUNCATION_NOTE.len());
    let cut = diff[..content_bytes]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    diff.truncate(cut);
    diff.extend_from_slice(TRUNCATION_NOTE);
}

fn safe_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn is_binary_diff(diff: &[u8]) -> bool {
    diff.split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"Binary files "))
}

fn error_rejection(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "git_error",
        message: error.to_string(),
        fatal: false,
    }
}

fn failure(prefix: &str, stderr: &str) -> Rejection {
    let detail = stderr.trim();
    Rejection {
        code: "git_error",
        message: if detail.is_empty() {
            prefix.into()
        } else {
            format!("{prefix}: {detail}")
        },
        fatal: false,
    }
}

fn timeout() -> Rejection {
    Rejection {
        code: "git_timeout",
        message: "Git operation exceeded 5 seconds".into(),
        fatal: false,
    }
}

fn invalid_credential(message: impl Into<String>) -> Rejection {
    Rejection {
        code: "invalid_git_credential",
        message: message.into(),
        fatal: false,
    }
}

fn credential_error(message: impl Into<String>) -> Rejection {
    Rejection {
        code: "git_credential_error",
        message: message.into(),
        fatal: false,
    }
}

fn invalid_credential_output() -> Rejection {
    credential_error("the host Git credential helper returned invalid data")
}

fn unknown_branch() -> Rejection {
    Rejection {
        code: "unknown_git_branch",
        message: "the requested Git branch is not a local branch".into(),
        fatal: false,
    }
}

fn invalid_branch_output() -> Rejection {
    Rejection {
        code: "git_error",
        message: "Git returned an invalid local branch".into(),
        fatal: false,
    }
}

fn invalid_path() -> Rejection {
    Rejection {
        code: "git_error",
        message: "Git returned an invalid untracked path".into(),
        fatal: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_credential_protocol_accepts_https_and_rejects_injection() {
        assert_eq!(
            credential_target("git.example.com/team/repo").expect("target"),
            "https://git.example.com/team/repo"
        );
        assert!(credential_target("http://git.example.com").is_err());
        assert!(credential_target("https://user:token@git.example.com").is_err());
        assert!(credential_target("git.example.com\npassword=leak").is_err());
        assert!(credential_field("name\npassword=leak", 512, "username").is_err());
        assert!(credential_field("name\twith-control", 512, "username").is_err());

        let input = credential_input("https://git.example.com/", Some("octo"), Some("token"));
        assert_eq!(
            input,
            b"url=https://git.example.com/\nusername=octo\npassword=token\n\n"
        );

        let mut output =
            b"protocol=https\nhost=git.example.com\nusername=octo\npassword=x\n\n".to_vec();
        let username = parse_credential_username("https://git.example.com/", &output)
            .expect("credential username");
        output.fill(0);
        assert_eq!(username, "octo");
        assert!(
            parse_credential_username(
                "https://git.example.com/",
                b"protocol=https\nhost=other.example.com\nusername=octo\npassword=x\n\n"
            )
            .is_err()
        );
    }

    fn run_git(workspace: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .env("LC_ALL", "C")
            .current_dir(workspace)
            .output()
            .expect("run Git");
        assert!(
            output.status.success(),
            "Git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn test_sandbox(workspace: &Path) -> (tempfile::TempDir, GatewaySandbox) {
        let state = tempfile::tempdir().expect("state");
        let sandbox =
            GatewaySandbox::new(workspace, state.path(), None, GIT_TIMEOUT).expect("Git sandbox");
        (state, sandbox)
    }

    fn initialize_repository(workspace: &Path, branch: &str) {
        run_git(workspace, &["init", "--quiet", "--initial-branch", branch]);
        run_git(
            workspace,
            &["config", "user.email", "mobius@example.invalid"],
        );
        run_git(workspace, &["config", "user.name", "möbius Test"]);
        run_git(workspace, &["config", "commit.gpgsign", "false"]);
        std::fs::write(workspace.join("tracked.txt"), branch).expect("tracked file");
        run_git(workspace, &["add", "--", "tracked.txt"]);
        run_git(workspace, &["commit", "--quiet", "-m", "initial"]);
    }

    #[tokio::test]
    async fn git_status_lists_sorted_local_branches() {
        let workspace = tempfile::tempdir().expect("workspace");
        initialize_repository(workspace.path(), "middle");
        run_git(workspace.path(), &["branch", "zeta"]);
        run_git(workspace.path(), &["branch", "Alpha"]);
        let (_state, sandbox) = test_sandbox(workspace.path());

        let status = status(&sandbox).await.expect("Git status");

        assert_eq!(status.branches, ["Alpha", "middle", "zeta"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn branch_switch_disables_hooks_and_confines_filters_to_the_workspace() {
        use std::os::unix::fs::PermissionsExt as _;

        let workspace = tempfile::tempdir().expect("workspace");
        initialize_repository(workspace.path(), "main");
        run_git(workspace.path(), &["switch", "--quiet", "-c", "feature"]);
        run_git(
            workspace.path(),
            &[
                "config",
                "filter.mobius.smudge",
                "sh -c 'touch .agents/filter-ran .codex/filter-ran 2>/dev/null; cat'",
            ],
        );
        std::fs::write(
            workspace.path().join(".gitattributes"),
            "filtered.txt filter=mobius\n",
        )
        .expect("attributes");
        std::fs::write(workspace.path().join("filtered.txt"), "feature\n").expect("filtered file");
        run_git(workspace.path(), &["add", "--", "."]);
        run_git(workspace.path(), &["commit", "--quiet", "-m", "feature"]);
        run_git(workspace.path(), &["switch", "--quiet", "main"]);
        for directory in [".agents", ".codex"] {
            std::fs::create_dir(workspace.path().join(directory)).expect("workspace directory");
            std::fs::write(
                workspace.path().join(directory).join("sentinel"),
                "existing",
            )
            .expect("workspace sentinel");
        }
        let hook = workspace.path().join(".git/hooks/post-checkout");
        std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\n").expect("checkout hook");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
            .expect("hook permissions");
        let (_state, sandbox) = test_sandbox(workspace.path());

        switch_branch(&sandbox, "feature")
            .await
            .expect("switch branch");
        let status = status(&sandbox).await.expect("Git status");

        assert_eq!(
            (
                status.current_branch.as_str(),
                std::fs::read_to_string(workspace.path().join("filtered.txt"))
                    .expect("filtered file"),
                workspace.path().join("hook-ran").exists(),
                workspace.path().join(".agents/filter-ran").exists(),
                workspace.path().join(".codex/filter-ran").exists(),
            ),
            ("feature", "feature\n".into(), false, true, true)
        );
    }

    #[tokio::test]
    async fn branch_switch_rejects_names_outside_advertised_local_heads() {
        let workspace = tempfile::tempdir().expect("workspace");
        initialize_repository(workspace.path(), "main");
        run_git(workspace.path(), &["branch", "feature"]);
        let (_state, sandbox) = test_sandbox(workspace.path());

        let error = switch_branch(&sandbox, "feature ")
            .await
            .expect_err("unadvertised branch");

        assert_eq!(error.code, "unknown_git_branch");
    }

    #[tokio::test]
    async fn workspace_diff_keeps_staged_and_unstaged_scopes_separate() {
        let workspace = tempfile::tempdir().expect("workspace");
        let (_state, sandbox) = test_sandbox(workspace.path());
        run_git(workspace.path(), &["init", "--quiet"]);
        run_git(
            workspace.path(),
            &["config", "user.email", "mobius@example.invalid"],
        );
        run_git(workspace.path(), &["config", "user.name", "möbius Test"]);
        run_git(workspace.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(workspace.path().join("staged.txt"), "before\n").expect("staged file");
        std::fs::write(workspace.path().join("unstaged.txt"), "before\n").expect("unstaged file");
        std::fs::write(workspace.path().join(".gitignore"), "ignored.txt\n").expect("ignore file");
        run_git(workspace.path(), &["add", "--", "."]);
        run_git(workspace.path(), &["commit", "--quiet", "-m", "initial"]);

        std::fs::write(workspace.path().join("staged.txt"), "staged change\n")
            .expect("change staged file");
        run_git(workspace.path(), &["add", "--", "staged.txt"]);
        std::fs::write(workspace.path().join("unstaged.txt"), "unstaged change\n")
            .expect("change unstaged file");
        std::fs::write(workspace.path().join("new.txt"), "untracked content\n")
            .expect("untracked file");
        std::fs::write(workspace.path().join("ignored.txt"), "ignored\n").expect("ignored file");
        std::fs::write(workspace.path().join("binary.bin"), [0, 1, 2]).expect("binary file");

        let staged = diff(&sandbox, workspace.path(), GitDiffScope::Staged)
            .await
            .expect("staged diff");
        let unstaged = diff(&sandbox, workspace.path(), GitDiffScope::Unstaged)
            .await
            .expect("unstaged diff");

        assert!(
            staged.contains("+staged change")
                && !staged.contains("a/unstaged.txt")
                && unstaged.contains("+unstaged change")
                && unstaged.contains("+untracked content")
                && !unstaged.contains("a/staged.txt")
                && !unstaged.contains("ignored.txt")
                && !unstaged.contains("binary.bin"),
            "unexpected scoped diffs:\nstaged:\n{staged}\nunstaged:\n{unstaged}"
        );
    }

    #[tokio::test]
    async fn workspace_diff_is_empty_outside_a_git_repository() {
        let workspace = tempfile::tempdir().expect("workspace");
        let (_state, sandbox) = test_sandbox(workspace.path());

        let diff = diff(&sandbox, workspace.path(), GitDiffScope::Staged)
            .await
            .expect("non-Git workspace");

        assert!(diff.is_empty());
    }

    #[tokio::test]
    async fn committed_scope_returns_the_head_patch() {
        let workspace = tempfile::tempdir().expect("workspace");
        initialize_repository(workspace.path(), "main");
        let (_state, sandbox) = test_sandbox(workspace.path());

        let diff = diff(&sandbox, workspace.path(), GitDiffScope::Committed)
            .await
            .expect("committed diff");

        assert!(diff.contains("diff --git a/tracked.txt b/tracked.txt"));
    }

    #[test]
    fn workspace_diff_truncates_oversized_output_at_a_line_boundary() {
        let mut diff = b"header\nfirst line\nsecond line\n".to_vec();

        truncate_diff(&mut diff, 25);

        assert_eq!(diff, b"header\n[diff truncated]\n");
    }

    #[test]
    fn workspace_diff_budget_fits_the_encoded_gateway_frame() {
        use crate::wire::{ServerFrame, ServerMessage};

        let mut diff = vec![1; MAX_GIT_DIFF_BYTES + 1];
        truncate_diff(&mut diff, MAX_GIT_DIFF_BYTES);
        let frame = ServerFrame::new(ServerMessage::GitDiff {
            request_id: "r".repeat(4 * 1024),
            session_id: "s".repeat(4 * 1024),
            scope: GitDiffScope::Unstaged,
            diff: String::from_utf8(diff).expect("control bytes are valid UTF-8"),
        });

        assert!(serde_json::to_vec(&frame).expect("encoded frame").len() <= MAX_FRAME_BYTES);
    }
}
