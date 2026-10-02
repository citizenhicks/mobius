use super::*;

#[cfg(target_os = "macos")]
#[tokio::test]
async fn local_browser_without_a_renderer_falls_back_without_starting_chromium() {
    let directory = tempfile::tempdir().unwrap();
    let remote = RemoteDesktop::new(directory.path(), true);
    assert!(remote.page("chat").await.unwrap().is_none());
    assert!(remote.runtime.lock().await.is_none());
}

#[test]
fn tigervnc_input_readback_accepts_canonical_booleans() {
    for value in [b"on\n".as_slice(), b"1\n"] {
        assert_eq!(input_enabled(value), Some(true));
    }
    for value in [b"off\n".as_slice(), b"0\n"] {
        assert_eq!(input_enabled(value), Some(false));
    }
    assert_eq!(input_enabled(b"unexpected\n"), None);
}

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
        profile: directory.join("profile"),
        endpoint: String::new(),
        websocket: String::new(),
        tabs: BTreeMap::new(),
        display: None,
        authority: directory.join("Xauthority"),
        socket,
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
async fn closing_the_viewer_keeps_the_agent_runtime_until_its_last_use_ends() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(directory.path(), true));
    let agent = remote.acquire_use().await;
    let pid = sleeping_runtime(&remote).await;
    let (socket, _peer) = UnixStream::pair().unwrap();
    let viewer = DesktopStream {
        remote: Arc::clone(&remote),
        connection: Uuid::new_v4(),
        socket,
        _use: remote.acquire_use().await,
    };
    drop(viewer);
    tokio::task::yield_now().await;
    assert_eq!(
        remote
            .runtime
            .lock()
            .await
            .as_ref()
            .unwrap()
            .browser
            .as_ref()
            .unwrap()
            .child
            .id(),
        Some(pid)
    );
    drop(agent);
    wait_stopped(&remote).await;
    assert!(!directory.path().join("desktop/browser.json").exists());
    assert!(!directory.path().join("desktop/rfb.sock").exists());
    assert_eq!(
        fs::read_to_string(directory.path().join("desktop/profile/marker")).unwrap(),
        "retained"
    );
}

#[tokio::test]
async fn last_use_waits_for_execution_and_rechecks_a_new_consumer() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(directory.path(), true));
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
}

#[tokio::test]
async fn pending_takeover_keeps_an_unused_runtime_until_hold_is_cleared() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(directory.path(), true));
    let agent = remote.acquire_use().await;
    sleeping_runtime(&remote).await;
    remote.held.store(true, Ordering::Release);
    drop(agent);
    tokio::task::yield_now().await;
    drop(Arc::clone(&remote.executions).read_owned().await);
    assert!(remote.runtime.lock().await.is_some());
    remote.held.store(false, Ordering::Release);
    remote.stop_if_unused();
    wait_stopped(&remote).await;
}

#[tokio::test]
async fn last_use_waits_for_a_background_command_but_not_its_unpolled_result() {
    use mobius::backend::sandbox::{CommandMode, CommandOutputSink};
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    let state = directory.path().join("state");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&state).unwrap();
    let remote = Arc::new(RemoteDesktop::new(&state, true));
    let sandbox = Arc::new(
        crate::sandbox::GatewaySandbox::new(&workspace, &state, None, Duration::from_secs(5))
            .unwrap()
            .with_remote_desktop(Arc::clone(&remote)),
    );
    sandbox.retain_desktop_use_for_test().await;
    sleeping_runtime(&remote).await;
    let command_sandbox = Arc::clone(&sandbox);
    let running = tokio::spawn(async move {
        command_sandbox
            .execute(
                "touch started; while [ ! -f finish ]; do sleep 0.01; done",
                SandboxMode::DangerFullAccess,
                NetworkAccess::Allowed,
                CommandMode::Background,
                CommandOutputSink::default(),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !workspace.join("started").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    sandbox.release_desktop_use().await;
    tokio::task::yield_now().await;
    assert!(remote.runtime.lock().await.is_some());
    fs::write(workspace.join("finish"), "finish").unwrap();
    wait_stopped(&remote).await;
    assert!(running.is_finished());
    assert_eq!(running.await.unwrap().unwrap().exit_code, 0);
}

#[tokio::test]
async fn failed_browser_is_reported_promptly_and_partial_children_are_reaped() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    fs::create_dir(&profile).unwrap();
    fs::write(profile.join("marker"), "retained").unwrap();
    let record = directory.path().join("browser.json");
    let mut browser =
        OwnedProcess::spawn(Command::new("/bin/sleep").arg("60"), record.clone()).unwrap();
    browser.child.start_kill().unwrap();
    browser.child.wait().await.unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        wait_devtools(&profile.join("DevToolsActivePort"), &mut browser),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("desktop browser exited"));

    let sibling_record = directory.path().join("xvnc.json");
    let sibling =
        OwnedProcess::spawn(Command::new("/bin/sleep").arg("60"), sibling_record.clone()).unwrap();
    let pid = sibling.child.id().unwrap();
    let socket = directory.path().join("rfb.sock");
    let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
    stop_children(vec![sibling, browser], &socket).await;

    assert!(!record.exists());
    assert!(!sibling_record.exists());
    assert!(!socket.exists());
    assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err());
    assert_eq!(
        fs::read_to_string(profile.join("marker")).unwrap(),
        "retained"
    );
}

