//! Local sandbox tests.

use super::*;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use tokio::process::Command;

#[cfg(target_os = "macos")]
use crate::backend::sandbox::MACOS_COMMAND_WRAPPER;

#[cfg(target_os = "linux")]
#[test]
fn ssh_agent_mount_accepts_only_a_real_socket() {
    use std::os::unix::net::UnixListener;

    let directory = tempfile::Builder::new()
        .prefix("mobius-agent-")
        .tempdir_in("/tmp")
        .expect("agent directory");
    let socket = directory.path().join("agent.sock");
    let _listener = UnixListener::bind(&socket).expect("agent socket");
    assert_eq!(
        platform::validated_ssh_agent_socket(Some(socket.as_os_str()), &[]),
        std::fs::canonicalize(&socket).ok()
    );

    let regular = directory.path().join("not-a-socket");
    std::fs::write(&regular, b"not an agent").expect("regular file");
    assert!(platform::validated_ssh_agent_socket(Some(regular.as_os_str()), &[]).is_none());
}

#[tokio::test]
async fn bounded_output_reports_when_the_stream_was_truncated() {
    let output = read_output(
        &b"abcdef"[..],
        CommandStream::Stdout,
        CommandOutputSink::default(),
        3,
    )
    .await
    .expect("bounded output");

    assert_eq!(output.text, "abc\n[output truncated]");
    assert!(output.truncated);
}

#[tokio::test]
async fn background_commands_do_not_use_the_foreground_deadline() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = LocalSandbox::new(workspace.path())
        .expect("sandbox")
        .command_timeout(Duration::from_millis(1))
        .expect("timeout");

    let output = sandbox
        .execute(
            "sleep 0.05",
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            CommandMode::Background,
            CommandOutputSink::default(),
        )
        .await
        .expect("background command");

    assert_eq!(output.exit_code, 0);
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
    assert!(error.to_string().contains("exceeded"));
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
        let _ = std::process::Command::new("/bin/kill")
            .args(["-KILL", &pid.to_string()])
            .status();
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

#[cfg(target_os = "macos")]
#[tokio::test(start_paused = true)]
async fn protected_full_access_cleanup_reaps_daemonized_descendants() {
    let workspace = tempfile::tempdir().expect("workspace");
    let protected = tempfile::tempdir().expect("protected");
    let sandbox = LocalSandbox::new(workspace.path())
        .expect("sandbox")
        .deny_read(protected.path())
        .expect("protected path")
        .command_timeout(Duration::from_millis(100))
        .expect("timeout");
    assert_timeout_reaps_daemonized_descendants(sandbox, SandboxMode::DangerFullAccess).await;
}

fn local_sandbox(workspace: &Path) -> LocalSandbox {
    LocalSandbox::new(workspace).expect("sandbox")
}

