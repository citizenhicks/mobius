use super::*;

async fn sleeping_runtime(remote: &RemoteDesktop) -> u32 {
    let directory = remote.state_dir.join("desktop");
    fs::create_dir_all(directory.join("profile")).unwrap();
    fs::write(directory.join("profile/marker"), "retained").unwrap();
    let child = OwnedProcess::spawn(
        Command::new("/bin/sleep").arg("60"),
        directory.join("browser.json"),
    )
    .unwrap();
    let pid = child.child.id().unwrap();
    let socket = directory.join("rfb.sock");
    let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
    *remote.runtime.lock().await = Some(Runtime {
        children: vec![],
        browser: Some(child),
        chromium: PathBuf::new(),
        state_profile: directory.join("profile"),
        endpoint: String::new(),
        websocket: String::new(),
        tabs: BTreeMap::new(),
        display: None,
        authority: directory.join("Xauthority"),
        socket,
        #[cfg(target_os = "linux")]
        native: None,
    });
    pid
}

async fn wait_stopped(remote: &RemoteDesktop) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while remote.runtime.lock().await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn last_use_waits_for_execution_and_rechecks_a_new_consumer() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(
        directory.path(),
        true,
        ComputerConfig::default(),
    ));
    let agent = remote.acquire_use().await;
    sleeping_runtime(&remote).await;
    let execution = remote.execution_lease().await.unwrap();
    drop(agent);
    tokio::task::yield_now().await;
    assert!(remote.runtime.lock().await.is_some());
    let reopened = remote.acquire_use().await;
    drop(execution);
    // The queued stop writer runs before this new read lease.
    drop(remote.execution_lease().await.unwrap());
    assert!(remote.runtime.lock().await.is_some());
    drop(reopened);
    wait_stopped(&remote).await;
    drop(remote);
}

use mobius::backend::sandbox::{NetworkAccess, SandboxBackend, SandboxMode};

#[tokio::test]
async fn one_lease_blocks_all_other_evaluations_and_releases_on_disconnect() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(
        directory.path(),
        true,
        ComputerConfig::default(),
    ));
    let native = Arc::new(DesktopControl::default());
    let worker = remote
        .connect(&SessionId::new("one").unwrap(), Arc::clone(&native))
        .unwrap();
    assert!(
        remote
            .connect(&SessionId::new("two").unwrap(), Arc::clone(&native))
            .is_err()
    );
    drop(worker);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if remote.control.try_lock().is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        remote
            .connect(&SessionId::new("two").unwrap(), native)
            .is_ok()
    );
    drop(remote);
}

#[tokio::test]
async fn stale_socket_cleanup_refuses_live_and_unrelated_paths() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("file");
    fs::write(&file, "unrelated").unwrap();
    assert!(clean_socket(&file).await.is_err());
    assert!(file.exists());
    let socket = directory.path().join("socket");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    assert!(clean_socket(&socket).await.is_err());
    drop(listener);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !matches!(
            UnixStream::connect(&socket).await,
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    clean_socket(&socket).await.unwrap();
    assert!(!socket.exists());
}

#[tokio::test]
async fn hold_cancels_a_running_command_before_admitting_user_input() {
    use mobius::backend::sandbox::{CommandMode, CommandOutputSink};
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    let state = directory.path().join("state");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&state).unwrap();
    let remote = Arc::new(RemoteDesktop::new(&state, true, ComputerConfig::default()));
    let sandbox =
        crate::sandbox::GatewaySandbox::new(&workspace, &state, None, Duration::from_secs(20))
            .unwrap()
            .with_remote_desktop(Arc::clone(&remote));
    let running = tokio::spawn(async move {
        sandbox
            .execute(
                "touch started; sleep 10; touch completed",
                SandboxMode::DangerFullAccess,
                NetworkAccess::Allowed,
                CommandMode::Foreground,
                CommandOutputSink::default(),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !workspace.join("started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    remote.held.store(true, Ordering::Release);
    remote.notify_changed();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(remote.executions.try_write().is_ok());
    assert!(!workspace.join("completed").exists());
    remote.cancel_takeover().await;
    drop(remote);
}

#[tokio::test]
async fn dropping_a_pending_grant_never_starts_a_late_controller() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(
        directory.path(),
        true,
        ComputerConfig::default(),
    ));
    let execution = remote.execution_lease().await.unwrap();
    remote.held.store(true, Ordering::Release);
    let mut grant = Box::pin(remote.grant_control(Uuid::new_v4(), SessionId::new("chat").unwrap()));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut grant)
            .await
            .is_err()
    );
    drop(grant);
    drop(execution);
    remote.cancel_takeover().await;
    assert!(remote.runtime.lock().await.is_none());
    assert_eq!(remote.control_state(Uuid::new_v4()), (false, false, None));
    assert!(remote.check_execution().is_ok());
    drop(remote);
}