#[tokio::test]
async fn inactive_stream_drop_does_not_release_a_later_control_grant() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(directory.path(), true));
    let connection = Uuid::new_v4();
    let (socket, _peer) = UnixStream::pair().unwrap();
    drop(DesktopStream {
        remote: Arc::clone(&remote),
        connection,
        socket,
        _use: remote.acquire_use().await,
    });
    *remote.user.lock().unwrap() = Some(UserControl {
        connection,
        session_id: "chat".into(),
        _lease: Arc::clone(&remote.control).lock_owned().await,
        _execution: Arc::clone(&remote.executions).write_owned().await,
    });
    remote.held.store(true, Ordering::Release);
    tokio::task::yield_now().await;
    assert_eq!(
        remote.control_state(connection),
        (true, true, Some("chat".into()))
    );
}

#[tokio::test]
async fn failed_release_notifies_chat_connections_that_execution_resumed() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(directory.path(), true));
    let connection = Uuid::new_v4();
    *remote.user.lock().unwrap() = Some(UserControl {
        connection,
        session_id: "chat".into(),
        _lease: Arc::clone(&remote.control).lock_owned().await,
        _execution: Arc::clone(&remote.executions).write_owned().await,
    });
    remote.held.store(true, Ordering::Release);
    let mut changed = remote.subscribe();
    assert!(remote.release_control(connection).await.is_err());
    assert!(remote.check_execution().is_ok());
    assert_eq!(remote.control_state(connection), (false, false, None));
    changed.try_recv().unwrap();
}
use mobius::backend::sandbox::{NetworkAccess, SandboxBackend, SandboxMode};

#[tokio::test]
async fn one_lease_blocks_all_other_evaluations_and_releases_on_disconnect() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(directory.path(), true));
    let native = Arc::new(DesktopControl::default());
    let worker = remote.connect("one", Arc::clone(&native)).unwrap();
    assert!(remote.connect("two", Arc::clone(&native)).is_err());
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
    assert!(remote.connect("two", native).is_ok());
}

#[tokio::test]
async fn execution_hold_covers_reads_writes_commands_workers_and_helpers() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    let state = directory.path().join("state");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&state).unwrap();
    let remote = Arc::new(RemoteDesktop::new(&state, true));
    let sandbox =
        crate::sandbox::GatewaySandbox::new(&workspace, &state, None, Duration::from_secs(5))
            .unwrap()
            .with_remote_desktop(Arc::clone(&remote));
    remote.held.store(true, Ordering::Release);
    assert!(sandbox.check_execution().is_err());
    assert!(
        sandbox
            .read("file", SandboxMode::DangerFullAccess)
            .await
            .is_err()
    );
    assert!(
        sandbox
            .write("file", "changed", SandboxMode::DangerFullAccess)
            .await
            .is_err()
    );
    assert!(!workspace.join("file").exists());
    assert!(
        sandbox
            .worker_connection(
                "session",
                SandboxMode::DangerFullAccess,
                NetworkAccess::Allowed
            )
            .await
            .is_err()
    );
    remote.cancel_takeover().await;
    assert!(sandbox.check_execution().is_ok());
}

#[tokio::test]
async fn disabled_desktop_does_not_start_or_lease_a_browser() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(directory.path(), false));
    assert!(remote.page("chat").await.unwrap().is_none());
    assert!(remote.runtime.lock().await.is_none());
    assert!(!remote.available());
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
    tokio::task::yield_now().await;
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
    let remote = Arc::new(RemoteDesktop::new(&state, true));
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
    let _ = remote.changed.send(());
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
}

