use super::*;
use clap::CommandFactory as _;

fn reject_bootstrap_test_client(_endpoint: &Endpoint, _token: String) -> Result<()> {
    Err(Error::Config("test token save failed".into()))
}

#[cfg(unix)]
#[test]
fn reset_gateway_state_preserves_an_unrelated_nonempty_directory() {
    let root = tempfile::tempdir().expect("state parent");
    let state = root.path().join("not-mobius");
    std::fs::create_dir(&state).expect("unrelated directory");
    let unrelated = state.join("keep.txt");
    std::fs::write(&unrelated, "keep").expect("unrelated file");

    let error = reset_gateway_state(state).expect_err("unrelated state must be refused");

    assert!(error.to_string().contains("refusing to reset") && unrelated.exists());
}

#[cfg(unix)]
#[test]
fn reset_gateway_state_refuses_a_symlinked_directory() {
    let root = tempfile::tempdir().expect("state parent");
    let real = root.path().join("real");
    let link = root.path().join("gateway");
    std::fs::create_dir(&real).expect("real directory");
    std::fs::write(real.join(STATE_MARKER_FILE), "version = 999\n").expect("gateway marker");
    std::os::unix::fs::symlink(&real, &link).expect("gateway symlink");

    reset_gateway_state(link).expect_err("symlinked state must be refused");

    assert!(real.exists());
}

#[test]
fn failed_auth_initialization_removes_only_the_new_gateway_state() {
    let root = tempfile::tempdir().expect("state parent");
    let state = root.path().join("gateway");
    let sibling = root.path().join("keep");
    std::fs::write(&sibling, "keep").expect("sibling state");
    let (store, config) =
        ConfigStore::initialize(state.clone(), DEFAULT_LISTEN, None).expect("gateway config");
    std::fs::create_dir(store.auth_path()).expect("conflicting auth path");

    initialize_auth(&store, config.auth).expect_err("auth initialization must fail");

    assert_eq!((state.exists(), sibling.exists()), (false, true));
}

#[test]
fn bootstrap_cleans_state_when_the_control_token_cannot_be_saved() {
    let directory = tempfile::tempdir().expect("gateway state parent");
    let state = directory.path().join("gateway");
    let sibling = directory.path().join("keep");
    std::fs::write(&sibling, "keep").expect("sibling state");

    initialize_bootstrap(state.clone(), reject_bootstrap_test_client)
        .expect_err("token save must fail initialization");

    assert_eq!((state.exists(), sibling.exists()), (false, true));
}

#[test]
fn command_definition_is_valid() {
    GatewayCli::command().debug_assert();
}

#[test]
fn provider_credential_stdin_is_bounded() {
    assert_eq!(
        read_provider_credential(std::io::Cursor::new(b"secret\n"))
            .expect("read provider credential"),
        "secret\n"
    );
    assert!(
        read_provider_credential(std::io::Cursor::new(vec![
            b'x';
            crate::config::MAX_PROVIDER_API_KEY_BYTES
                + 1
        ]))
        .expect_err("oversized provider credential")
        .to_string()
        .contains("API key must be")
    );
}

#[cfg(unix)]
#[test]
fn parse_init_loads_a_private_cloudflare_token_without_debugging_it() {
    let token = tempfile::NamedTempFile::new().expect("token file");
    std::fs::write(token.path(), "secret-tunnel-token").expect("write token");
    token
        .as_file()
        .set_permissions(mobius::owner_only::file())
        .expect("secure token");
    let command = parse(vec![
        "init".into(),
        "--cloudflare-hostname".into(),
        "mobius.example.com".into(),
        "--cloudflare-token-file".into(),
        token.path().into(),
    ])
    .expect("parse Cloudflare init");

    assert!(!format!("{command:?}").contains("secret-tunnel-token"));
}

