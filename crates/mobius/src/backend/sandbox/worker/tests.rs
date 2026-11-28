use super::*;
use crate::BoxFuture;
use crate::backend::sandbox::{ApprovalPolicy, Sandbox, SandboxPermissions};
use std::sync::Arc;

struct TestBackend;
impl SandboxBackend for TestBackend {
    fn worker_connection<'a>(
        &'a self,
        _: &'a SessionId,
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

    crate::backend::sandbox::unreachable_backend_methods!(execute);

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
    let allowed = SandboxPermissions::restore(
        &"session".parse().expect("session ID"),
        SandboxMode::WorkspaceWrite,
        NetworkAccess::Denied,
        ["call".into()],
    )
    .for_call("call");
    let allowed = allowed.mutation().expect("mutation permit");
    assert_eq!(
        sandbox
            .evaluate_worker(&command, allowed, b"run", response_timeout, false)
            .await
            .expect("first"),
        b"1"
    );
    assert_eq!(
        sandbox
            .evaluate_worker(&command, allowed, b"run", response_timeout, false)
            .await
            .expect("second"),
        b"2"
    );
    let error = sandbox
        .evaluate_worker(
            &command,
            allowed,
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
            .evaluate_worker(&command, allowed, b"wait", Duration::from_secs(1), false)
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
            .evaluate_worker(&command, allowed, b"run", response_timeout, true)
            .await
            .expect("reset"),
        b"1"
    );
    // Cancellation while awaiting a response has the same tombstone as timeout.
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            sandbox.evaluate_worker(&command, allowed, b"wait", Duration::from_secs(2), false)
        )
        .await
        .is_err()
    );
    assert!(
        sandbox
            .evaluate_worker(&command, allowed, b"run", Duration::from_secs(1), false)
            .await
            .is_err()
    );
    assert!(matches!(
        std::fs::read_to_string(marker).expect("actions").as_str(),
        "" | "action\n" | "action\naction\n"
    ));
}
