use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use serde_json::json;
use tokio::sync::Notify;

use super::*;
use crate::backend::sandbox::CommandAuthorization;

struct FakeSandbox {
    commands: Mutex<Vec<String>>,
    started: AtomicUsize,
    release: Notify,
}

impl FakeSandbox {
    fn new() -> Self {
        Self {
            commands: Mutex::new(Vec::new()),
            started: AtomicUsize::new(0),
            release: Notify::new(),
        }
    }

    fn launch(&self, command: &str) -> bool {
        self.commands.lock().expect("commands").push(command.into());
        let second = self.started.fetch_add(1, Ordering::SeqCst) + 1 == 2;
        if second {
            self.release.notify_waiters();
        }
        second
    }

    fn output(command: &str) -> CommandOutput {
        let stdout = if command.contains("/bin/sh -c 'first' <") {
            r#"{"systemMessage":"first"}"#
        } else {
            r#"{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"second","deliverOnce":"guidance"}}"#
        };
        CommandOutput {
            exit_code: 0,
            stdout: stdout.into(),
            stdout_truncated: false,
            stderr: String::new(),
            stderr_truncated: false,
        }
    }
}

impl SandboxBackend for FakeSandbox {
    fn read<'a>(
        &'a self,
        _path: &'a str,
        _sandbox_mode: SandboxMode,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async { Err(Error::Sandbox("unexpected read".into())) })
    }

    fn read_bytes<'a>(
        &'a self,
        _path: &'a str,
        _max_bytes: usize,
        _sandbox_mode: SandboxMode,
    ) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async { Err(Error::Sandbox("unexpected read".into())) })
    }

    fn write<'a>(
        &'a self,
        _path: &'a str,
        _content: &'a str,
        _sandbox_mode: SandboxMode,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(Error::Sandbox("unexpected write".into())) })
    }

    fn execute<'a>(
        &'a self,
        command: &'a str,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
        mode: CommandMode,
        _output: CommandOutputSink,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        Box::pin(async move {
            assert_eq!(sandbox_mode, SandboxMode::WorkspaceWrite);
            assert_eq!(network_access, NetworkAccess::Denied);
            assert_eq!(mode, CommandMode::Foreground);
            let released = self.release.notified();
            if !self.launch(command) {
                released.await;
            }
            Ok(Self::output(command))
        })
    }

    fn execute_authorized<'a>(
        &'a self,
        command: &'a str,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
        mode: CommandMode,
        _output: CommandOutputSink,
        authorization: &'a CommandAuthorization,
    ) -> BoxFuture<'a, Result<Option<CommandOutput>>> {
        Box::pin(async move {
            assert_eq!(sandbox_mode, SandboxMode::WorkspaceWrite);
            assert_eq!(network_access, NetworkAccess::Denied);
            assert_eq!(mode, CommandMode::Foreground);
            let released = self.release.notified();
            let mut second = None;
            authorization(&mut || {
                second = Some(self.launch(command));
                Ok(())
            })?;
            let Some(second) = second else {
                return Ok(None);
            };
            if !second {
                released.await;
            }
            Ok(Some(Self::output(command)))
        })
    }
}

#[test]
fn pre_tool_decision_block_is_a_denial() {
    let outcome = parse_output(
        HookEvent::PreToolUse,
        DEFAULT_CONTEXT_BYTES,
        CommandOutput {
            exit_code: 0,
            stdout: r#"{"decision":"block","reason":"unsafe"}"#.into(),
            stdout_truncated: false,
            stderr: String::new(),
            stderr_truncated: false,
        },
    )
    .expect("decision block");

    assert_eq!(outcome.decision, Some(HookDecision::Block));
    assert_eq!(outcome.reason.as_deref(), Some("unsafe"));
}

#[test]
fn nonzero_informational_hook_exit_is_a_failure_not_a_block() {
    let outcome = parse_output(
        HookEvent::SessionStart,
        DEFAULT_CONTEXT_BYTES,
        CommandOutput {
            exit_code: 2,
            stdout: String::new(),
            stdout_truncated: false,
            stderr: "informational failure".into(),
            stderr_truncated: false,
        },
    )
    .expect("nonzero informational hook");

    assert!(outcome.failure.is_some());
    assert_eq!(outcome.decision, None);
}

#[tokio::test]
async fn runner_starts_matches_concurrently_and_preserves_declaration_order() {
    let workspace = tempfile::tempdir().expect("workspace");
    let root = tempfile::tempdir().expect("plugin root");
    let data = tempfile::tempdir().expect("plugin data");
    let hooks = json!({
        "SessionStart": [{
            "matcher": "startup|resume",
            "hooks": [
                {"type":"command","command":"first","timeout":1},
                {"type":"command","command":"second","timeout":1}
            ]
        }]
    });
    let set = HookSet::load(root.path().into(), data.path(), Some(&hooks)).expect("hook set");
    let permitted = Arc::new(AtomicBool::new(true));
    let gate = Arc::clone(&permitted);
    let set = AuthorizedHooks {
        set,
        authorization: Arc::new(move |launch| {
            if gate.load(Ordering::SeqCst) {
                launch()?;
            }
            Ok(())
        }),
    };
    let sets = [set];
    let backend = Arc::new(FakeSandbox::new());
    let runtime =
        HookRuntime::new(Arc::<FakeSandbox>::clone(&backend), workspace.path()).expect("runtime");
    let outcomes = runtime
        .run_all(
            &sets,
            HookEvent::SessionStart,
            json!({"session_id":"thread","source":"startup","prompt":"it's safe"}),
            &["startup"],
        )
        .await
        .expect("outcomes");

    assert_eq!(backend.started.load(Ordering::SeqCst), 2);
    assert_eq!(outcomes[0].system_message.as_deref(), Some("first"));
    assert_eq!(outcomes[1].additional_context.as_deref(), Some("second"));
    assert_eq!(outcomes[1].deliver_once.as_deref(), Some("test:guidance"));
    assert!(
        backend
            .commands
            .lock()
            .expect("commands")
            .iter()
            .all(|command| command.contains(" < '") && command.contains("PLUGIN_ROOT='"))
    );

    permitted.store(false, Ordering::SeqCst);
    runtime
        .run_all(
            &sets,
            HookEvent::SessionStart,
            json!({"session_id":"thread","source":"startup"}),
            &["startup"],
        )
        .await
        .expect("revoked outcomes");
    assert_eq!(backend.started.load(Ordering::SeqCst), 2);
}
