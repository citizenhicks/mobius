//! Operator-owned host activity lease, independent of outbound collectors.

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
use std::time::Duration;

use mobius::backend::sandbox::ProcessGroupGuard;
#[cfg(unix)]
use nix::sys::signal::{Signal, kill};
#[cfg(unix)]
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tokio::process::Command;
use tokio::process::{Child, ChildStdin};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::{Error, Result, host::GatewayHost};

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct Defaults {
    idle_grace_seconds: u64,
    timeout_seconds: u64,
    retry_seconds: u64,
}
mobius::embedded_config! {
    static DEFAULTS: Defaults = include_str!("activity.toml");
}

/// Local foreground command holding a host inhibitor until stdin closes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActivityHookConfig {
    /// Absolute trusted executable followed by its arguments; no shell is added.
    pub command: Vec<String>,
    /// Hold after the latest work transition or first confirmed idle observation.
    pub idle_grace_seconds: u64,
    /// Deadline for activity measurements and each graceful child shutdown phase.
    pub timeout_seconds: u64,
    /// Retry and child-health interval after a failed measurement or command exit.
    pub retry_seconds: u64,
}

impl Default for ActivityHookConfig {
    fn default() -> Self {
        Self {
            command: Vec::new(),
            idle_grace_seconds: DEFAULTS.idle_grace_seconds,
            timeout_seconds: DEFAULTS.timeout_seconds,
            retry_seconds: DEFAULTS.retry_seconds,
        }
    }
}

impl ActivityHookConfig {
    /// Validates local command text, deadlines, and executable identity.
    /// # Errors
    /// Returns an error for invalid settings or an unavailable executable.
    pub fn validate(&self) -> Result<()> {
        self.executable().map(drop)
    }

    /// Rejects hook executables located inside protected or agent-writable roots.
    /// # Errors
    /// Returns an error for inaccessible roots or an overlapping executable.
    pub fn validate_roots<'a>(&self, roots: impl IntoIterator<Item = &'a Path>) -> Result<()> {
        let executable = self.executable()?;
        for root in roots {
            if executable.starts_with(std::fs::canonicalize(root)?) {
                return Err(Error::Config(
                    "activity hook executable must be outside state and workspace roots".into(),
                ));
            }
        }
        Ok(())
    }

    fn executable(&self) -> Result<PathBuf> {
        if !cfg!(unix) {
            return Err(Error::Config("activity hooks require Unix".into()));
        }
        crate::config::bounded(
            "telemetry.activity_hook.command",
            self.command.len(),
            1..=64,
        )?;
        for argument in &self.command {
            if argument.len() > 4096 || argument.chars().any(char::is_control) {
                return Err(Error::Config(
                    "activity hook arguments must be at most 4096 bytes without control characters"
                        .into(),
                ));
            }
        }
        crate::config::bounded(
            "telemetry.activity_hook.idle_grace_seconds",
            self.idle_grace_seconds,
            0..=3600,
        )?;
        crate::config::bounded(
            "telemetry.activity_hook.timeout_seconds",
            self.timeout_seconds,
            1..=60,
        )?;
        crate::config::bounded(
            "telemetry.activity_hook.retry_seconds",
            self.retry_seconds,
            1..=3600,
        )?;
        let path = Path::new(&self.command[0]);
        if !path.is_absolute() {
            return Err(Error::Config(
                "activity hook executable must be absolute".into(),
            ));
        }
        let path = std::fs::canonicalize(path)?;
        let metadata = path.metadata()?;
        if !metadata.is_file() {
            return Err(Error::Config(
                "activity hook executable must be a file".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(Error::Config("activity hook file is not executable".into()));
            }
        }
        Ok(path)
    }
}

struct HookProcess {
    child: Child,
    group: ProcessGroupGuard,
    // Child::wait closes Child.stdin, so the hold owns its write end separately.
    stdin: Option<ChildStdin>,
}

pub(crate) struct ActivityHook<'a> {
    config: Option<&'a ActivityHookConfig>,
    executable: Option<PathBuf>,
    process: Option<HookProcess>,
    retiring: JoinSet<()>,
}

impl<'a> ActivityHook<'a> {
    pub(crate) fn new(config: Option<&'a ActivityHookConfig>) -> Result<Self> {
        Ok(Self {
            executable: config.map(ActivityHookConfig::executable).transpose()?,
            config,
            process: None,
            retiring: JoinSet::new(),
        })
    }

