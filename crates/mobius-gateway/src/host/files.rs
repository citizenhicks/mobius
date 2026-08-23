use std::collections::BTreeSet;
use std::path::{Component, Path};
use std::time::Duration;

use mobius::backend::sandbox::SandboxBackend as _;

use crate::sandbox::GatewaySandbox;
use crate::wire::{WorkspaceFileRecord, WorkspaceFileScope};

use super::Rejection;

pub(super) const MAX_WORKSPACE_READ_BYTES: usize = 256 * 1024;
const MAX_WORKSPACE_WRITE_BYTES: usize = 1024 * 1024;
const MAX_WORKSPACE_FILES: usize = 20_000;
const MAX_WORKSPACE_PATH_BYTES: usize = 1024 * 1024;
const FILE_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct WorkspaceRead {
    pub(crate) data: Vec<u8>,
    pub(crate) next_offset: Option<u64>,
}

pub(crate) struct WorkspaceFiles {
    pub(crate) files: Vec<WorkspaceFileRecord>,
    pub(crate) truncated: bool,
}

pub(super) async fn list(
    sandbox: &GatewaySandbox,
    workspace: &Path,
    scope: WorkspaceFileScope,
) -> std::result::Result<WorkspaceFiles, Rejection> {
    tokio::time::timeout(FILE_TIMEOUT, list_inner(sandbox, workspace, scope))
        .await
        .map_err(|_| timeout())?
}

pub(super) async fn read(
    sandbox: &GatewaySandbox,
    path: &str,
    offset: u64,
    max_bytes: usize,
) -> std::result::Result<WorkspaceRead, Rejection> {
    if max_bytes == 0 || max_bytes > MAX_WORKSPACE_READ_BYTES {
        return Err(invalid(format!(
            "workspace read size must be 1–{MAX_WORKSPACE_READ_BYTES} bytes"
        )));
    }
    validate_relative(path)?;
    let (data, next_offset) = sandbox
        .read_workspace_range(path, offset, max_bytes)
        .await
        .map_err(error_rejection)?;
    Ok(WorkspaceRead { data, next_offset })
}

pub(super) async fn write(
    sandbox: &GatewaySandbox,
    path: &str,
    content: &str,
) -> std::result::Result<(), Rejection> {
    validate_relative(path)?;
    if content.len() > MAX_WORKSPACE_WRITE_BYTES {
        return Err(invalid(format!(
            "workspace text files are limited to {MAX_WORKSPACE_WRITE_BYTES} bytes"
        )));
    }
    sandbox.write(path, content).await.map_err(error_rejection)
}

async fn list_inner(
    sandbox: &GatewaySandbox,
    workspace: &Path,
    scope: WorkspaceFileScope,
) -> std::result::Result<WorkspaceFiles, Rejection> {
    let args = match scope {
        WorkspaceFileScope::Modified => &[
            "ls-files",
            "-z",
            "--modified",
            "--deleted",
            "--others",
            "--exclude-standard",
            "--",
        ][..],
        WorkspaceFileScope::All => &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
        ],
    };
    let mut output = sandbox.execute_git(args).await.map_err(error_rejection)?;
    if output.exit_code == 0 {
        let mut truncated = retain_complete_git_paths(&mut output.stdout, output.stdout_truncated)?;
        if scope == WorkspaceFileScope::Modified {
            let mut staged = sandbox
                .execute_git(&[
                    "diff",
                    "--cached",
                    "--name-only",
                    "-z",
                    "--relative",
                    "--no-ext-diff",
                    "--no-renames",
                    "--",
                ])
                .await
                .map_err(error_rejection)?;
            if staged.exit_code != 0 {
                return Err(error_rejection(format!(
                    "listing staged workspace files failed: {}",
                    staged.stderr.trim()
                )));
            }
            truncated |= retain_complete_git_paths(&mut staged.stdout, staged.stdout_truncated)?;
            output.stdout.push_str(&staged.stdout);
        }
        if scope == WorkspaceFileScope::All
            && !output
                .stdout
                .split_terminator('\0')
                .any(|path| path == ".env")
            && std::fs::symlink_metadata(workspace.join(".env"))
                .is_ok_and(|metadata| metadata.is_file() && !metadata.is_symlink())
        {
            output.stdout.push_str(".env\0");
        }
        let workspace = workspace.to_path_buf();
        return tokio::task::spawn_blocking(move || {
            git_workspace_files(&workspace, &output.stdout, truncated)
        })
        .await
        .map_err(error_rejection)?;
    }
    if !output.stderr.contains("not a git repository") {
        return Err(error_rejection(format!(
            "listing Git workspace files failed: {}",
            output.stderr.trim()
        )));
    }
    if scope == WorkspaceFileScope::Modified {
        return Ok(WorkspaceFiles {
            files: Vec::new(),
            truncated: false,
        });
    }
    Err(invalid(
        "all-files catalog requires a Git repository so ignore rules remain authoritative",
    ))
}

