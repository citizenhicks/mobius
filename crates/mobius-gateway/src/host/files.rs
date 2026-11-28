use std::path::{Component, Path};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mobius::backend::sandbox::{SandboxBackend as _, SandboxMode};

use crate::sandbox::GatewaySandbox;
use crate::wire::{WorkspaceFileRecord, WorkspaceFileScope};

use super::Rejection;

const MAX_WORKSPACE_WRITE_BYTES: usize = 1024 * 1024;
const MAX_WORKSPACE_FILES: usize = 20_000;
const MAX_WORKSPACE_PATH_BYTES: usize = 1024 * 1024;
const MAX_WORKSPACE_DEPTH: usize = 64;
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
        .map_err(|elapsed| {
            let mut rejection = timeout();
            rejection.message = format!("{}: {elapsed}", rejection.message).into();
            rejection
        })?
}

/// Reads host files outside protected gateway paths, relative to the chat's folder.
pub(super) async fn read(
    sandbox: &GatewaySandbox,
    workspace: Option<&Path>,
    path: &str,
    offset: u64,
    max_bytes: usize,
) -> std::result::Result<WorkspaceRead, Rejection> {
    use crate::wire::MAX_READ_CHUNK_BYTES;
    if max_bytes == 0 || max_bytes > MAX_READ_CHUNK_BYTES {
        return Err(invalid(format!(
            "workspace read size must be 1–{MAX_READ_CHUNK_BYTES} bytes"
        )));
    }
    let requested = Path::new(path);
    let resolved = match (requested.strip_prefix("~"), workspace) {
        (Ok(relative), _) => std::env::home_dir()
            .ok_or_else(|| invalid("the gateway's home directory is unavailable"))?
            .join(relative),
        _ if requested.is_absolute() => requested.to_path_buf(),
        (_, Some(workspace)) => workspace.join(requested),
        (_, None) => {
            return Err(invalid(
                "a relative path needs a chat with a project folder",
            ));
        }
    };
    let (data, next_offset) = sandbox
        .read_workspace_range(resolved, offset, max_bytes)
        .await
        .map_err(|error| invalid(format!("couldn’t read {path}: {error}")))?;
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
    sandbox
        .write(path, content, SandboxMode::WorkspaceWrite)
        .await
        .map_err(error_rejection)
}

pub(super) async fn remove(
    sandbox: &GatewaySandbox,
    path: &str,
) -> std::result::Result<(), Rejection> {
    validate_relative(path)?;
    sandbox.remove(path).await.map_err(error_rejection)
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
    let (output, staged) = if scope == WorkspaceFileScope::Modified {
        let (output, staged) = tokio::join!(
            sandbox.execute_git(args),
            sandbox.execute_git(&[
                "diff",
                "--cached",
                "--name-only",
                "-z",
                "--relative",
                "--no-ext-diff",
                "--no-renames",
                "--",
            ])
        );
        (output, Some(staged))
    } else {
        (sandbox.execute_git(args).await, None)
    };
    let mut output = output.map_err(error_rejection)?;
    if output.exit_code == 0 {
        let mut truncated = retain_complete_git_paths(&mut output.stdout, output.stdout_truncated)?;
        if let Some(staged) = staged {
            let mut staged = staged.map_err(error_rejection)?;
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
    let workspace = workspace.to_path_buf();
    tokio::task::spawn_blocking(move || non_git_workspace_files(&workspace))
        .await
        .map_err(error_rejection)?
}

fn non_git_workspace_files(workspace: &Path) -> std::result::Result<WorkspaceFiles, Rejection> {
    let started = Instant::now();
    let workspace = std::fs::canonicalize(workspace).map_err(error_rejection)?;
    let mut directories = vec![(std::borrow::Cow::Borrowed(workspace.as_path()), 0)];
    let mut catalog = WorkspaceFiles {
        files: Vec::new(),
        truncated: false,
    };
    let mut remaining_entries = MAX_WORKSPACE_FILES;
    let mut path_bytes = 0;
    'walk: while let Some((directory, depth)) = directories.pop() {
        if std::fs::canonicalize(&directory).ok().as_deref() != Some(directory.as_ref()) {
            continue;
        }
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if directory == workspace.as_path() => return Err(error_rejection(error)),
            Err(_) => continue,
        };
        let mut entries = entries
            .take(remaining_entries + 1)
            .filter_map(std::result::Result::ok)
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            if started.elapsed() >= FILE_TIMEOUT {
                return Err(timeout());
            }
            if remaining_entries == 0 {
                catalog.truncated = true;
                break 'walk;
            }
            remaining_entries -= 1;
            if matches!(entry.file_name().to_str(), Some(".git" | "target")) {
                continue;
            }
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                if depth < MAX_WORKSPACE_DEPTH {
                    directories.push((std::borrow::Cow::Owned(path), depth + 1));
                } else {
                    catalog.truncated = true;
                }
            } else if metadata.is_file() {
                let Some(relative) = path.strip_prefix(&workspace).ok().and_then(Path::to_str)
                else {
                    continue;
                };
                let relative = relative.replace(std::path::MAIN_SEPARATOR, "/");
                if validate_relative(&relative).is_err() {
                    continue;
                }
                path_bytes += relative.len();
                if path_bytes > MAX_WORKSPACE_PATH_BYTES {
                    catalog.truncated = true;
                    break 'walk;
                }
                catalog.files.push(WorkspaceFileRecord {
                    path: relative,
                    size: metadata.len(),
                });
            }
        }
    }
    catalog
        .files
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(catalog)
}

fn git_workspace_files(
    workspace: &Path,
    output: &str,
    mut truncated: bool,
) -> std::result::Result<WorkspaceFiles, Rejection> {
    validate_git_paths(output)?;
    let mut files = Vec::new();
    let mut path_bytes = 0_usize;
    let mut paths = output.split_terminator('\0').collect::<Vec<_>>();
    paths.sort_unstable();
    paths.dedup();
    for path in paths {
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

fn invalid(message: impl Into<Arc<str>>) -> Rejection {
    Rejection::new("invalid_workspace_file", message)
}

fn error_rejection(error: impl std::fmt::Display) -> Rejection {
    Rejection::new("workspace_file_error", error.to_string())
}

fn timeout() -> Rejection {
    Rejection::new(
        "workspace_file_timeout",
        format!(
            "workspace file operation exceeded {} seconds",
            FILE_TIMEOUT.as_secs()
        ),
    )
}

#[cfg(test)]
mod tests;
