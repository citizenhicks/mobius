use std::ffi::OsStr;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use tokio::process::Command;

use super::super::Invocation;
use super::super::LocalSandbox;
use super::super::NetworkAccess;
use super::super::WorkspaceAccess;
use super::append_invocation;
use crate::Error;
use crate::Result;

pub(crate) fn sandboxed_command(
    sandbox: &LocalSandbox,
    invocation: &Invocation<'_>,
    network_access: NetworkAccess,
    workspace_access: WorkspaceAccess,
) -> Result<Command> {
    let bwrap = sandbox
        .find_executable("bwrap")
        .map_err(|_| Error::Sandbox("bubblewrap (`bwrap`) is required on Linux".into()))?;
    let mut command = Command::new(bwrap);
    command.args([
        "--new-session",
        "--die-with-parent",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
    ]);
    command.args(["--tmpfs", "/tmp"]);
    let private_parent = std::fs::canonicalize(sandbox.temp.path())?
        .parent()
        .ok_or_else(|| Error::Sandbox("temporary storage has no parent".into()))?
        .to_path_buf();
    if !private_parent.starts_with("/tmp") {
        command.arg("--tmpfs").arg(private_parent);
    }
    command
        .arg("--bind")
        .arg(sandbox.temp.path())
        .arg(sandbox.temp.path());
    if let Some(socket) = ssh_agent_socket(sandbox)
        && let Ok(relative) = socket.strip_prefix("/tmp")
    {
        let mut directory = PathBuf::from("/tmp");
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                let Component::Normal(component) = component else {
                    continue;
                };
                directory.push(component);
                command.arg("--dir").arg(&directory);
            }
        }
        command.arg("--ro-bind").arg(&socket).arg(&socket);
        command.env("SSH_AUTH_SOCK", socket);
    }
    if network_access == NetworkAccess::Denied && Path::new("/run").is_dir() {
        command.args(["--tmpfs", "/run"]);
    }
    command
        .arg(if workspace_access == WorkspaceAccess::ReadOnly {
            "--ro-bind"
        } else {
            "--bind"
        })
        .arg(&sandbox.root)
        .arg(&sandbox.root);
    for root in &sandbox.workspace_roots {
        command
            .arg(if workspace_access == WorkspaceAccess::ReadOnly {
                "--ro-bind"
            } else {
                "--bind"
            })
            .arg(&root.path)
            .arg(&root.path);
    }
    for denied in &sandbox.denied_reads {
        if denied.directory {
            command.arg("--tmpfs").arg(&denied.path);
        } else {
            command.arg("--ro-bind").arg("/dev/null").arg(&denied.path);
        }
    }
    command.args(["--unshare-user", "--unshare-pid"]);
    if network_access == NetworkAccess::Denied {
        command.arg("--unshare-net");
    }
    command
        .arg(if sandbox.empty_proc {
            "--tmpfs"
        } else {
            "--proc"
        })
        .args(["/proc", "--chdir"]);
    command.arg(&sandbox.root);
    command.arg("--");
    append_invocation(&mut command, invocation, sandbox.isolated_home);
    Ok(command)
}

pub(crate) fn protected_full_access_command(
    sandbox: &LocalSandbox,
    invocation: &Invocation<'_>,
) -> Result<Command> {
    let bwrap = sandbox
        .find_executable("bwrap")
        .map_err(|_| Error::Sandbox("bubblewrap (`bwrap`) is required on Linux".into()))?;
    let mut command = Command::new(bwrap);
    command.args([
        "--new-session",
        "--die-with-parent",
        "--bind",
        "/",
        "/",
        "--dev-bind",
        "/dev",
        "/dev",
    ]);
    for denied in &sandbox.denied_reads {
        if denied.directory {
            command.arg("--tmpfs").arg(&denied.path);
        } else {
            command.arg("--ro-bind").arg("/dev/null").arg(&denied.path);
        }
    }
    command.args(["--unshare-user", "--unshare-pid"]);
    command
        .arg(if sandbox.empty_proc {
            "--tmpfs"
        } else {
            "--proc"
        })
        .args(["/proc", "--chdir"]);
    command.arg(&sandbox.root).arg("--");
    append_invocation(&mut command, invocation, sandbox.isolated_home);
    Ok(command)
}

fn ssh_agent_socket(sandbox: &LocalSandbox) -> Option<PathBuf> {
    if sandbox.isolated_home || sandbox.denied_environment.contains("SSH_AUTH_SOCK") {
        return None;
    }
    let value = std::env::var_os("SSH_AUTH_SOCK");
    validated_ssh_agent_socket(value.as_deref(), &sandbox.denied_reads)
}

pub(crate) fn validated_ssh_agent_socket(
    value: Option<&OsStr>,
    denied_reads: &[super::super::DeniedRead],
) -> Option<PathBuf> {
    use std::os::unix::fs::FileTypeExt as _;

    let path = std::fs::canonicalize(Path::new(value?)).ok()?;
    let metadata = std::fs::metadata(&path).ok()?;
    (path.is_absolute()
        && metadata.file_type().is_socket()
        && denied_reads
            .iter()
            .all(|denied| !path.starts_with(&denied.path)))
    .then_some(path)
}