fn git_workspace_files(
    workspace: &Path,
    output: &str,
    mut truncated: bool,
) -> std::result::Result<WorkspaceFiles, Rejection> {
    validate_git_paths(output)?;
    let mut files = Vec::new();
    let mut path_bytes = 0_usize;
    for path in output.split_terminator('\0').collect::<BTreeSet<_>>() {
        let relative = validate_relative(path)?;
        let metadata = match std::fs::symlink_metadata(workspace.join(relative)) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error_rejection(error)),
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let next_path_bytes = path_bytes.saturating_add(path.len());
        if next_path_bytes > MAX_WORKSPACE_PATH_BYTES || files.len() == MAX_WORKSPACE_FILES {
            truncated = true;
            break;
        }
        path_bytes = next_path_bytes;
        files.push(WorkspaceFileRecord {
            path: path.into(),
            size: metadata.len(),
        });
    }
    Ok(WorkspaceFiles { files, truncated })
}

fn retain_complete_git_paths(
    output: &mut String,
    truncated: bool,
) -> std::result::Result<bool, Rejection> {
    if truncated {
        let complete = output.rfind('\0').map_or(0, |index| index + 1);
        output.truncate(complete);
    }
    validate_git_paths(output)?;
    Ok(truncated)
}

fn validate_git_paths(output: &str) -> std::result::Result<(), Rejection> {
    if !output.is_empty() && !output.ends_with('\0') {
        return Err(invalid(
            "Git workspace file output is truncated or malformed",
        ));
    }
    Ok(())
}

fn validate_relative(path: &str) -> std::result::Result<&Path, Rejection> {
    let relative = Path::new(path);
    let safe = relative.components().all(|component| {
        matches!(component, Component::Normal(value) if value != std::ffi::OsStr::new(".git"))
    });
    if path.is_empty() || path.len() > 4096 || !safe {
        return Err(invalid("workspace file path must be a safe relative path"));
    }
    Ok(relative)
}

fn invalid(message: impl Into<String>) -> Rejection {
    Rejection {
        code: "invalid_workspace_file",
        message: message.into(),
        fatal: false,
    }
}

fn error_rejection(error: impl std::fmt::Display) -> Rejection {
    Rejection {
        code: "workspace_file_error",
        message: error.to_string(),
        fatal: false,
    }
}

