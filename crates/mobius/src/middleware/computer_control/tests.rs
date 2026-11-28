use super::*;
use crate::backend::sandbox::{
    ApprovalPolicy, NetworkAccess, Sandbox, SandboxMode, SandboxPermissions,
};

fn runtime(worker: WorkerCommand) -> Arc<ComputerRuntime> {
    Arc::new(
        ComputerRuntime::new(worker, "/unused/computer-control.md".into())
            .expect("absolute runtime paths"),
    )
}

#[test]
fn installed_runtime_cannot_resolve_from_the_chat_workspace() {
    for (executable, documentation) in [
        ("node", "/installed/computer-control.md"),
        ("/installed/node", "computer-control.md"),
    ] {
        assert!(
            ComputerRuntime::new(
                WorkerCommand {
                    executable: executable.into(),
                    arguments: Vec::new()
                },
                documentation.into(),
            )
            .is_err()
        );
    }
}

#[tokio::test]
async fn browser_selection_requires_full_access_network_and_gateway_availability() {
    use crate::backend::sandbox::{
        CommandMode, CommandOutput, CommandOutputSink, DesktopBrowserPage, SandboxBackend,
        WorkerProcess,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct AvailableBrowser {
        requested: Arc<AtomicUsize>,
        available: bool,
    }
    impl SandboxBackend for AvailableBrowser {
        fn desktop_browser_page<'a>(
            &'a self,
            _: &'a crate::SessionId,
        ) -> BoxFuture<'a, Result<Option<DesktopBrowserPage>>> {
            self.requested.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(self.available.then(|| DesktopBrowserPage {
                    endpoint: "http://127.0.0.1:9222".into(),
                    target_id: Some("assigned".into()),
                }))
            })
        }
        fn start_worker(
            &self,
            command: &WorkerCommand,
            _: SandboxMode,
            _: NetworkAccess,
        ) -> Result<WorkerProcess> {
            WorkerProcess::spawn(
                tokio::process::Command::new(&command.executable).args(&command.arguments),
            )
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
    }
    let state = tempfile::tempdir().expect("state");
    // Echoes the evaluation request back as the observation's text.
    let echo = "import sys,struct,json; n=struct.unpack('>I',sys.stdin.buffer.read(4))[0]; request=sys.stdin.buffer.read(n).decode(); data=json.dumps({'content':[{'type':'text','text':request}],'is_error':False,'devtools':'http://127.0.0.1:9222'}).encode(); sys.stdout.buffer.write(struct.pack('>I',len(data))+data); sys.stdout.buffer.flush()";
    let tool = Evaluate {
        files: SessionFileStore::new(state.path(), None).into(),
        runtime: runtime(WorkerCommand {
            executable: "/usr/bin/python3".into(),
            arguments: vec!["-c".into(), echo.into()],
        }),
    };
    let requested = Arc::new(AtomicUsize::new(0));
    for available in [false, true] {
        let sandbox = Arc::new(Sandbox::new(
            Arc::new(AvailableBrowser {
                requested: Arc::clone(&requested),
                available,
            }),
            ApprovalPolicy::Ask,
        ));
        for mode in [SandboxMode::WorkspaceWrite, SandboxMode::DangerFullAccess] {
            for network in [NetworkAccess::Denied, NetworkAccess::Allowed] {
                let permissions = SandboxPermissions::restore(
                    &"session".parse().expect("session ID"),
                    mode,
                    network,
                    ["call".into()],
                )
                .for_call("call");
                let before = requested.load(Ordering::SeqCst);
                let result = tool
                    .call(
                        ToolContext::new(Arc::clone(&sandbox), permissions, "turn"),
                        serde_json::json!({"code":"await getPage()", "reset":true}),
                    )
                    .await
                    .expect("echoed request");
                let request: Value =
                    serde_json::from_str(&result.completed_content().text()).expect("request JSON");
                assert_eq!(request["code"], "await getPage()");
                let eligible =
                    mode == SandboxMode::DangerFullAccess && network == NetworkAccess::Allowed;
                assert_eq!(
                    requested.load(Ordering::SeqCst) - before,
                    usize::from(eligible)
                );
                assert_eq!(!request["desktop"].is_null(), eligible && available);
                if eligible && available {
                    assert_eq!(request["desktop"]["target_id"], "assigned");
                }
            }
        }
        sandbox
            .session_end(&"session".parse().expect("session ID"))
            .await
            .expect("worker cleanup");
    }
}
