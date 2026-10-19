//! Session-owned framed execution. Cancellation poisons the worker; actions are never replayed.

use std::collections::BTreeMap;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::{NetworkAccess, ProcessGroupGuard, SandboxBackend, SandboxMode, ToolPermissions};
use crate::{Error, Result};

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const HOST_REQUEST: u32 = 1 << 31;
const HOST_ERROR: u32 = 1 << 30;
const MAX_HOST_REPLY_BYTES: usize = 64 * 1024 * 1024;

/// Trusted runtime command supplied by the owning capability, never by tool arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerCommand {
    /// The executable.
    pub executable: PathBuf,
    /// The arguments.
    pub arguments: Vec<String>,
}

/// A backend-launched process with a length-prefixed binary stdin/stdout channel.
/// Dropping it closes the channel and kills its process group.
#[derive(Debug)]
pub struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    _group: ProcessGroupGuard,
    #[cfg(target_os = "macos")]
    _cleanup_lease: std::os::unix::net::UnixStream,
}

impl WorkerProcess {
    /// Launches a command already configured with the backend's filesystem and network policy.
    #[cfg(unix)]
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn spawn(command: &mut Command) -> Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true);
        #[cfg(target_os = "macos")]
        let (cleanup_lease, child_lease) = std::os::unix::net::UnixStream::pair()?;
        #[cfg(target_os = "macos")]
        command.stderr(Stdio::from(std::os::fd::OwnedFd::from(child_lease)));
        let mut child = command.spawn()?;
        let group = ProcessGroupGuard::new(&child)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Sandbox("worker stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Sandbox("worker stdout unavailable".into()))?;
        Ok(Self {
            child,
            stdin,
            stdout,
            _group: group,
            #[cfg(target_os = "macos")]
            _cleanup_lease: cleanup_lease,
        })
    }

    /// Persistent process isolation is unavailable on this platform.
    #[cfg(not(unix))]
    pub fn spawn(_: &mut Command) -> Result<Self> {
        Err(Error::Sandbox(
            "persistent execution is unavailable on this platform".into(),
        ))
    }

    async fn exchange(
        &mut self,
        request: &[u8],
        backend: &dyn SandboxBackend,
        permissions: &ToolPermissions,
    ) -> Result<Vec<u8>> {
        if self.child.try_wait()?.is_some() {
            return Err(state_lost());
        }
        self.stdin
            .write_u32(
                u32::try_from(request.len()).map_err(|error| {
                    Error::Sandbox(format!("worker request too large: {error}"))
                })?,
            )
            .await?;
        self.stdin.write_all(request).await?;
        self.stdin.flush().await?;
        let mut host: Option<tokio::io::DuplexStream> = None;
        loop {
            let header = self.stdout.read_u32().await?;
            let size = (header & !HOST_REQUEST) as usize;
            if size == 0 || size > MAX_FRAME_BYTES {
                return Err(Error::Sandbox(
                    "worker response exceeded its frame limit".into(),
                ));
            }
            let mut response = vec![0; size];
            self.stdout.read_exact(&mut response).await?;
            if header & HOST_REQUEST == 0 {
                return Ok(response);
            }
            if host.is_none() {
                match backend
                    .worker_connection(
                        &permissions.session_id,
                        permissions.sandbox_mode,
                        permissions.network_access,
                    )
                    .await
                {
                    Ok(connection) => host = Some(connection),
                    Err(error) => {
                        let message = error.to_string();
                        self.host_reply(message.as_bytes(), HOST_ERROR).await?;
                        continue;
                    }
                }
            }
            let connection = host.as_mut().ok_or_else(state_lost)?;
            connection
                .write_u32(u32::try_from(response.len()).map_err(|error| state_lost_by(&error))?)
                .await?;
            connection.write_all(&response).await?;
            connection.flush().await?;
            let size = connection.read_u32().await? as usize;
            if size == 0 || size > MAX_HOST_REPLY_BYTES {
                return Err(Error::Sandbox(
                    "host response exceeded its frame limit".into(),
                ));
            }
            let mut response = vec![0; size];
            connection.read_exact(&mut response).await?;
            self.host_reply(&response, 0).await?;
        }
    }

    async fn host_reply(&mut self, response: &[u8], flags: u32) -> Result<()> {
        self.stdin
            .write_u32(
                HOST_REQUEST
                    | flags
                    | u32::try_from(response.len()).map_err(|error| state_lost_by(&error))?,
            )
            .await?;
        self.stdin.write_all(response).await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

struct Entry {
    command: WorkerCommand,
    sandbox_mode: SandboxMode,
    network_access: NetworkAccess,
    process: Option<WorkerProcess>,
}

#[derive(Default)]
pub(super) struct Workers {
    // ponytail: all sessions serialize through one map lock; per-session locks if concurrency matters.
    entries: Mutex<BTreeMap<Arc<str>, Entry>>,
}

impl Workers {
    pub(super) async fn evaluate(
        &self,
        backend: &dyn SandboxBackend,
        command: &WorkerCommand,
        permissions: &ToolPermissions,
        request: &[u8],
        timeout: Duration,
        reset: bool,
    ) -> Result<Vec<u8>> {
        if !permissions.mutation {
            return Err(Error::Sandbox(
                "evaluation requires mutation authority".into(),
            ));
        }
        if request.is_empty()
            || request.len() > MAX_FRAME_BYTES
            || timeout.is_zero()
            || timeout > Duration::from_secs(120)
        {
            return Err(Error::Sandbox("invalid worker request or timeout".into()));
        }
        let mut entries = self.entries.lock().await;
        if reset {
            entries.remove(&permissions.session_id);
        }
        if !entries.contains_key(&permissions.session_id) {
            if entries.len() >= 4 {
                return Err(Error::Sandbox("worker session limit reached".into()));
            }
            let process = backend.start_worker(
                command,
                permissions.sandbox_mode,
                permissions.network_access,
            )?;
            entries.insert(
                Arc::clone(&permissions.session_id),
                Entry {
                    // The persistent worker retains its launch contract to reject policy drift on later requests.
                    command: command.clone(), // rules-allow: the worker keeps its launch contract
                    sandbox_mode: permissions.sandbox_mode,
                    network_access: permissions.network_access,
                    process: Some(process),
                },
            );
        }
        let entry = entries
            .get_mut(&permissions.session_id)
            .ok_or_else(state_lost)?;
        if entry.command != *command
            || entry.sandbox_mode != permissions.sandbox_mode
            || entry.network_access != permissions.network_access
        {
            entry.process = None;
            return Err(Error::Sandbox(
                "runtime policy changed; session state: lost; reset the worker explicitly".into(),
            ));
        }
        // Leave a tombstone before writing. Dropping this future destroys the process and
        // the next call reports state loss, even when the action may already have completed.
        let mut process = entry.process.take().ok_or_else(state_lost)?;
        let response = tokio::time::timeout(timeout, process.exchange(request, backend, permissions)).await
            .map_err(|elapsed| Error::Sandbox(format!("overall evaluation deadline expired ({elapsed}); session state: lost (worker terminated); action outcome unknown; reset explicitly before continuing; do not repeat automatically")))?
            .map_err(|error| Error::Sandbox(format!("worker/interpreter lost; session state: lost; action outcome unknown; reset before continuing: {error}")))?;
        entry.process = Some(process);
        drop(entries);
        Ok(response)
    }

    pub(super) async fn shutdown(&self, session_id: &str) {
        self.entries.lock().await.remove(session_id);
    }
}

const STATE_LOST: &str = "worker state lost; session state: lost; the last action may have completed; inspect the environment and reset explicitly before continuing";

fn state_lost() -> Error {
    Error::Sandbox(STATE_LOST.into())
}

fn state_lost_by(error: &dyn std::fmt::Display) -> Error {
    Error::Sandbox(format!("{STATE_LOST}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BoxFuture;
    use crate::backend::sandbox::{
        ApprovalPolicy, CommandMode, CommandOutput, CommandOutputSink, Sandbox, SandboxPermissions,
    };
    use std::sync::Arc;

    struct TestBackend;
    impl SandboxBackend for TestBackend {
        fn worker_connection<'a>(
            &'a self,
            _: &'a str,
            mode: SandboxMode,
            _: NetworkAccess,
        ) -> BoxFuture<'a, Result<tokio::io::DuplexStream>> {
            Box::pin(async move {
                if mode != SandboxMode::DangerFullAccess {
                    return Err(Error::Sandbox("native access denied".into()));
                }
                let (worker, mut host) = tokio::io::duplex(4096);
                tokio::spawn(async move {
                    while let Ok(size) = host.read_u32().await {
                        let mut request = vec![0; size as usize];
                        if host.read_exact(&mut request).await.is_err() {
                            break;
                        }
                        let reply = vec![b'x'; 2 * MAX_FRAME_BYTES];
                        if host
                            .write_u32(u32::try_from(reply.len()).expect("reply length fits"))
                            .await
                            .is_err()
                            || host.write_all(&reply).await.is_err()
                        {
                            break;
                        }
                    }
                });
                Ok(worker)
            })
        }

        fn read<'a>(&'a self, _: &'a str, _: SandboxMode) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { unreachable!() })
        }
        fn read_bytes<'a>(
            &'a self,
            _: &'a str,
            _: usize,
            _: SandboxMode,
        ) -> BoxFuture<'a, Result<Vec<u8>>> {
            Box::pin(async { unreachable!() })
        }
        fn write<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: SandboxMode,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { unreachable!() })
        }
        fn execute<'a>(
            &'a self,
            _: &'a str,
            _: SandboxMode,
            _: NetworkAccess,
            _: CommandMode,
            _: CommandOutputSink,
        ) -> BoxFuture<'a, Result<CommandOutput>> {
            Box::pin(async { unreachable!() })
        }
        fn start_worker(
            &self,
            spec: &WorkerCommand,
            _: SandboxMode,
            _: NetworkAccess,
        ) -> Result<WorkerProcess> {
            WorkerProcess::spawn(Command::new(&spec.executable).args(&spec.arguments))
        }
    }

    #[tokio::test]
    async fn worker_authority_state_loss_and_reset_never_replay_an_uncertain_action() {
        let state = tempfile::tempdir().expect("state");
        let marker = state.path().join("actions");
        std::fs::write(&marker, "").expect("action log");
        let script = r"import sys, struct, time
count = 0
while True:
    header = sys.stdin.buffer.read(4)
    if not header: break
    request = sys.stdin.buffer.read(struct.unpack('>I', header)[0])
    count += 1
    if request == b'wait':
        with open(sys.argv[1], 'a') as marker: marker.write('action\n')
        time.sleep(5)
    response = str(count).encode()
    sys.stdout.buffer.write(struct.pack('>I', len(response)) + response)
    sys.stdout.buffer.flush()
";
        let command = WorkerCommand {
            executable: "/usr/bin/python3".into(),
            arguments: vec![
                "-u".into(),
                "-c".into(),
                script.into(),
                marker.to_str().expect("path").into(),
            ],
        };
        let sandbox = Sandbox::new(Arc::new(TestBackend), ApprovalPolicy::Ask);
        // Python's cold start on macOS CI can exceed a second; only `wait` tests a deadline.
        let response_timeout = Duration::from_secs(10);
        let denied = SandboxPermissions::restore(
            "session",
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            [],
        )
        .for_call("call");
        let allowed = SandboxPermissions::restore(
            "session",
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            ["call".into()],
        )
        .for_call("call");
        assert!(
            sandbox
                .evaluate_worker(&command, &denied, b"run", Duration::from_secs(1), false)
                .await
                .is_err()
        );
        assert_eq!(
            sandbox
                .evaluate_worker(&command, &allowed, b"run", response_timeout, false)
                .await
                .expect("first"),
            b"1"
        );
        assert_eq!(
            sandbox
                .evaluate_worker(&command, &allowed, b"run", response_timeout, false)
                .await
                .expect("second"),
            b"2"
        );
        let error = sandbox
            .evaluate_worker(
                &command,
                &allowed,
                b"wait",
                Duration::from_millis(100),
                false,
            )
            .await
            .expect_err("timeout");
        assert!(
            error
                .to_string()
                .contains("overall evaluation deadline expired")
        );
        assert!(error.to_string().contains("session state: lost"));
        assert!(error.to_string().contains("outcome unknown"));
        assert!(
            sandbox
                .evaluate_worker(&command, &allowed, b"wait", Duration::from_secs(1), false)
                .await
                .expect_err("explicit reset required")
                .to_string()
                .contains("state lost")
        );
        // The deadline can expire before the request executes; neither outcome may be replayed.
        assert!(matches!(
            std::fs::read_to_string(&marker).expect("actions").as_str(),
            "" | "action\n"
        ));
        assert_eq!(
            sandbox
                .evaluate_worker(&command, &allowed, b"run", response_timeout, true)
                .await
                .expect("reset"),
            b"1"
        );
        // Cancellation while awaiting a response has the same tombstone as timeout.
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                sandbox.evaluate_worker(&command, &allowed, b"wait", Duration::from_secs(2), false)
            )
            .await
            .is_err()
        );
        assert!(
            sandbox
                .evaluate_worker(&command, &allowed, b"run", Duration::from_secs(1), false)
                .await
                .is_err()
        );
        assert!(matches!(
            std::fs::read_to_string(marker).expect("actions").as_str(),
            "" | "action\n" | "action\naction\n"
        ));
    }
}