#[cfg(unix)]
async fn report_gateway_version(
    listener: tokio::net::TcpListener,
    version: Option<&str>,
    protocol: u16,
) {
    use crate::wire::{
        ClientFrame, FrameReader, PROTOCOL_VERSION, ServerFrame, read_frame, write_frame,
    };

    for expected_protocol in [PROTOCOL_VERSION, protocol] {
        let (stream, _) = listener.accept().await.expect("accept version check");
        let (reader, mut writer) = tokio::io::split(stream);
        let frame = read_frame::<ClientFrame>(&mut FrameReader::new(reader))
            .await
            .expect("read authentication")
            .expect("authentication frame");
        assert_eq!(frame.version, expected_protocol);
        assert_eq!(
            frame.message,
            ClientMessage::Authenticate {
                token: "version-check-token".into(),
                client_kind: ClientKind::GatewayDashboard,
                catalog: Default::default(),
            }
        );
        if frame.version != protocol {
            write_frame(
                &mut writer,
                &ServerFrame {
                    version: protocol,
                    message: ServerMessage::Error {
                        code: "protocol_version".into(),
                        message: "unsupported protocol version".into(),
                        fatal: true,
                    },
                },
            )
            .await
            .expect("reject newer protocol");
            continue;
        }
        write_frame(
            &mut writer,
            &ServerFrame {
                version: protocol,
                message: ServerMessage::Authenticated,
            },
        )
        .await
        .expect("authenticate client");
        if let Some(version) = version {
            write_frame(
                &mut writer,
                &serde_json::json!({
                    "version": protocol,
                    "type": "ready",
                    "payload": { "gateway_version": version },
                }),
            )
            .await
            .expect("report gateway version");
        }
        return;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn gateway_reuse_evicts_only_a_confirmed_older_release() {
    use crate::wire::PROTOCOL_VERSION;

    for (version, protocol, expected_reuse) in [
        (Some("0.0.0"), PROTOCOL_VERSION, Some(false)),
        (Some("0.15.46"), 84, Some(false)),
        (
            Some(env!("CARGO_PKG_VERSION")),
            PROTOCOL_VERSION,
            Some(true),
        ),
        (Some("999.0.0"), PROTOCOL_VERSION, Some(true)),
        (Some("unknown"), PROTOCOL_VERSION, None),
        (None, PROTOCOL_VERSION, None),
    ] {
        let directory = tempfile::tempdir().expect("gateway state parent");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind version peer");
        let (store, config) = ConfigStore::initialize(
            directory.path().join("gateway"),
            listener.local_addr().expect("version peer address"),
            None,
        )
        .expect("gateway config");
        let path = store.state_dir().join(PROCESS_FILE);
        let mut file = File::create(&path).expect("process record");
        file.lock().expect("lock process record");
        let mut child = TokioCommand::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .expect("disposable gateway process");
        let pid = child.id().expect("disposable process ID");
        serde_json::to_writer(
            &mut file,
            &ProcessRecord {
                pid,
                endpoint: None,
            },
        )
        .expect("write disposable process record");
        file.flush().expect("flush process record");
        let (cleanup, cleanup_requested) = tokio::sync::oneshot::channel();
        let stopped = tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status.map(|_| ()),
                _ = cleanup_requested => child.kill().await,
            };
            drop(file);
            status
        });
        let peer = tokio::spawn(async move {
            tokio::time::timeout(
                Duration::from_secs(5),
                report_gateway_version(listener, version, protocol),
            )
            .await
            .expect("version peer completes");
        });

        let result = reuse_current_gateway(&store, &config, None, |_| {
            Ok(Some("version-check-token".into()))
        })
        .await;
        let running = running_process_pid(&path);
        let _ = cleanup.send(());
        stopped.await.expect("process task").expect("reap process");
        peer.await.expect("version peer");

        assert_eq!(
            result.map(|process| process.is_some()).ok(),
            expected_reuse,
            "reported version {version:?}, protocol {protocol}"
        );
        assert_eq!(
            running.expect("check process lifetime"),
            (expected_reuse != Some(false)).then_some(pid),
            "reported version {version:?}, protocol {protocol}"
        );
    }
}

#[cfg(unix)]
#[test]
fn startup_cleanup_removes_only_an_unlocked_process_record() {
    let directory = tempfile::tempdir().expect("process record directory");
    let path = directory.path().join(PROCESS_FILE);
    std::fs::write(&path, b"{").expect("partial process record");

    remove_unlocked_process_record(&path);

    assert!(!path.exists());
    let guard = ProcessRecordGuard::create(directory.path(), None).expect("locked process record");
    remove_unlocked_process_record(&path);
    assert!(path.exists());
    drop(guard);
}