#[tokio::test]
async fn cancelled_takeover_keeps_hold_until_execution_has_drained() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(directory.path(), true));
    let execution = remote.execution_lease().await.unwrap();
    remote.held.store(true, Ordering::Release);
    let connection = Uuid::new_v4();
    assert_eq!(remote.control_state(connection), (true, false, None));
    let owner = Arc::clone(&remote);
    let mut cancellation = tokio::spawn(async move {
        owner.cancel_takeover().await;
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut cancellation)
            .await
            .is_err()
    );
    assert!(remote.check_execution().is_err());
    drop(execution);
    tokio::time::timeout(Duration::from_secs(1), cancellation)
        .await
        .unwrap()
        .unwrap();
    assert!(remote.check_execution().is_ok());
    assert_eq!(remote.control_state(connection), (false, false, None));
}

#[tokio::test]
async fn dropping_a_pending_grant_never_starts_a_late_controller() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(directory.path(), true));
    let execution = remote.execution_lease().await.unwrap();
    remote.held.store(true, Ordering::Release);
    let mut grant = Box::pin(remote.grant_control(Uuid::new_v4(), "chat".into()));
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
}

#[tokio::test]
async fn closed_browser_does_not_stop_the_native_desktop_or_retain_stale_assignments() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(directory.path(), true));
    sleeping_runtime(&remote).await;
    let path = directory.path().join("desktop");
    let display =
        OwnedProcess::spawn(Command::new("/bin/sleep").arg("60"), path.join("xvnc.json")).unwrap();
    let pid = display.child.id().unwrap();
    {
        let mut runtime = remote.runtime.lock().await;
        let runtime = runtime.as_mut().unwrap();
        runtime.children.push(display);
        runtime
            .browser
            .as_mut()
            .unwrap()
            .child
            .start_kill()
            .unwrap();
        runtime
            .browser
            .as_mut()
            .unwrap()
            .child
            .wait()
            .await
            .unwrap();
        runtime.tabs.insert("chat".into(), "old-target".into());
        runtime.endpoint = "old-endpoint".into();
        runtime.websocket = "old-websocket".into();
        runtime.show("chat").await.unwrap();
        assert!(runtime.browser.is_none());
        assert!(runtime.tabs.is_empty());
        assert!(runtime.endpoint.is_empty());
        assert_eq!(runtime.children[0].child.id(), Some(pid));
        runtime.children[0].check_running().unwrap();
    }
    assert!(!path.join("browser.json").exists());
    assert!(path.join("rfb.sock").exists());
    remote.shutdown().await;
    assert!(!path.join("xvnc.json").exists());
    assert!(!path.join("rfb.sock").exists());
}

#[tokio::test]
async fn a_reopened_browser_endpoint_discards_old_chat_assignments_without_another_launch() {
    let directory = tempfile::tempdir().unwrap();
    let remote = Arc::new(RemoteDesktop::new(directory.path(), true));
    sleeping_runtime(&remote).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        let request = socket.next().await.unwrap().unwrap();
        let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
        assert_eq!(request["method"], "Browser.getVersion");
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"id":1,"result":{}}).to_string().into(),
            ))
            .await
            .unwrap();
    });
    {
        let mut runtime = remote.runtime.lock().await;
        let runtime = runtime.as_mut().unwrap();
        runtime
            .browser
            .as_mut()
            .unwrap()
            .child
            .start_kill()
            .unwrap();
        runtime
            .browser
            .as_mut()
            .unwrap()
            .child
            .wait()
            .await
            .unwrap();
        runtime.websocket = "old-websocket".into();
        runtime.tabs.insert("chat".into(), "old-target".into());
        fs::write(
            runtime.profile.join("DevToolsActivePort"),
            format!("{}\n/devtools/browser/reopened\n", address.port()),
        )
        .unwrap();
        runtime.ensure_browser().await.unwrap();
        assert!(runtime.browser.is_none());
        assert!(runtime.tabs.is_empty());
        assert_eq!(
            runtime.endpoint,
            format!("http://127.0.0.1:{}", address.port())
        );
        assert_eq!(
            runtime.websocket,
            format!(
                "ws://127.0.0.1:{}/devtools/browser/reopened",
                address.port()
            )
        );
    }
    server.await.unwrap();
    remote.shutdown().await;
}