fn timeout() -> Rejection {
    Rejection {
        code: "workspace_file_timeout",
        message: format!(
            "workspace file operation exceeded {} seconds",
            FILE_TIMEOUT.as_secs()
        ),
        fatal: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(workspace: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(workspace)
            .status()
            .expect("run Git");
        assert!(status.success());
    }

    fn sandbox(workspace: &Path) -> (tempfile::TempDir, GatewaySandbox) {
        let state = tempfile::tempdir().expect("state");
        let sandbox = GatewaySandbox::new(workspace, state.path(), None, Duration::from_secs(5))
            .expect("gateway sandbox");
        (state, sandbox)
    }

    #[tokio::test]
    async fn read_rejects_parent_traversal() {
        let workspace = tempfile::tempdir().expect("workspace");
        let (_state, sandbox) = sandbox(workspace.path());

        let result = read(&sandbox, "../outside", 0, 16).await;

        assert!(matches!(
            result,
            Err(Rejection {
                code: "invalid_workspace_file",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn write_creates_and_atomically_replaces_workspace_text() {
        let workspace = tempfile::tempdir().expect("workspace");
        let (_state, sandbox) = sandbox(workspace.path());

        write(&sandbox, ".env", "TOKEN=first\n")
            .await
            .expect("create workspace file");
        write(&sandbox, ".env", "TOKEN=second\n")
            .await
            .expect("replace workspace file");

        assert_eq!(
            std::fs::read_to_string(workspace.path().join(".env")).expect("read workspace file"),
            "TOKEN=second\n"
        );
        assert!(write(&sandbox, "../outside", "secret").await.is_err());
    }

    #[tokio::test]
    async fn git_catalog_excludes_ignored_directories_and_git_internals() {
        let workspace = tempfile::tempdir().expect("workspace");
        git(workspace.path(), &["init", "--quiet"]);
        std::fs::write(workspace.path().join(".gitignore"), b"ignored/\n.env\n")
            .expect("ignore rules");
        std::fs::create_dir(workspace.path().join("ignored")).expect("ignored directory");
        std::fs::write(workspace.path().join("ignored/generated.txt"), b"ignored")
            .expect("ignored file");
        std::fs::write(workspace.path().join("included.txt"), b"included").expect("included file");
        std::fs::write(workspace.path().join(".env"), b"TOKEN=secret\n").expect("environment file");
        let (_state, sandbox) = sandbox(workspace.path());

        let catalog = list(&sandbox, workspace.path(), WorkspaceFileScope::All)
            .await
            .expect("workspace files");

        assert!(!catalog.truncated);
        assert!(catalog.files.iter().any(|file| file.path == ".env"));
        assert!(catalog.files.iter().any(|file| file.path == ".gitignore"));
        assert!(catalog.files.iter().any(|file| file.path == "included.txt"));
        assert!(
            catalog
                .files
                .iter()
                .all(|file| !file.path.starts_with("ignored/"))
        );
        assert!(
            catalog
                .files
                .iter()
                .all(|file| !file.path.starts_with(".git/"))
        );
        assert!(read(&sandbox, ".git/config", 0, 16).await.is_err());
    }

    #[tokio::test]
    async fn modified_catalog_includes_every_openable_uncommitted_file() {
        let workspace = tempfile::tempdir().expect("workspace");
        git(workspace.path(), &["init", "--quiet"]);
        std::fs::write(workspace.path().join(".gitignore"), b"ignored.txt\n")
            .expect("ignore rules");
        for path in ["clean.txt", "staged.txt", "unstaged.txt", "deleted.txt"] {
            std::fs::write(workspace.path().join(path), b"baseline").expect("baseline file");
        }
        git(workspace.path(), &["add", "."]);
        git(
            workspace.path(),
            &[
                "-c",
                "user.name=möbius Test",
                "-c",
                "user.email=mobius@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "baseline",
            ],
        );
        std::fs::write(workspace.path().join("staged.txt"), b"staged").expect("staged file");
        git(workspace.path(), &["add", "staged.txt"]);
        std::fs::write(workspace.path().join("staged.txt"), b"staged and unstaged")
            .expect("second staged file change");
        std::fs::write(workspace.path().join("unstaged.txt"), b"unstaged").expect("unstaged file");
        std::fs::write(workspace.path().join("untracked.txt"), b"untracked")
            .expect("untracked file");
        std::fs::write(workspace.path().join("ignored.txt"), b"ignored").expect("ignored file");
        std::fs::remove_file(workspace.path().join("deleted.txt")).expect("deleted file");
        let (_state, sandbox) = sandbox(workspace.path());

        let catalog = list(&sandbox, workspace.path(), WorkspaceFileScope::Modified)
            .await
            .expect("modified files");

        assert_eq!(
            catalog
                .files
                .into_iter()
                .map(|file| file.path)
                .collect::<Vec<_>>(),
            ["staged.txt", "unstaged.txt", "untracked.txt"]
        );
    }

    #[tokio::test]
    async fn modified_catalog_is_relative_to_a_nested_workspace() {
        // Bubblewrap intentionally masks host /tmp; keep the parent repository visible.
        let repository = tempfile::tempdir_in(std::env::current_dir().expect("current directory"))
            .expect("repository");
        let workspace = repository.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        git(repository.path(), &["init", "--quiet"]);
        std::fs::write(workspace.join("inside.txt"), b"baseline").expect("inside file");
        std::fs::write(repository.path().join("outside.txt"), b"baseline").expect("outside file");
        git(repository.path(), &["add", "."]);
        git(
            repository.path(),
            &[
                "-c",
                "user.name=möbius Test",
                "-c",
                "user.email=mobius@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "baseline",
            ],
        );
        std::fs::write(workspace.join("inside.txt"), b"modified").expect("inside change");
        std::fs::write(repository.path().join("outside.txt"), b"modified").expect("outside change");
        git(repository.path(), &["add", "."]);
        let (_state, sandbox) = sandbox(&workspace);

        let catalog = list(&sandbox, &workspace, WorkspaceFileScope::Modified)
            .await
            .expect("modified files");

        assert_eq!(catalog.files.len(), 1);
        assert_eq!(catalog.files[0].path, "inside.txt");
    }

    #[tokio::test]
    async fn non_git_all_catalog_does_not_bypass_ignore_rules() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join(".gitignore"), b"ignored.txt\n")
            .expect("ignore rules");
        std::fs::write(workspace.path().join("ignored.txt"), b"ignored").expect("ignored file");
        let (_state, sandbox) = sandbox(workspace.path());

        let result = list(&sandbox, workspace.path(), WorkspaceFileScope::All).await;

        assert!(matches!(
            result,
            Err(Rejection {
                code: "invalid_workspace_file",
                ..
            })
        ));
    }

    #[test]
    fn git_catalog_rejects_non_terminated_output() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("file.txt"), b"file").expect("file");

        assert!(git_workspace_files(workspace.path(), "file.txt", false).is_err());
    }

    #[test]
    fn each_git_catalog_stream_requires_a_terminator() {
        assert!(validate_git_paths("first\0").is_ok());
        assert!(validate_git_paths("truncated").is_err());
    }

    #[test]
    fn workspace_file_timeout_reports_the_configured_budget() {
        let rejection = timeout();

        assert_eq!(FILE_TIMEOUT, Duration::from_secs(30));
        assert_eq!(rejection.code, "workspace_file_timeout");
        assert_eq!(
            rejection.message,
            "workspace file operation exceeded 30 seconds"
        );
    }

    #[test]
    fn truncated_git_catalog_keeps_only_complete_records() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("first.txt"), b"file").expect("file");
        let mut output = "first.txt\0partial".to_string();

        let truncated = retain_complete_git_paths(&mut output, true).expect("Git paths");
        let catalog =
            git_workspace_files(workspace.path(), &output, truncated).expect("workspace files");

        assert!(catalog.truncated);
        assert_eq!(catalog.files.len(), 1);
        assert_eq!(catalog.files[0].path, "first.txt");
    }
}