#[cfg(unix)]
#[test]
fn process_record_lock_tracks_the_gateway_lifetime() {
    let directory = tempfile::tempdir().expect("process record directory");
    let guard = ProcessRecordGuard::create(directory.path(), None).expect("process record");
    let (_, file) = open_process_record(&guard.path)
        .expect("read process record")
        .expect("process record");

    assert!(process_is_running(&file).expect("locked process record"));
    assert_eq!(
        running_process_pid(&guard.path).expect("running process ID"),
        Some(std::process::id())
    );
    drop(guard);
    assert!(!directory.path().join(PROCESS_FILE).exists());
}

#[cfg(unix)]
#[test]
fn startup_lock_allows_only_one_lifecycle_operation() {
    let directory = tempfile::tempdir().expect("startup directory");
    let guard = StartupGuard::create(directory.path()).expect("startup lock");

    let error = StartupGuard::create(directory.path()).expect_err("competing startup");

    assert!(error.to_string().contains("already in progress"));
    drop(guard);
    StartupGuard::create(directory.path()).expect("released startup lock");
}

#[cfg(unix)]
#[tokio::test]
async fn autostart_process_group_cleanup_kills_descendants() {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let directory = tempfile::tempdir().expect("process group directory");
    let mut command = TokioCommand::new("sh");
    command
        .arg("-c")
        .arg("sleep 30 & echo $!; wait")
        .stdout(std::process::Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().expect("process group child");
    let mut output = BufReader::new(child.stdout.take().expect("child stdout"));
    let mut pid = String::new();
    let ready = tokio::time::timeout(Duration::from_secs(5), output.read_line(&mut pid)).await;
    stop_background_child(&mut child, &directory.path().join(PROCESS_FILE)).await;
    ready.expect("descendant ready").expect("descendant PID");
    let descendant = pid.trim().parse::<u32>().expect("valid descendant PID");

    for _ in 0..20 {
        let alive = std::process::Command::new("kill")
            .args(["-0", &descendant.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("probe descendant")
            .success();
        if !alive {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("process-group descendant remained alive");
}

#[cfg(unix)]
#[test]
fn process_group_termination_ignores_reserved_pids() {
    terminate_process_group(0);
    terminate_process_group(1);
}

#[cfg(unix)]
#[tokio::test]
async fn set_runtime_preserves_omitted_policy_fields() {
    let root = tempfile::tempdir().unwrap();
    let state_dir = root.path().join("state");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = listener.local_addr().unwrap();
    drop(listener);
    let (store, mut config) = ConfigStore::initialize(state_dir.clone(), listen, None).unwrap();
    config.runtime.ingress = Some("0.0.0.0:8742".parse().unwrap());
    config.runtime.require_access_lease = true;
    config.runtime.access_grace_seconds = 30;
    config.runtime.storage_limit_bytes = Some(5368709120);
    store.save(&config).unwrap();
    run(
        vec![
            "--state-dir".into(),
            state_dir.into_os_string(),
            "set-runtime".into(),
            "--idle-exit-seconds".into(),
            "300".into(),
        ],
        |_, _| Ok(()),
        |_| Ok(None),
    )
    .await
    .unwrap();
    let (_, updated) = ConfigStore::open(store.state_dir().to_path_buf()).unwrap();
    assert_eq!(updated.runtime.idle_exit_seconds, 300);
    assert_eq!(updated.runtime.ingress, config.runtime.ingress);
    assert!(updated.runtime.require_access_lease);
    assert_eq!(updated.runtime.access_grace_seconds, 30);
    assert_eq!(
        updated.runtime.storage_limit_bytes,
        config.runtime.storage_limit_bytes
    );
    run(
        vec![
            "--state-dir".into(),
            store.state_dir().as_os_str().to_owned(),
            "set-runtime".into(),
            "--clear-ingress".into(),
            "--clear-storage-limit".into(),
        ],
        |_, _| Ok(()),
        |_| Ok(None),
    )
    .await
    .unwrap();
    let (_, cleared) = ConfigStore::open(store.state_dir().to_path_buf()).unwrap();
    assert_eq!(cleared.runtime.idle_exit_seconds, 300);
    assert_eq!(cleared.runtime.ingress, None);
    assert!(cleared.runtime.require_access_lease);
    assert_eq!(cleared.runtime.access_grace_seconds, 30);
    assert_eq!(cleared.runtime.storage_limit_bytes, None);
}
