use tokio::process::Command;

use super::Invocation;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod unsupported;

#[cfg(all(test, target_os = "linux"))]
pub(super) use linux::validated_ssh_agent_socket;
#[cfg(target_os = "linux")]
pub(super) use linux::{protected_full_access_command, sandboxed_command};
#[cfg(target_os = "macos")]
pub(super) use macos::{protected_full_access_command, sandboxed_command};
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) use unsupported::{protected_full_access_command, sandboxed_command};

pub(super) fn append_invocation(
    command: &mut Command,
    invocation: &Invocation<'_>,
    isolated_home: bool,
) {
    match invocation {
        Invocation::Shell(script) => {
            command.arg("/bin/bash");
            if isolated_home {
                command.args(["--noprofile", "--norc", "-c", script]);
            } else {
                command.args(["-lc", script]);
            }
        }
        Invocation::Argv {
            executable,
            arguments,
            ..
        } => {
            command.arg(executable).args(arguments.iter().copied());
        }
    }
}

pub(super) fn host_command(invocation: &Invocation<'_>, isolated_home: bool) -> Command {
    match invocation {
        Invocation::Shell(script) => {
            let mut command = Command::new("/bin/bash");
            if isolated_home {
                command.args(["--noprofile", "--norc", "-c", script]);
            } else {
                command.args(["-lc", script]);
            }
            command
        }
        Invocation::Argv {
            executable,
            arguments,
            ..
        } => {
            let mut command = Command::new(executable);
            command.args(arguments.iter().copied());
            command
        }
    }
}