    /// Runs as a pinned server future; cancellation leaves cleanup owned by this hook.
    pub(crate) async fn run(&mut self, host: &GatewayHost, clients: impl Fn() -> Result<usize>) {
        let Some(config) = self.config else {
            return std::future::pending().await;
        };
        let retry = Duration::from_secs(config.retry_seconds);
        let grace = Duration::from_secs(config.idle_grace_seconds);
        let mut changes = host.activity_changes();
        let mut revision = *changes.borrow_and_update();
        let mut idle_at = None;
        let mut retry_at = None;
        // Unknown initial activity must not release the host before measurement succeeds.
        self.acquire_when_due(&mut retry_at);
        loop {
            let current = *changes.borrow_and_update();
            if current != revision {
                revision = current;
                idle_at = Some(Instant::now() + grace);
                self.acquire_when_due(&mut retry_at);
            }
            let mut dirty = false;
            let measured = {
                let measurement = tokio::time::timeout(self.timeout(), host.runtime_activity());
                tokio::pin!(measurement);
                loop {
                    tokio::select! {
                        result = &mut measurement => break result,
                        result = wait_process(&mut self.process) => {
                            self.on_exit(result, &mut retry_at, retry);
                        }
                        () = tokio::time::sleep_until(retry_at.unwrap_or_else(Instant::now)),
                            if self.process.is_none() && retry_at.is_some() => {
                            self.acquire_when_due(&mut retry_at);
                        }
                        Ok(()) = changes.changed() => {
                            dirty = true;
                            let current = *changes.borrow_and_update();
                            if current != revision {
                                revision = current;
                                idle_at = Some(Instant::now() + grace);
                                self.acquire_when_due(&mut retry_at);
                            }
                        }
                    }
                }
            };
            let mut idle = measured_idle(measured, &clients);
            dirty |= changes.has_changed().unwrap_or(true);
            let current = *changes.borrow_and_update();
            if current != revision {
                revision = current;
                idle = false;
            } else if dirty {
                // A storage wake can commit pending work without changing the execution revision.
                self.acquire_when_due(&mut retry_at);
                continue;
            }
            let now = Instant::now();
            if !idle {
                idle_at = None;
            }
            let held = !idle || now < *idle_at.get_or_insert(now + grace);
            if held {
                self.acquire_when_due(&mut retry_at);
            } else {
                retry_at = None;
                self.retire();
            }
            let mut next = Instant::now() + retry;
            if held
                && self.process.is_none()
                && let Some(deadline) = retry_at
            {
                next = next.min(deadline);
            }
            if idle
                && held
                && let Some(deadline) = idle_at
            {
                next = next.min(deadline);
            }
            tokio::select! {
                Ok(()) = changes.changed() => {}
                () = tokio::time::sleep_until(next) => {}
                result = wait_process(&mut self.process) => {
                    self.on_exit(result, &mut retry_at, retry);
                }
                Some(result) = self.retiring.join_next(), if !self.retiring.is_empty() => {
                    if let Err(error) = result {
                        tracing::warn!(%error, "activity hook cleanup failed");
                    }
                }
            }
        }
    }

    fn acquire_when_due(&mut self, retry_at: &mut Option<Instant>) {
        if self.process.is_none() && retry_at.is_none_or(|deadline| Instant::now() >= deadline) {
            match self.acquire() {
                Ok(()) => *retry_at = None,
                Err(error) => {
                    tracing::warn!(%error, "activity hook acquisition failed");
                    *retry_at = Some(
                        Instant::now()
                            + Duration::from_secs(
                                self.config
                                    .map_or(DEFAULTS.retry_seconds, |config| config.retry_seconds),
                            ),
                    );
                }
            }
        }
    }

    fn on_exit(
        &mut self,
        result: std::io::Result<std::process::ExitStatus>,
        retry_at: &mut Option<Instant>,
        retry: Duration,
    ) {
        match result {
            Ok(status) => tracing::warn!(%status, "activity hook command exited"),
            Err(error) => tracing::warn!(%error, "activity hook command status failed"),
        }
        self.process = None;
        *retry_at = Some(Instant::now() + retry);
    }

