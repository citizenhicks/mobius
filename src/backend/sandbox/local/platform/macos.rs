use std::path::Path;

use tokio::process::Command;

use super::super::Invocation;
use super::super::LocalSandbox;
use super::super::NetworkAccess;
use super::super::WorkspaceAccess;
use super::append_invocation;
use crate::Error;
use crate::Result;
use crate::backend::sandbox::MACOS_COMMAND_WRAPPER;
use crate::backend::sandbox::MACOS_SEATBELT_BASE_POLICY;
use crate::backend::sandbox::MACOS_SEATBELT_NETWORK_POLICY;

const PRIVATE_TEMP_POLICY: &str = r#"
(deny file-read* file-write*
  (require-all
    (subpath (param "TEMP_PARENT"))
    (require-not (subpath (param "TEMP_ROOT")))))
"#;

const SEATBELT_POLICY_SUFFIX: &str = r#"
(allow file-read*)
(allow file-write*
  (subpath (param "TEMP_ROOT"))
  (subpath (param "WRITABLE_ROOT")))
"#;

pub(crate) fn protected_full_access_command(
    sandbox: &LocalSandbox,
    invocation: &Invocation<'_>,
) -> Result<Command> {
    let executable = Path::new("/usr/bin/sandbox-exec");
    if !executable.is_file() {
        return Err(Error::Sandbox(
            "/usr/bin/sandbox-exec is unavailable".into(),
        ));
    }
    let mut policy = String::from(
        "(version 1)\n(allow default)\n\
         (deny signal (require-not (target same-sandbox)))\n\
         (deny process-info* (require-not (target same-sandbox)))\n\
         (deny mach-task-name (require-not (target same-sandbox)))\n",
    );
    for (index, denied) in sandbox.denied_reads.iter().enumerate() {
        let parameter = format!("DENIED_READ_{index}");
        policy.push_str(&format!(
            "\n(deny file-read* file-write*\n  (literal (param \"{parameter}\")){}\n)",
            if denied.directory {
                format!("\n  (subpath (param \"{parameter}\"))")
            } else {
                String::new()
            }
        ));
    }
    let mut command = Command::new(executable);
    command.arg("-p").arg(policy);
    for (index, denied) in sandbox.denied_reads.iter().enumerate() {
        let path = denied
            .path
            .to_str()
            .ok_or_else(|| Error::Sandbox("sandbox path is not UTF-8".into()))?;
        command.arg(format!("-DDENIED_READ_{index}={path}"));
    }
    append_isolated_invocation(&mut command, invocation, sandbox.isolated_home);
    Ok(command)
}

pub(crate) fn sandboxed_command(
    sandbox: &LocalSandbox,
    invocation: &Invocation<'_>,
    network_access: NetworkAccess,
    workspace_access: WorkspaceAccess,
) -> Result<Command> {
    let executable = Path::new("/usr/bin/sandbox-exec");
    if !executable.is_file() {
        return Err(Error::Sandbox(
            "/usr/bin/sandbox-exec is unavailable".into(),
        ));
    }
    let mut command = Command::new(executable);
    let mut policy =
        format!("{MACOS_SEATBELT_BASE_POLICY}{SEATBELT_POLICY_SUFFIX}{PRIVATE_TEMP_POLICY}");
    for index in 0..sandbox.workspace_roots.len() {
        let parameter = format!("WORKSPACE_ROOT_{index}");
        policy.push_str(&format!(
            "\n(allow file-write*\n  (literal (param \"{parameter}\"))\n  (subpath (param \"{parameter}\")))"
        ));
    }
    for (index, denied) in sandbox.denied_reads.iter().enumerate() {
        let parameter = format!("DENIED_READ_{index}");
        policy.push_str(&format!(
            "\n(deny file-read*\n  (literal (param \"{parameter}\")){}\n)",
            if denied.directory {
                format!("\n  (subpath (param \"{parameter}\"))")
            } else {
                String::new()
            }
        ));
    }
    if network_access == NetworkAccess::Allowed {
        policy.push_str("\n(allow network-outbound)\n(allow network-inbound)\n");
        policy.push_str(MACOS_SEATBELT_NETWORK_POLICY);
    }
    if workspace_access == WorkspaceAccess::ReadOnly {
        policy.push_str(
            r#"
(deny file-write*
  (literal (param "WRITABLE_ROOT"))
  (subpath (param "WRITABLE_ROOT")))"#,
        );
        for index in 0..sandbox.workspace_roots.len() {
            let parameter = format!("WORKSPACE_ROOT_{index}");
            policy.push_str(&format!(
                "\n(deny file-write*\n  (literal (param \"{parameter}\"))\n  (subpath (param \"{parameter}\")))"
            ));
        }
    }
    command.arg("-p").arg(policy);
    let root = sandbox
        .root
        .to_str()
        .ok_or_else(|| Error::Sandbox("sandbox path is not UTF-8".into()))?;
    command.arg(format!("-DWRITABLE_ROOT={root}"));
    append_temporary_parameters(&mut command, sandbox)?;
    for (index, root) in sandbox.workspace_roots.iter().enumerate() {
        let path = root
            .path
            .to_str()
            .ok_or_else(|| Error::Sandbox("sandbox path is not UTF-8".into()))?;
        command.arg(format!("-DWORKSPACE_ROOT_{index}={path}"));
    }
    for (index, denied) in sandbox.denied_reads.iter().enumerate() {
        let path = denied
            .path
            .to_str()
            .ok_or_else(|| Error::Sandbox("sandbox path is not UTF-8".into()))?;
        command.arg(format!("-DDENIED_READ_{index}={path}"));
    }
    append_isolated_invocation(&mut command, invocation, sandbox.isolated_home);
    Ok(command)
}

fn append_temporary_parameters(command: &mut Command, sandbox: &LocalSandbox) -> Result<()> {
    let temp = std::fs::canonicalize(sandbox.temp.path())?;
    let parent = temp
        .parent()
        .ok_or_else(|| Error::Sandbox("temporary parent unavailable".into()))?;
    for (name, path) in [("TEMP_PARENT", parent), ("TEMP_ROOT", temp.as_path())] {
        let path = path
            .to_str()
            .ok_or_else(|| Error::Sandbox("sandbox path is not UTF-8".into()))?;
        command.arg(format!("-D{name}={path}"));
    }
    Ok(())
}

fn append_isolated_invocation(
    command: &mut Command,
    invocation: &Invocation<'_>,
    isolated_home: bool,
) {
    let piped = matches!(
        invocation,
        Invocation::Argv {
            piped_input: true,
            ..
        }
    );
    command.args([
        "--",
        "/bin/bash",
        "--noprofile",
        "--norc",
        "-c",
        MACOS_COMMAND_WRAPPER,
        "mobius-command",
        if piped { "2" } else { "0" },
        if piped { "/dev/stdin" } else { "/dev/null" },
    ]);
    append_invocation(command, invocation, isolated_home);
}
