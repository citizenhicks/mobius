use std::path::{Component, Path};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mobius::backend::sandbox::{SandboxBackend as _, SandboxMode};

use crate::sandbox::GatewaySandbox;
use crate::wire::{WorkspaceFileRecord, WorkspaceFileScope};

use super::Rejection;

pub(super) const MAX_WORKSPACE_READ_BYTES: usize = 256 * 1024;
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
    if max_bytes == 0 || max_bytes > MAX_WORKSPACE_READ_BYTES {
        return Err(invalid(format!(
            "workspace read size must be 1–{MAX_WORKSPACE_READ_BYTES} bytes"
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
mod tests {
    use super::*;

    fn sandbox(workspace: &Path) -> (tempfile::TempDir, GatewaySandbox) {
        let state = tempfile::tempdir().expect("state");
        let sandbox = GatewaySandbox::new(workspace, state.path(), None, Duration::from_secs(5))
            .expect("gateway sandbox");
        (state, sandbox)
    }

    #[tokio::test]
    async fn read_opens_relative_and_absolute_paths_in_ranges() {
        let workspace = tempfile::tempdir().expect("workspace");
        let (_state, sandbox) = sandbox(workspace.path());
        let home = std::env::home_dir().expect("gateway home");
        let elsewhere = tempfile::tempdir_in(&home).expect("elsewhere");
        std::fs::write(workspace.path().join("main.rs"), b"fn main() {}").expect("file");
        std::fs::write(elsewhere.path().join("fetch_images.py"), b"print(1)").expect("file");
        let file = elsewhere
            .path()
            .join("fetch_images.py")
            .display()
            .to_string();
        let root = Some(workspace.path());

        let first = read(&sandbox, root, &file, 0, 5)
            .await
            .expect("first range");
        assert_eq!(
            (first.data.as_slice(), first.next_offset),
            (&b"print"[..], Some(5))
        );
        let rest = read(&sandbox, None, &file, 5, 64).await.expect("rest");
        assert_eq!(
            (rest.data.as_slice(), rest.next_offset),
            (&b"(1)"[..], None)
        );
        let relative = elsewhere.path().strip_prefix(&home).expect("home-relative");
        let home_path = format!("~/{}/fetch_images.py", relative.display());
        assert_eq!(
            read(&sandbox, None, &home_path, 0, 64)
                .await
                .expect("home path")
                .data,
            b"print(1)"
        );
        assert_eq!(
            read(&sandbox, root, "main.rs", 0, 64)
                .await
                .expect("relative")
                .data,
            b"fn main() {}"
        );
        assert!(read(&sandbox, None, "main.rs", 0, 64).await.is_err());
        assert!(read(&sandbox, root, "missing.rs", 0, 64).await.is_err());
        assert!(read(&sandbox, root, &file, 9, 64).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the sandbox serves every read in the test"
    )]
    async fn host_reads_follow_safe_links_and_reject_protected_paths_and_special_files() {
        use std::os::unix::fs::{OpenOptionsExt as _, symlink};

        let workspace = tempfile::tempdir().expect("workspace");
        let state = tempfile::tempdir().expect("state");
        let host = tempfile::tempdir().expect("host files");
        let tls = host.path().join("tls.key");
        let profile = host.path().join("profile");
        let bearer = host.path().join("bearer");
        std::fs::create_dir(&profile).expect("profile");
        let protected = [
            state.path().join("gateway.toml"),
            tls,
            profile.join("secret"),
            bearer,
        ];
        for path in &protected {
            std::fs::write(path, b"secret").expect("protected file");
        }
        let sandbox = GatewaySandbox::new(
            workspace.path(),
            state.path(),
            Some(&protected[1]),
            Duration::from_secs(5),
        )
        .expect("gateway sandbox")
        .deny_read_paths([profile, protected[3].clone()])
        .expect("protected paths");
        let root = Some(workspace.path());
        let public = host.path().join("public.txt");
        std::fs::write(&public, b"visible").expect("public file");
        symlink(&public, workspace.path().join("visible")).expect("safe link");
        assert_eq!(
            read(&sandbox, root, "visible", 0, 64)
                .await
                .expect("safe link")
                .data,
            b"visible"
        );
        for (index, path) in protected.iter().enumerate() {
            let link = format!("private-{index}");
            symlink(path, workspace.path().join(&link)).expect("private link");
            for requested in [path.to_str().expect("test path"), link.as_str()] {
                assert!(
                    read(&sandbox, root, requested, 0, 64).await.is_err(),
                    "{requested}"
                );
            }
        }
        let fifo = workspace.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("FIFO")
                .success()
        );
        let result =
            tokio::time::timeout(Duration::from_secs(2), read(&sandbox, root, "fifo", 0, 64)).await;
        if result.is_err() {
            // Unblock a regressed reader so the test runtime can still shut down.
            drop(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(nix::libc::O_NONBLOCK)
                    .open(&fifo),
            );
        }
        assert!(result.expect("FIFO must not block").is_err());
        assert!(read(&sandbox, root, ".", 0, 64).await.is_err());
    }

    #[tokio::test]
    async fn remove_refuses_traversal_git_directories_and_symlinks() {
        let workspace = tempfile::tempdir().unwrap();
        let (_state, sandbox) = sandbox(workspace.path());
        std::fs::create_dir(workspace.path().join("folder")).unwrap();
        std::fs::create_dir(workspace.path().join(".git")).unwrap();
        std::fs::write(workspace.path().join(".git/config"), b"config").unwrap();
        std::fs::write(workspace.path().join("file.txt"), b"data").unwrap();
        for path in ["../outside", ".git/config", "folder"] {
            assert!(remove(&sandbox, path).await.is_err(), "{path}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("file.txt", workspace.path().join("link")).unwrap();
            assert!(
                remove(&sandbox, "link")
                    .await
                    .unwrap_err()
                    .message
                    .contains("symbolic link")
            );
            assert!(workspace.path().join("link").symlink_metadata().is_ok());
        }
        assert!(
            remove(&sandbox, "folder")
                .await
                .unwrap_err()
                .message
                .contains("non-regular file")
        );
        remove(&sandbox, "file.txt").await.unwrap();
        assert!(!workspace.path().join("file.txt").exists());
        assert!(workspace.path().join(".git/config").exists());
    }

    #[tokio::test]
    async fn non_git_catalog_lists_files_without_following_symlinks_or_build_directories() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("source.txt"), b"source").expect("source");
        std::fs::create_dir(workspace.path().join("target")).expect("build directory");
        std::fs::write(workspace.path().join("target/artifact"), b"artifact").expect("artifact");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("private.txt"), b"private").expect("private");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), workspace.path().join("linked-directory"))
                .expect("directory symlink");
            std::os::unix::fs::symlink(
                outside.path().join("private.txt"),
                workspace.path().join("linked-file"),
            )
            .expect("file symlink");
        }
        let (_state, sandbox) = sandbox(workspace.path());

        let catalog = list(&sandbox, workspace.path(), WorkspaceFileScope::All)
            .await
            .expect("catalog");

        assert!(!catalog.truncated);
        assert_eq!(catalog.files.len(), 1);
        assert_eq!(
            (catalog.files[0].path.as_str(), catalog.files[0].size),
            ("source.txt", 6)
        );
        std::fs::remove_file(workspace.path().join("source.txt")).expect("remove source");
        std::fs::write(workspace.path().join("new.txt"), b"new").expect("new file");
        let refreshed = list(&sandbox, workspace.path(), WorkspaceFileScope::All)
            .await
            .expect("refreshed catalog");
        assert_eq!(refreshed.files[0].path, "new.txt");
        assert!(
            list(&sandbox, workspace.path(), WorkspaceFileScope::Modified)
                .await
                .expect("modified catalog")
                .files
                .is_empty()
        );
    }

    #[test]
    fn truncated_git_catalog_keeps_only_complete_records() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("first.txt"), b"file").expect("file");
        let mut output = "first.txt\0partial".to_owned();

        let truncated = retain_complete_git_paths(&mut output, true).expect("Git paths");
        let catalog =
            git_workspace_files(workspace.path(), &output, truncated).expect("workspace files");

        assert!(catalog.truncated);
        assert_eq!(catalog.files.len(), 1);
        assert_eq!(catalog.files[0].path, "first.txt");
    }
}