    fn retire(&mut self) {
        if self.retiring.is_empty()
            && let Some(process) = self.process.take()
        {
            self.retiring.spawn(stop_process(process, self.timeout()));
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(
            self.config
                .map_or(DEFAULTS.timeout_seconds, |config| config.timeout_seconds),
        )
    }

    #[cfg(unix)]
    fn acquire(&mut self) -> Result<()> {
        let Some(config) = self.config else {
            return Ok(());
        };
        let Some(executable) = self.executable.as_ref() else {
            return Ok(());
        };
        if self.process.is_some() {
            return Ok(());
        }
        let mut child = Command::new(executable)
            .args(&config.command[1..])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let group = ProcessGroupGuard::new(&child)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Config("activity hook stdin unavailable".into()))?;
        self.process = Some(HookProcess {
            child,
            group,
            stdin: Some(stdin),
        });
        Ok(())
    }

    #[cfg(not(unix))]
    fn acquire(&mut self) -> Result<()> {
        Err(Error::Config("activity hooks require Unix".into()))
    }

    pub(crate) async fn stop(&mut self) {
        if let Some(process) = self.process.take() {
            stop_process(process, self.timeout()).await;
        }
        while let Some(result) = self.retiring.join_next().await {
            if let Err(error) = result {
                tracing::warn!(%error, "activity hook cleanup failed");
            }
        }
    }
}

fn measured_idle(
    measured: std::result::Result<
        std::result::Result<crate::host::RuntimeActivity, crate::host::Rejection>,
        tokio::time::error::Elapsed,
    >,
    clients: impl Fn() -> Result<usize>,
) -> bool {
    match measured {
        Ok(Ok(activity)) => match clients() {
            Ok(clients) => activity.idle && clients == 0,
            Err(error) => {
                tracing::warn!(%error, "activity hook client count failed");
                false
            }
        },
        Ok(Err(error)) => {
            tracing::warn!(error = %error.message, "activity hook measurement failed");
            false
        }
        Err(_) => {
            tracing::warn!("activity hook measurement timed out");
            false
        }
    }
}

async fn wait_process(
    process: &mut Option<HookProcess>,
) -> std::io::Result<std::process::ExitStatus> {
    match process {
        Some(process) => process.child.wait().await,
        None => std::future::pending().await,
    }
}

async fn stop_process(mut process: HookProcess, timeout: Duration) {
    drop(process.stdin.take());
    if !matches!(
        tokio::time::timeout(timeout, process.child.wait()).await,
        Ok(Ok(_))
    ) {
        #[cfg(unix)]
        if let Some(pid) = process.child.id().and_then(|pid| i32::try_from(pid).ok()) {
            let _ = kill(Pid::from_raw(-pid), Signal::SIGTERM);
        }
        if !matches!(
            tokio::time::timeout(timeout, process.child.wait()).await,
            Ok(Ok(_))
        ) {
            process.group.kill();
            if !matches!(
                tokio::time::timeout(timeout, process.child.wait()).await,
                Ok(Ok(_))
            ) {
                tracing::warn!("activity hook command did not reap after termination");
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn command(arguments: &[&str]) -> ActivityHookConfig {
        ActivityHookConfig {
            command: arguments
                .iter()
                .map(|argument| (*argument).into())
                .collect(),
            timeout_seconds: 1,
            ..Default::default()
        }
    }

    #[test]
    fn writable_roots_and_symlinks_cannot_supply_the_hook() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("hook");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, mobius::owner_only::file()).unwrap();
        let config = command(&[executable.to_str().unwrap()]);
        assert!(config.validate().is_err());
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        config.validate().unwrap();
        assert!(config.validate_roots([directory.path()]).is_err());
        let link = directory.path().join("alias");
        std::os::unix::fs::symlink(&executable, &link).unwrap();
        assert!(
            command(&[link.to_str().unwrap()])
                .validate_roots([directory.path()])
                .is_err()
        );
    }

    #[tokio::test]
    async fn uncooperative_hook_is_killed_and_reaped_within_the_deadline() {
        let config = command(&["/bin/sh", "-c", "trap '' TERM; while :; do sleep 1; done"]);
        let mut hook = ActivityHook::new(Some(&config)).unwrap();
        hook.acquire().unwrap();
        let pid = i32::try_from(hook.process.as_ref().unwrap().child.id().unwrap()).unwrap();
        tokio::time::timeout(Duration::from_secs(4), hook.stop())
            .await
            .unwrap();
        assert_eq!(
            kill(Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        );
    }
}
