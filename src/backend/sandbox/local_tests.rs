//! Local sandbox tests.

use super::*;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use tokio::process::Command;

#[cfg(target_os = "macos")]
#[tokio::test]
async fn apple_git_launcher_resolves_to_the_trusted_tool_without_changing_other_executables() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = local_sandbox(workspace.path());
    let git = sandbox
        .resolve_argv_executable("/usr/bin/git".into(), (&[], &[]))
        .await
        .expect("resolve Apple Git launcher");
    assert_ne!(git, Path::new("/usr/bin/git"));
    let expected = Command::new("/usr/bin/xcrun")
        .args(["--find", "git"])
        .output()
        .await
        .expect("developer Git path");
    assert!(expected.status.success());
    let expected = String::from_utf8(expected.stdout).expect("Git path UTF-8");
    assert_eq!(git, std::fs::canonicalize(expected.trim()).expect("Git"));
    assert_eq!(
        sandbox
            .resolve_argv_executable("/bin/sh".into(), (&[], &[]))
            .await
            .expect("preserve selected executable"),
        Path::new("/bin/sh")
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn apple_git_resolution_does_not_bypass_protected_paths() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = local_sandbox(workspace.path());
    let git = sandbox
        .resolve_argv_executable("/usr/bin/git".into(), (&[], &[]))
        .await
        .expect("developer Git");
    let sandbox = sandbox.deny_read(&git).expect("protect developer Git");
    let resolved = sandbox
        .resolve_argv_executable("/usr/bin/git".into(), (&[], &[]))
        .await;
    assert!(!matches!(&resolved, Ok(path) if path == &git));
    // The first cold lookup can emit timestamped Xcode warnings even on success.
    let mut baseline = None;
    for _ in 0..2 {
        baseline = sandbox
            .execute_invocation(
                Invocation::Argv {
                    executable: Path::new("/usr/bin/git"),
                    arguments: &["--version"],
                    piped_input: false,
                },
                CommandIsolation {
                    sandbox_mode: SandboxMode::WorkspaceWrite,
                    network_access: NetworkAccess::Denied,
                    workspace_access: WorkspaceAccess::ReadOnly,
                },
                CommandMode::Foreground,
                CommandOutputSink::default(),
                (&[], &[]),
                None,
            )
            .await
            .expect("original launcher");
    }
    let baseline = baseline.expect("launcher output");
    let result = sandbox
        .execute_read_only("git", &["--version"], &[])
        .await
        .expect("resolved Git");
    assert_eq!(
        (
            result.exit_code,
            result.stdout,
            result.stdout_truncated,
            result.stderr,
            result.stderr_truncated,
        ),
        (
            baseline.exit_code,
            baseline.stdout,
            baseline.stdout_truncated,
            baseline.stderr,
            baseline.stderr_truncated,
        )
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn apple_git_lookup_uses_command_environment_overrides_and_removals() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = local_sandbox(workspace.path());
    let environment = [("DEVELOPER_DIR", "/nonexistent-mobius-developer-directory")];
    assert_eq!(
        sandbox
            .resolve_argv_executable("/usr/bin/git".into(), (&environment, &[]))
            .await
            .expect("preserve launcher diagnostics when lookup fails"),
        Path::new("/usr/bin/git")
    );
    let output = sandbox
        .execute_read_only_with_environment_removals("git", &["--version"], &environment, &[])
        .await
        .expect("return Git's unsuccessful command output");
    assert_ne!(output.exit_code, 0);
    assert!(output.stderr.contains("DEVELOPER_DIR"), "{output:?}");
    let output = sandbox
        .execute_read_only_with_environment_removals(
            "git",
            &["--version"],
            &[("SDKROOT", "nonexistent-mobius-sdk")],
            &[],
        )
        .await
        .expect("preserve SDK selection errors");
    assert_ne!(output.exit_code, 0);
    assert!(
        output.stderr.contains("nonexistent-mobius-sdk"),
        "{output:?}"
    );
    assert!(
        sandbox
            .resolve_argv_executable("/usr/bin/git".into(), (&environment, &["DEVELOPER_DIR"]),)
            .await
            .is_ok()
    );
    assert!(
        sandbox
            .deny_environment("DEVELOPER_DIR")
            .resolve_argv_executable("/usr/bin/git".into(), (&environment, &[]))
            .await
            .is_ok()
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorization_holds_the_catalog_lock_through_process_spawn() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, Mutex};

    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = local_sandbox(workspace.path());
    let allowed = Arc::new(Mutex::new(true));
    let checked = Arc::new(Barrier::new(2));
    let attempting_cutover = Arc::new(Barrier::new(2));
    let cutover_finished = Arc::new(AtomicBool::new(false));
    let authorization: CommandAuthorization = {
        let allowed = Arc::clone(&allowed);
        let checked = Arc::clone(&checked);
        let attempting_cutover = Arc::clone(&attempting_cutover);
        let cutover_finished = Arc::clone(&cutover_finished);
        Arc::new(move |launch| {
            let Ok(allowed) = allowed.lock() else {
                return Ok(());
            };
            if *allowed {
                checked.wait();
                attempting_cutover.wait();
                launch()?;
                assert!(!cutover_finished.load(Ordering::SeqCst));
            }
            Ok(())
        })
    };
    let writer = {
        let allowed = Arc::clone(&allowed);
        let checked = Arc::clone(&checked);
        let attempting_cutover = Arc::clone(&attempting_cutover);
        let cutover_finished = Arc::clone(&cutover_finished);
        std::thread::spawn(move || {
            checked.wait();
            attempting_cutover.wait();
            *allowed.lock().expect("authorization catalog") = false;
            cutover_finished.store(true, Ordering::SeqCst);
        })
    };

    let first = sandbox
        .execute_authorized(
            "true",
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            CommandMode::Foreground,
            CommandOutputSink::default(),
            &authorization,
        )
        .await
        .expect("authorized command");
    writer.join().expect("catalog writer");
    let second = sandbox
        .execute_authorized(
            "true",
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            CommandMode::Foreground,
            CommandOutputSink::default(),
            &authorization,
        )
        .await
        .expect("revoked command");

    assert!(first.is_some());
    assert!(second.is_none());
}

#[cfg(target_os = "macos")]
async fn assert_timeout_reaps_daemonized_descendants(
    sandbox: LocalSandbox,
    sandbox_mode: SandboxMode,
) {
    let pid_path = sandbox.root.join("daemon.pid");
    let network_access = if sandbox_mode == SandboxMode::DangerFullAccess {
        NetworkAccess::Allowed
    } else {
        NetworkAccess::Denied
    };
    let script = r#"/usr/bin/perl -MPOSIX=setsid -e '
exit if fork; setsid(); exit if fork;
open my $file, ">", "daemon.pid" or die; print $file "$$\n"; close $file;
sleep 30;
' &
sleep 30"#;

    let execution = tokio::spawn(async move {
        sandbox
            .execute(
                script,
                sandbox_mode,
                network_access,
                CommandMode::Foreground,
                CommandOutputSink::default(),
            )
            .await
    });
    let pid = tokio::task::spawn_blocking(move || {
        for _ in 0..500 {
            if let Ok(pid) = std::fs::read_to_string(&pid_path)
                .and_then(|pid| pid.trim().parse::<u32>().map_err(std::io::Error::other))
            {
                return Some(pid);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    })
    .await
    .expect("daemon readiness task");
    let Some(pid) = pid else {
        execution.abort();
        panic!("daemon pid was not written");
    };

    tokio::time::advance(Duration::from_millis(101)).await;
    let error = execution
        .await
        .expect("command task")
        .expect_err("foreground deadline");
    assert!(error.to_string().contains("timed out"));
    let alive = tokio::task::spawn_blocking(move || {
        for _ in 0..100 {
            let alive = std::process::Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    })
    .await
    .expect("daemon cleanup task");
    if alive {
        std::process::Command::new("/bin/kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .expect("kill surviving daemon");
    }
    assert!(!alive, "daemonized child {pid} survived command cleanup");
}

#[cfg(target_os = "macos")]
#[tokio::test(start_paused = true)]
async fn command_cleanup_reaps_daemonized_descendants() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = LocalSandbox::new(workspace.path())
        .expect("sandbox")
        .command_timeout(Duration::from_millis(100))
        .expect("timeout");
    assert_timeout_reaps_daemonized_descendants(sandbox, SandboxMode::WorkspaceWrite).await;
}

fn local_sandbox(workspace: &Path) -> LocalSandbox {
    LocalSandbox::new(workspace).expect("sandbox")
}

#[tokio::test]
async fn absolute_reads_and_writes_are_confined_to_workspace_roots() {
    let workspace = tempfile::tempdir().expect("workspace");
    let attached = tempfile::tempdir().expect("attached workspace");
    let outside = tempfile::tempdir().expect("outside");
    let workspace_root = std::fs::canonicalize(workspace.path()).expect("canonical workspace root");
    let attached_root = std::fs::canonicalize(attached.path()).expect("canonical workspace root");
    let outside_root = std::fs::canonicalize(outside.path()).expect("canonical outside root");
    let primary_source = workspace_root.join("source.txt");
    let primary_target = workspace_root.join("target.txt");
    let source = attached_root.join("source.txt");
    let target = attached_root.join("target.txt");
    let blocked = outside_root.join("blocked.txt");
    std::fs::write(&primary_source, "primary").expect("primary file");
    std::fs::write(&source, "attached").expect("attached file");
    let sandbox = local_sandbox(workspace.path())
        .allow_workspace_root(attached.path())
        .expect("workspace root");

    assert_eq!(
        sandbox
            .read(
                primary_source.to_str().expect("UTF-8 primary path"),
                SandboxMode::WorkspaceWrite,
            )
            .await
            .expect("primary read"),
        "primary"
    );
    sandbox
        .write(
            primary_target.to_str().expect("UTF-8 primary path"),
            "written",
            SandboxMode::WorkspaceWrite,
        )
        .await
        .expect("primary write");
    assert_eq!(
        sandbox
            .read(
                source.to_str().expect("UTF-8 source path"),
                SandboxMode::WorkspaceWrite,
            )
            .await
            .expect("attached read"),
        "attached"
    );
    sandbox
        .write(
            target.to_str().expect("UTF-8 target path"),
            "written",
            SandboxMode::WorkspaceWrite,
        )
        .await
        .expect("attached write");
    assert_eq!(
        std::fs::read_to_string(target).expect("written file"),
        "written"
    );
    assert!(
        sandbox
            .write(
                blocked.to_str().expect("UTF-8 blocked path"),
                "blocked",
                SandboxMode::WorkspaceWrite,
            )
            .await
            .is_err()
    );
    assert!(!blocked.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn absolute_read_roots_reject_symlink_escapes() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().expect("workspace");
    let resources = tempfile::tempdir().expect("resources");
    let outside = tempfile::tempdir().expect("outside");
    let secret = outside.path().join("secret.txt");
    let link = resources.path().join("link.txt");
    std::fs::write(&secret, "secret").expect("secret");
    symlink(&secret, &link).expect("resource symlink");
    let link = std::fs::canonicalize(resources.path())
        .expect("canonical resources")
        .join("link.txt");
    let sandbox = local_sandbox(workspace.path())
        .allow_read_root(resources.path())
        .expect("read root");

    assert!(
        sandbox
            .read(
                link.to_str().expect("UTF-8 link path"),
                SandboxMode::WorkspaceWrite,
            )
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn absolute_workspace_roots_reject_replaced_roots() {
    let workspace = tempfile::tempdir().expect("workspace");
    let parent = tempfile::tempdir().expect("parent");
    let attached = parent.path().join("attached");
    let displaced = parent.path().join("displaced");
    std::fs::create_dir(&attached).expect("attached workspace");
    let requested = std::fs::canonicalize(&attached)
        .expect("canonical attached workspace")
        .join("bait.txt");
    let sandbox = local_sandbox(workspace.path())
        .allow_workspace_root(&attached)
        .expect("workspace root");
    std::fs::rename(&attached, &displaced).expect("displace attached workspace");
    std::fs::create_dir(&attached).expect("replacement workspace");
    std::fs::write(attached.join("bait.txt"), "replacement").expect("replacement file");

    assert!(
        sandbox
            .read(
                requested.to_str().expect("UTF-8 attached path"),
                SandboxMode::WorkspaceWrite,
            )
            .await
            .is_err()
    );
    assert!(
        sandbox
            .write(
                requested.to_str().expect("UTF-8 attached path"),
                "escaped",
                SandboxMode::WorkspaceWrite,
            )
            .await
            .is_err()
    );
    assert!(
        sandbox
            .execute(
                "true",
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Denied,
                CommandMode::Foreground,
                CommandOutputSink::default(),
            )
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(attached.join("bait.txt")).expect("replacement file"),
        "replacement"
    );
    assert!(!displaced.join("bait.txt").exists());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn commands_cannot_write_outside_the_workspace_through_symlinks() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    symlink(outside.path(), workspace.path().join("outside")).expect("outside symlink");
    let sandbox = local_sandbox(workspace.path());

    let output = sandbox
        .execute(
            "touch outside/escaped",
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Allowed,
            CommandMode::Foreground,
            CommandOutputSink::default(),
        )
        .await
        .expect("sandboxed command");

    assert_ne!(output.exit_code, 0);
    assert!(!outside.path().join("escaped").exists());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn denied_environment_is_removed_from_full_access_commands() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = local_sandbox(workspace.path())
        .isolated_home()
        .deny_environment("MOBIUS_TEST_SECRET");

    let output = sandbox
        .execute_invocation(
            Invocation::Shell {
                script: r#"printf '%s:%s' "${MOBIUS_TEST_SECRET-unset}" "$MOBIUS_TEST_VISIBLE""#,
                executable: Path::new("/bin/bash"),
            },
            CommandIsolation {
                sandbox_mode: SandboxMode::DangerFullAccess,
                network_access: NetworkAccess::Allowed,
                workspace_access: WorkspaceAccess::Writable,
            },
            CommandMode::Foreground,
            CommandOutputSink::default(),
            (
                &[
                    ("MOBIUS_TEST_SECRET", "secret"),
                    ("MOBIUS_TEST_VISIBLE", "visible"),
                ],
                &[],
            ),
            None,
        )
        .await
        .expect("full access command")
        .expect("command launch");

    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert_eq!(output.stdout, "unset:visible");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn read_only_argv_cannot_modify_the_workspace() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = local_sandbox(workspace.path());

    let output = sandbox
        .execute_read_only("touch", &["blocked"], &[])
        .await
        .expect("read-only command");

    assert_ne!(output.exit_code, 0);
    assert!(!workspace.path().join("blocked").exists());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn network_policy_changes_command_isolation() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = local_sandbox(workspace.path());
    let denied = platform::sandboxed_command(
        &sandbox,
        &Invocation::Shell {
            script: "true",
            executable: Path::new("/bin/bash"),
        },
        NetworkAccess::Denied,
        WorkspaceAccess::Writable,
    )
    .expect("network-disabled command");
    let allowed = platform::sandboxed_command(
        &sandbox,
        &Invocation::Shell {
            script: "true",
            executable: Path::new("/bin/bash"),
        },
        NetworkAccess::Allowed,
        WorkspaceAccess::Writable,
    )
    .expect("network-enabled command");
    #[cfg(target_os = "linux")]
    {
        let isolation = |command: &Command| {
            let arguments = command.as_std().get_args().collect::<Vec<_>>();
            (
                arguments.contains(&OsStr::new("--unshare-net")),
                arguments
                    .windows(2)
                    .any(|pair| pair == [OsStr::new("--tmpfs"), OsStr::new("/run")]),
            )
        };
        assert_eq!(
            (isolation(&denied), isolation(&allowed)),
            ((true, Path::new("/run").is_dir()), (false, false))
        );
    }
    #[cfg(target_os = "macos")]
    {
        let policy = |command: &Command| {
            command
                .as_std()
                .get_args()
                .map(|argument| argument.to_string_lossy())
                .find(|argument| argument.contains("(deny default)"))
                .expect("Seatbelt policy")
                .into_owned()
        };
        assert!(!policy(&denied).contains("com.apple.SecurityServer"));
        assert!(policy(&allowed).contains("com.apple.SecurityServer"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn filesystem_handles_reject_symlink_escapes_and_aliases() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("parent");
    let workspace = parent.path().join("workspace");
    let outside = parent.path().join("outside.txt");
    let outside_directory = parent.path().join("outside");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&outside_directory).expect("outside directory");
    std::fs::write(&outside, "outside").expect("outside");
    std::fs::create_dir(workspace.join(".git")).expect("metadata");
    std::fs::write(workspace.join(".git/config"), "metadata").expect("metadata file");
    symlink(&outside, workspace.join("outside-link")).expect("outside link");
    symlink(&outside_directory, workspace.join("outside-directory"))
        .expect("outside directory link");
    symlink(workspace.join(".git"), workspace.join("metadata-link")).expect("metadata link");
    let sandbox = local_sandbox(&workspace);

    assert!(
        sandbox
            .read("outside-link", SandboxMode::WorkspaceWrite)
            .await
            .is_err()
    );
    assert!(
        sandbox
            .write("outside-link", "escaped", SandboxMode::WorkspaceWrite)
            .await
            .is_err()
    );
    assert!(
        sandbox
            .write(
                "outside-directory/new",
                "escaped",
                SandboxMode::WorkspaceWrite,
            )
            .await
            .is_err()
    );
    assert!(
        sandbox
            .write(
                "metadata-link/config",
                "escaped",
                SandboxMode::WorkspaceWrite,
            )
            .await
            .is_err()
    );
    assert!(
        sandbox
            .write("metadata-link/new", "escaped", SandboxMode::WorkspaceWrite,)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(outside).expect("outside"),
        "outside"
    );
    assert!(!outside_directory.join("new").exists());
    assert_eq!(
        std::fs::read_to_string(workspace.join(".git/config")).expect("metadata"),
        "metadata"
    );
    assert!(!workspace.join(".git/new").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn writes_replace_files_without_partial_state_or_permission_drift() {
    use std::os::unix::fs::PermissionsExt;

    let workspace = tempfile::tempdir().expect("workspace");
    let path = workspace.path().join("state.txt");
    std::fs::write(&path, "old").expect("old file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("permissions");
    let sandbox = local_sandbox(workspace.path());

    sandbox
        .write(
            "state.txt",
            "complete replacement",
            SandboxMode::WorkspaceWrite,
        )
        .await
        .expect("atomic write");

    assert_eq!(
        std::fs::read_to_string(&path).expect("replacement"),
        "complete replacement"
    );
    assert_eq!(
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    assert!(
        workspace
            .path()
            .read_dir()
            .expect("workspace entries")
            .all(|entry| !entry
                .expect("workspace entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".mobius-write-"))
    );
}

#[tokio::test]
async fn temporary_files_survive_calls_and_are_isolated_from_child_execution() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = LocalSandbox::new(workspace.path()).expect("sandbox");
    let child = sandbox.isolated_execution().expect("child");
    let path = sandbox.temporary_directory().join("observation.txt");
    let name = path.to_str().expect("path");
    let output = sandbox
        .execute(
            "printf before > \"$TMPDIR/observation.txt\"",
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            CommandMode::Foreground,
            CommandOutputSink::default(),
        )
        .await
        .expect("write");
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert_eq!(
        sandbox
            .read(name, SandboxMode::WorkspaceWrite)
            .await
            .expect("read"),
        "before"
    );
    sandbox
        .write(name, "after", SandboxMode::WorkspaceWrite)
        .await
        .expect("replace");
    for mode in [SandboxMode::WorkspaceWrite, SandboxMode::DangerFullAccess] {
        let output = sandbox
            .execute(
                "cat \"$TMPDIR/observation.txt\"",
                mode,
                NetworkAccess::Denied,
                CommandMode::Foreground,
                CommandOutputSink::default(),
            )
            .await
            .expect("read command");
        assert_eq!(output.exit_code, 0, "{}", output.stderr);
        assert_eq!(output.stdout, "after");
    }
    assert!(child.read(name, SandboxMode::WorkspaceWrite).await.is_err());
    let protected = tempfile::tempdir().expect("protected path");
    let guarded_child = sandbox
        .isolated_execution()
        .expect("guarded child")
        .deny_read(protected.path())
        .expect("guarded path");
    for (child, mode) in [
        (&child, SandboxMode::WorkspaceWrite),
        (&child, SandboxMode::DangerFullAccess),
        (&guarded_child, SandboxMode::DangerFullAccess),
    ] {
        let output = child
            .execute(
                &format!(
                    "cat '{}'",
                    sandbox.temp.path().join("observation.txt").display()
                ),
                mode,
                NetworkAccess::Denied,
                CommandMode::Foreground,
                CommandOutputSink::default(),
            )
            .await
            .expect("child command");
        if mode == SandboxMode::DangerFullAccess {
            assert_eq!(output.exit_code, 0, "{}", output.stderr);
            assert_eq!(output.stdout, "after");
        } else {
            assert_ne!(output.exit_code, 0, "sibling temporary files leaked");
        }
    }
    let escape = sandbox.temporary_directory().join("../escape");
    assert!(
        sandbox
            .write(
                escape.to_str().expect("path"),
                "no",
                SandboxMode::WorkspaceWrite,
            )
            .await
            .is_err()
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(workspace.path(), sandbox.temp.path().join("escape"))
            .expect("symlink");
        assert!(
            sandbox
                .write(
                    sandbox
                        .temporary_directory()
                        .join("escape/file")
                        .to_str()
                        .expect("path"),
                    "no",
                    SandboxMode::WorkspaceWrite,
                )
                .await
                .is_err()
        );
    }
    let physical = sandbox.temp.path().to_path_buf();
    drop(sandbox);
    assert!(!physical.exists());
}
