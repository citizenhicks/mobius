use super::*;
use serde_json::json;

#[tokio::test]
async fn desktop_registration_rejects_remote_and_non_macos_clients() {
    use crate::wire::{ClientKind, ClientMessage, FrameReader, ServerFrame, read_frame};

    for (local, kind) in [(false, ClientKind::Macos), (true, ClientKind::Ios)] {
        let control = Arc::new(DesktopControl::default());
        let mut connection = None;
        let (reader, mut writer) = tokio::io::duplex(4096);
        handle_message(
            ClientMessage::SetDesktopRuntime {
                request_id: "register".into(),
                enabled: true,
            },
            &control,
            &mut connection,
            local,
            kind,
            &mut writer,
        )
        .await
        .expect("registration handled");
        let frame = read_frame::<ServerFrame>(&mut FrameReader::new(reader))
            .await
            .expect("response frame")
            .expect("registration response");
        assert!(
            matches!(frame.message, ServerMessage::Rejected { request_id, code, fatal: false, .. }
            if request_id == "register" && code == "desktop_control")
        );
        assert!(connection.is_none());
        assert!(control.connect("session").is_err());
    }
}

#[tokio::test]
async fn desktop_serializes_evaluations_and_rejects_stale_replies() {
    let control = Arc::new(DesktopControl::default());
    assert!(control.connect("session").is_err());
    let mut app = control.attach().expect("local runtime");
    assert!(control.attach().is_err());
    let mut worker = control.connect("session").expect("evaluation");
    assert!(control.connect("other-session").is_err());
    let request = serde_json::to_vec(&json!({"action": "apps"})).expect("request");
    worker.write_u32(request.len() as u32).await.expect("size");
    worker.write_all(&request).await.expect("request");
    let Some(ServerMessage::DesktopControlRequested {
        request_id,
        execution_id,
        session_id,
        request,
    }) = app.outgoing.recv().await
    else {
        panic!("desktop request");
    };
    assert_eq!(session_id, "session");
    assert_eq!(request, json!({"action": "apps"}));
    assert!(app.reply("stale", json!({"result": []})).is_err());
    app.reply(&request_id, json!({"result": []}))
        .expect("reply");
    let size = worker.read_u32().await.expect("reply size") as usize;
    let mut reply = vec![0; size];
    worker.read_exact(&mut reply).await.expect("reply");
    assert_eq!(
        serde_json::from_slice::<Value>(&reply).expect("JSON"),
        json!({"result": []})
    );
    assert!(app.reply(&request_id, json!({"result": []})).is_err());
    drop(worker);
    assert!(
        matches!(app.outgoing.recv().await, Some(ServerMessage::DesktopControlEnded { execution_id: id }) if id == execution_id)
    );
    let worker = control.connect("next").expect("released lease");
    drop(worker);
}

#[tokio::test]
async fn disconnect_cancels_pending_desktop_request() {
    let control = Arc::new(DesktopControl::default());
    let mut app = control.attach().expect("runtime");
    let mut worker = control.connect("session").expect("evaluation");
    let request = br#"{"action":"apps"}"#;
    worker.write_u32(request.len() as u32).await.expect("size");
    worker.write_all(request).await.expect("request");
    assert!(matches!(
        app.outgoing.recv().await,
        Some(ServerMessage::DesktopControlRequested { .. })
    ));
    drop(app);
    assert!(worker.read_u32().await.is_err());
    assert!(control.connect("session").is_err());
}

#[tokio::test]
async fn workspace_policy_cannot_reach_the_native_runtime() {
    use mobius::backend::sandbox::{SandboxBackend, SandboxMode};
    let directory = tempfile::tempdir().expect("directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(directory.path().join("state")).expect("state");
    let sandbox = crate::sandbox::GatewaySandbox::new(
        &workspace,
        &directory.path().join("state"),
        None,
        std::time::Duration::from_secs(5),
    )
    .expect("sandbox")
    .with_desktop(Arc::new(DesktopControl::default()));
    assert!(
        sandbox
            .worker_connection("session", SandboxMode::WorkspaceWrite)
            .await
            .is_err()
    );
}
