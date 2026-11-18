use tokio::process::Command;

use super::Invocation;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod unsupported;

#[cfg(target_os = "linux")]
pub(super) use linux::{check_procfs_output, protected_full_access_command, sandboxed_command};
#[cfg(target_os = "macos")]
pub(super) use macos::{protected_full_access_command, sandboxed_command};
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) use unsupported::{protected_full_access_command, sandboxed_command};

impl Invocation<'_> {
    fn executable(&self) -> &std::path::Path {
        match self {
            Self::Shell { executable, .. } | Self::Argv { executable, .. } => executable,
        }
    }

    fn append_arguments(&self, command: &mut Command, isolated_home: bool) {
        match self {
            Self::Shell { script, .. } => {
                command.args([if isolated_home { "-c" } else { "-lc" }, script]);
            }
            Self::Argv { arguments, .. } => {
                command.args(arguments.iter().copied());
            }
        }
    }
}

pub(super) fn append_invocation(
    command: &mut Command,
    invocation: &Invocation<'_>,
    isolated_home: bool,
) {
    command.arg(invocation.executable());
    invocation.append_arguments(command, isolated_home);
}

pub(super) fn host_command(invocation: &Invocation<'_>, isolated_home: bool) -> Command {
    let mut command = Command::new(invocation.executable());
    invocation.append_arguments(&mut command, isolated_home);
    command
}
