use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use mobius::backend::sandbox::ProcessGroupGuard;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};

use crate::{Error, Result};

pub(super) struct OwnedProcess {
    pub(super) child: Child,
    group: ProcessGroupGuard,
    record: PathBuf,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    pid: u32,
    start: String,
    executable: PathBuf,
}

impl OwnedProcess {
    pub(super) fn spawn(command: &mut Command, record: PathBuf) -> Result<Self> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let group = ProcessGroupGuard::new(&child)?;
        let pid = child
            .id()
            .ok_or_else(|| Error::Config("desktop child PID unavailable".into()))?;
        let identity = identity(pid)?
            .ok_or_else(|| Error::Config("desktop child exited during startup".into()))?;
        if let Err(error) = fs::write(&record, serde_json::to_vec(&identity)?) {
            let _ = child.start_kill();
            return Err(error.into());
        }
        private_file(&record)?;
        Ok(Self {
            child,
            group,
            record,
        })
    }

    pub(super) fn exit_status(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    pub(super) fn check_running(&mut self) -> Result<()> {
        if let Some(status) = self.exit_status()? {
            let name = self
                .record
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy();
            return Err(Error::Config(format!("desktop {name} exited ({status})")));
        }
        Ok(())
    }

    pub(super) async fn stop(mut self) {
        if let Some(pid) = self.child.id().and_then(|pid| i32::try_from(pid).ok()) {
            let _ = kill(Pid::from_raw(-pid), Signal::SIGTERM);
        }
        if tokio::time::timeout(Duration::from_secs(2), self.child.wait())
            .await
            .is_err()
        {
            self.group.kill();
            let _ = self.child.wait().await;
        }
        self.group.kill();
        let _ = fs::remove_file(&self.record);
    }
}

pub(super) async fn clean_record(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 4096 {
        return Err(Error::Config("invalid desktop process record".into()));
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if bytes.len() > 4096 || fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(Error::Config("invalid desktop process record".into()));
    }
    let recorded: Identity = serde_json::from_slice(&bytes)?;
    if recorded.pid <= 1 {
        return Err(Error::Config("invalid desktop child PID".into()));
    }
    if let Some(current) = identity(recorded.pid)? {
        if current != recorded {
            return Err(Error::Config(
                "desktop child identity changed; refusing stale process cleanup".into(),
            ));
        }
        // Records are advisory; refuse cleanup unless the live group leader matches.
        let pid = i32::try_from(recorded.pid)
            .map_err(|_| Error::Config("invalid desktop child PID".into()))?;
        kill(Pid::from_raw(-pid), Signal::SIGTERM)
            .map_err(|error| Error::Config(error.to_string()))?;
        wait_gone(&recorded, Duration::from_secs(2)).await?;
        if identity(recorded.pid)?.as_ref() == Some(&recorded) {
            kill(Pid::from_raw(-pid), Signal::SIGKILL)
                .map_err(|error| Error::Config(error.to_string()))?;
            wait_gone(&recorded, Duration::from_secs(2)).await?;
        }
        if identity(recorded.pid)?.as_ref() == Some(&recorded) {
            return Err(Error::Config(
                "desktop child did not stop; refusing to start another desktop".into(),
            ));
        }
    }
    fs::remove_file(path)?;
    Ok(())
}

async fn wait_gone(recorded: &Identity, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while identity(recorded.pid)?.as_ref() == Some(recorded)
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn identity(pid: u32) -> Result<Option<Identity>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let fields: Vec<_> = stat
        .rsplit_once(')')
        .ok_or_else(|| Error::Config("invalid desktop process identity".into()))?
        .1
        .split_whitespace()
        .collect();
    if fields.first() == Some(&"Z") {
        return Ok(None);
    }
    if fields.get(2).and_then(|value| value.parse::<u32>().ok()) != Some(pid) {
        return Err(Error::Config(
            "desktop process is not its group leader".into(),
        ));
    }
    let ticks = fields
        .get(19)
        .ok_or_else(|| Error::Config("desktop start time unavailable".into()))?;
    Ok(Some(Identity {
        pid,
        start: format!(
            "{}:{ticks}",
            fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim()
        ),
        executable: fs::read_link(format!("/proc/{pid}/exe"))?,
    }))
}

#[cfg(target_os = "macos")]
fn identity(pid: u32) -> Result<Option<Identity>> {
    let output = std::process::Command::new("/bin/ps")
        .args([
            "-p",
            &pid.to_string(),
            "-o",
            "pgid=",
            "-o",
            "lstart=",
            "-o",
            "comm=",
        ])
        .env("LC_ALL", "C")
        .output()?;
    if !output.status.success() || output.stdout.is_empty() {
        return Ok(None);
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| Error::Config("invalid desktop child identity".into()))?;
    let text = text.trim();
    let (group, text) = text
        .split_once(char::is_whitespace)
        .ok_or_else(|| Error::Config("invalid desktop child identity".into()))?;
    if group.parse::<u32>().ok() != Some(pid) {
        return Err(Error::Config(
            "desktop process is not its group leader".into(),
        ));
    }
    let text = text.trim_start();
    let (start, executable) = text
        .split_at_checked(24)
        .ok_or_else(|| Error::Config("invalid desktop child identity".into()))?;
    Ok(Some(Identity {
        pid,
        start: start.into(),
        executable: PathBuf::from(executable.trim()),
    }))
}

pub(super) fn private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Config(
            "desktop state must be a private directory".into(),
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub(super) fn private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub(super) fn executable(names: &[&str]) -> Result<PathBuf> {
    for name in names {
        for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            let path = directory.join(name);
            if path.is_file() {
                return Ok(fs::canonicalize(path)?);
            }
        }
    }
    Err(Error::Config(format!(
        "desktop requires {}",
        names.join(" or ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_record_never_signals_a_reused_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("child.json");
        let mut child = Command::new("/bin/sleep")
            .arg("10")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let current = identity(child.id().unwrap()).unwrap();
        if let Some(mut current) = current {
            current.start.push_str("-different");
            fs::write(&path, serde_json::to_vec(&current).unwrap()).unwrap();
            assert!(clean_record(&path).await.is_err());
            assert!(path.exists());
        }
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }
}