#[tokio::test]
async fn absolute_reads_are_confined_to_explicit_read_roots() {
    let workspace = tempfile::tempdir().expect("workspace");
    let resources = tempfile::tempdir().expect("resources");
    let outside = tempfile::tempdir().expect("outside");
    let resource = resources.path().join("resource.txt");
    let secret = outside.path().join("secret.txt");
    std::fs::write(&resource, "resource").expect("resource");
    std::fs::write(&secret, "secret").expect("secret");
    let resource = std::fs::canonicalize(resource).expect("canonical resource");
    let secret = std::fs::canonicalize(secret).expect("canonical secret");
    let sandbox = local_sandbox(workspace.path())
        .allow_read_root(resources.path())
        .expect("read root");

    assert_eq!(
        sandbox
            .read(resource.to_str().expect("UTF-8 resource path"))
            .await
            .expect("allowed resource"),
        "resource"
    );
    assert!(
        sandbox
            .read(secret.to_str().expect("UTF-8 secret path"))
            .await
            .is_err()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn sandboxed_commands_can_execute_read_roots_inside_masked_temporary_storage() {
    use std::os::unix::fs::PermissionsExt;

    let parent = tempfile::tempdir_in("/tmp").expect("masked temporary directory");
    let resources = parent.path().join("runtime");
    let workspace = resources.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let executable = resources.join("runtime.sh");
    std::fs::write(&executable, "#!/bin/sh\nprintf runtime").expect("executable");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
        .expect("executable permission");
    let secret = parent.path().join("secret");
    std::fs::write(&secret, "private").expect("private sibling");
    let sandbox = local_sandbox(&workspace)
        .allow_read_root(&resources)
        .expect("runtime resources");
    let output = sandbox
        .execute(
            &format!(
                "'{}' && ! touch '{}' && ! test -e '{}' && printf written > allowed",
                executable.display(),
                resources.join("blocked").display(),
                secret.display(),
            ),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            CommandMode::Foreground,
            CommandOutputSink::default(),
        )
        .await
        .expect("sandboxed runtime");
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert_eq!(output.stdout, "runtime");
    assert!(!resources.join("blocked").exists());
    assert_eq!(
        sandbox.read("allowed").await.expect("workspace write"),
        "written"
    );
}

#[tokio::test]
async fn absolute_reads_and_writes_are_confined_to_workspace_roots() {
    let workspace = tempfile::tempdir().expect("workspace");
    let attached = tempfile::tempdir().expect("attached workspace");
    let outside = tempfile::tempdir().expect("outside");
    let attached_root = std::fs::canonicalize(attached.path()).expect("canonical workspace root");
    let outside_root = std::fs::canonicalize(outside.path()).expect("canonical outside root");
    let source = attached_root.join("source.txt");
    let target = attached_root.join("target.txt");
    let blocked = outside_root.join("blocked.txt");
    std::fs::write(&source, "attached").expect("attached file");
    let sandbox = local_sandbox(workspace.path())
        .allow_workspace_root(attached.path())
        .expect("workspace root");

    assert_eq!(
        sandbox
            .read(source.to_str().expect("UTF-8 source path"))
            .await
            .expect("attached read"),
        "attached"
    );
    sandbox
        .write(target.to_str().expect("UTF-8 target path"), "written")
        .await
        .expect("attached write");
    assert_eq!(
        std::fs::read_to_string(target).expect("written file"),
        "written"
    );
    assert!(
        sandbox
            .write(blocked.to_str().expect("UTF-8 blocked path"), "blocked")
            .await
            .is_err()
    );
    assert!(!blocked.exists());
}

#[test]
fn read_roots_cannot_overlap_denied_paths() {
    let workspace = tempfile::tempdir().expect("workspace");
    let protected = tempfile::tempdir().expect("protected");
    let sandbox = local_sandbox(workspace.path())
        .deny_read(protected.path())
        .expect("denied read");

    let result = sandbox.allow_read_root(protected.path());

    assert!(result.is_err());
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
            .read(link.to_str().expect("UTF-8 link path"))
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn absolute_read_roots_reject_replaced_roots() {
    let workspace = tempfile::tempdir().expect("workspace");
    let parent = tempfile::tempdir().expect("parent");
    let resources = parent.path().join("resources");
    let displaced = parent.path().join("displaced");
    std::fs::create_dir(&resources).expect("resources");
    let requested = std::fs::canonicalize(&resources)
        .expect("canonical resources")
        .join("bait.txt");
    let sandbox = local_sandbox(workspace.path())
        .allow_read_root(&resources)
        .expect("read root");
    std::fs::rename(&resources, &displaced).expect("displace resources");
    std::fs::create_dir(&resources).expect("replacement resources");
    std::fs::write(resources.join("bait.txt"), "replacement").expect("replacement file");

    assert!(
        sandbox
            .read(requested.to_str().expect("UTF-8 resource path"))
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
            .read(requested.to_str().expect("UTF-8 attached path"))
            .await
            .is_err()
    );
    assert!(
        sandbox
            .write(requested.to_str().expect("UTF-8 attached path"), "escaped")
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

#[tokio::test]
async fn binary_range_reads_from_the_same_opened_file() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join("nested")).expect("nested directory");
    std::fs::write(workspace.path().join("nested/data.bin"), b"\0abcdef").expect("binary file");
    let sandbox = local_sandbox(workspace.path());

    let (data, next_offset) = sandbox
        .read_range("nested/data.bin", 1, 3)
        .await
        .expect("range");

    assert_eq!(data, b"abc");
    assert_eq!(next_offset, Some(4));
}

#[cfg(unix)]
#[tokio::test]
async fn binary_range_rejects_symlinked_files_and_parents() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("secret"), b"secret").expect("outside file");
    symlink(
        outside.path().join("secret"),
        workspace.path().join("file-link"),
    )
    .expect("file symlink");
    symlink(outside.path(), workspace.path().join("directory-link")).expect("directory symlink");
    let sandbox = local_sandbox(workspace.path());

    assert!(sandbox.read_range("file-link", 0, 16).await.is_err());
    assert!(
        sandbox
            .read_range("directory-link/secret", 0, 16)
            .await
            .is_err()
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn isolated_home_is_private_and_writable() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = local_sandbox(workspace.path()).isolated_home();
    let expected = sandbox.temp.path().to_string_lossy().into_owned();

    let output = sandbox
        .execute(
            r#"test -d "$HOME" && test -w "$HOME" && printf '%s' "$HOME""#,
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            CommandMode::Foreground,
            CommandOutputSink::default(),
        )
        .await
        .expect("isolated home command");

    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert_eq!(output.stdout, expected);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn authorized_commands_can_modify_the_whole_workspace() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = local_sandbox(workspace.path()).isolated_home();

    for (label, mode) in [
        ("foreground", CommandMode::Foreground),
        ("background", CommandMode::Background),
    ] {
        let script = format!(
            "mkdir -p .agents .codex && touch .agents/{label} .codex/{label} {label}.txt && git init --quiet && git add -- {label}.txt && git -c user.name=möbius -c user.email=mobius@example.invalid commit --quiet -m {label}"
        );
        let output = sandbox
            .execute(
                &script,
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Denied,
                mode,
                CommandOutputSink::default(),
            )
            .await
            .expect("authorized workspace command");

        assert_eq!(output.exit_code, 0, "{}", output.stderr);
        assert!(workspace.path().join(".git/index").is_file());
        assert!(workspace.path().join(".agents").join(label).is_file());
        assert!(workspace.path().join(".codex").join(label).is_file());
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn commands_apply_workspace_access_to_attached_roots() {
    let workspace = tempfile::tempdir().expect("workspace");
    let attached = tempfile::tempdir().expect("attached workspace");
    let writable = attached.path().join("writable");
    let blocked = attached.path().join("blocked");
    let sandbox = local_sandbox(workspace.path())
        .allow_workspace_root(attached.path())
        .expect("workspace root")
        .isolated_home();

    let output = sandbox
        .execute(
            &format!("touch {}", writable.display()),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            CommandMode::Foreground,
            CommandOutputSink::default(),
        )
        .await
        .expect("workspace command");
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert!(writable.is_file());

    let output = sandbox
        .execute_read_only(
            "touch",
            &[blocked.to_str().expect("UTF-8 blocked path")],
            &[],
        )
        .await
        .expect("read-only command");
    assert_ne!(output.exit_code, 0);
    assert!(!blocked.exists());
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
async fn full_access_commands_run_outside_the_workspace_sandbox() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    let target = outside.path().join("written");
    let sandbox = local_sandbox(workspace.path());

    let output = sandbox
        .execute(
            &format!("touch {}", target.display()),
            SandboxMode::DangerFullAccess,
            NetworkAccess::Allowed,
            CommandMode::Foreground,
            CommandOutputSink::default(),
        )
        .await
        .expect("full access command");

    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert!(target.is_file());
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
            Invocation::Shell(
                r#"printf '%s:%s' "${MOBIUS_TEST_SECRET-unset}" "$MOBIUS_TEST_VISIBLE""#,
            ),
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
#[tokio::test(start_paused = true)]
async fn full_access_timeout_reaps_process_group_descendants() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = local_sandbox(workspace.path())
        .command_timeout(Duration::from_millis(100))
        .expect("timeout");
    let execution = tokio::spawn(async move {
        sandbox
            .execute(
                "sh -c 'echo $$ > child.pid; sleep 30' & sleep 30",
                SandboxMode::DangerFullAccess,
                NetworkAccess::Allowed,
                CommandMode::Foreground,
                CommandOutputSink::default(),
            )
            .await
    });
    let pid_path = workspace.path().join("child.pid");
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
    .expect("child readiness task")
    .expect("child pid");

    tokio::time::advance(Duration::from_millis(101)).await;
    let error = execution
        .await
        .expect("command task")
        .expect_err("foreground deadline");
    assert!(error.to_string().contains("exceeded"));
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
    .expect("child cleanup task");
    if alive {
        let _ = std::process::Command::new("/bin/kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    assert!(!alive, "child {pid} survived full-access timeout cleanup");
}

#[cfg(target_os = "macos")]
#[test]
fn host_commands_do_not_embed_the_seatbelt_cleanup_wrapper() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = local_sandbox(workspace.path());
    let command = platform::host_command(&Invocation::Shell("true"), sandbox.isolated_home);
    let command = command.as_std();

    assert_eq!(command.get_program(), "/bin/bash");
    assert!(
        command
            .get_args()
            .all(|argument| argument != MACOS_COMMAND_WRAPPER)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn protected_full_access_masks_paths_and_scopes_cleanup() {
    let workspace = tempfile::tempdir().expect("workspace");
    let protected = tempfile::tempdir().expect("protected");
    let sandbox = local_sandbox(workspace.path())
        .deny_read(protected.path())
        .expect("protected path");
    let command = platform::protected_full_access_command(&sandbox, &Invocation::Shell("true"))
        .expect("protected full access command");
    let arguments = command
        .as_std()
        .get_args()
        .map(|argument| argument.to_string_lossy())
        .collect::<Vec<_>>();
    let policy = arguments
        .iter()
        .position(|argument| argument == "-p")
        .and_then(|index| arguments.get(index + 1))
        .expect("Seatbelt policy");

    assert!(policy.contains("(allow default)"));
    assert!(policy.contains("(deny file-read* file-write*"));
    assert!(policy.contains("(deny signal (require-not (target same-sandbox)))"));
    assert!(policy.contains("(deny process-info* (require-not (target same-sandbox)))"));
    assert!(
        arguments
            .iter()
            .any(|argument| argument.as_ref() == MACOS_COMMAND_WRAPPER)
    );
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
        &Invocation::Shell("true"),
        NetworkAccess::Denied,
        WorkspaceAccess::Writable,
    )
    .expect("network-disabled command");
    let allowed = platform::sandboxed_command(
        &sandbox,
        &Invocation::Shell("true"),
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

#[cfg(target_os = "linux")]
#[test]
fn empty_proc_keeps_pid_isolation_in_both_bubblewrap_commands() {
    let workspace = tempfile::tempdir().expect("workspace");
    let protected = tempfile::tempdir().expect("protected");
    let default = local_sandbox(workspace.path());
    let empty = local_sandbox(workspace.path())
        .deny_read(protected.path())
        .expect("protected path")
        .empty_proc();
    let default_command = platform::sandboxed_command(
        &default,
        &Invocation::Shell("true"),
        NetworkAccess::Denied,
        WorkspaceAccess::Writable,
    )
    .expect("default command");
    let empty_commands = [
        platform::sandboxed_command(
            &empty,
            &Invocation::Shell("true"),
            NetworkAccess::Denied,
            WorkspaceAccess::Writable,
        )
        .expect("sandboxed command"),
        platform::protected_full_access_command(&empty, &Invocation::Shell("true"))
            .expect("protected full-access command"),
    ];
    let mounts_proc = |command: &Command, option: &str| {
        command
            .as_std()
            .get_args()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|pair| pair == [OsStr::new(option), OsStr::new("/proc")])
    };

    assert!(mounts_proc(&default_command, "--proc"));
    for command in empty_commands {
        assert!(
            command
                .as_std()
                .get_args()
                .any(|argument| argument == OsStr::new("--unshare-pid"))
        );
        assert!(mounts_proc(&command, "--tmpfs"));
        assert!(!mounts_proc(&command, "--proc"));
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn empty_proc_hides_host_processes() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = local_sandbox(workspace.path()).empty_proc();
    let output = sandbox
        .execute(
            &format!(
                "test ! -e /proc/self/status && ! kill -0 {} 2>/dev/null",
                std::process::id()
            ),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            CommandMode::Foreground,
            CommandOutputSink::default(),
        )
        .await
        .expect("sandboxed command");

    assert_eq!(output.exit_code, 0, "{}", output.stderr);
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

    assert!(sandbox.read("outside-link").await.is_err());
    assert!(sandbox.write("outside-link", "escaped").await.is_err());
    assert!(
        sandbox
            .write("outside-directory/new", "escaped")
            .await
            .is_err()
    );
    assert!(
        sandbox
            .write("metadata-link/config", "escaped")
            .await
            .is_err()
    );
    assert!(sandbox.write("metadata-link/new", "escaped").await.is_err());
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
async fn filesystem_handles_reject_a_replaced_workspace_root() {
    let parent = tempfile::tempdir().expect("parent");
    let workspace = parent.path().join("workspace");
    let displaced = parent.path().join("displaced");
    std::fs::create_dir(&workspace).expect("workspace");
    let sandbox = local_sandbox(&workspace);
    let child = sandbox.isolated_execution().expect("child sandbox");
    std::fs::rename(&workspace, &displaced).expect("displace workspace");
    std::fs::create_dir(&workspace).expect("replacement workspace");
    std::fs::write(workspace.join("bait.txt"), "replacement").expect("replacement file");

    assert!(sandbox.read("bait.txt").await.is_err());
    assert!(child.read("bait.txt").await.is_err());
    assert!(sandbox.isolated_execution().is_err());
    assert!(sandbox.write("new.txt", "escaped").await.is_err());
    assert!(!workspace.join("new.txt").exists());
    assert!(!displaced.join("new.txt").exists());
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
        .write("state.txt", "complete replacement")
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

#[cfg(target_os = "linux")]
#[test]
fn bwrap_discovery_rejects_workspace_path_aliases() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("parent");
    let workspace = parent.path().join("workspace");
    let binaries = workspace.join("bin");
    let alias = parent.path().join("alias");
    std::fs::create_dir_all(&binaries).expect("create binaries");
    std::fs::write(binaries.join("bwrap"), "").expect("create bwrap");
    symlink(&binaries, &alias).expect("create path alias");

    assert!(find_executable_in("bwrap", &workspace, &[], alias.as_os_str()).is_none());
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
    assert_eq!(sandbox.read(name).await.expect("read"), "before");
    sandbox.write(name, "after").await.expect("replace");
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
    assert!(child.read(name).await.is_err());
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
            .write(escape.to_str().expect("path"), "no")
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
                    "no"
                )
                .await
                .is_err()
        );
    }
    let physical = sandbox.temp.path().to_path_buf();
    drop(sandbox);
    assert!(!physical.exists());
}

#[test]
fn private_temporary_storage_cannot_be_exposed_as_a_workspace_or_read_root() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sandbox = LocalSandbox::new(workspace.path()).expect("sandbox");
    let private_parent = sandbox.temp.path().parent().expect("private parent");
    assert!(LocalSandbox::new(private_parent).is_err());
    assert!(
        LocalSandbox::new(workspace.path())
            .expect("sandbox")
            .allow_workspace_root(private_parent)
            .is_err()
    );
    assert!(
        LocalSandbox::new(workspace.path())
            .expect("sandbox")
            .allow_read_root(private_parent)
            .is_err()
    );
}
