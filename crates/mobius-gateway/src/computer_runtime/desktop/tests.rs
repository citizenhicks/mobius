use super::*;
use serde_json::json;

#[tokio::test]
async fn desktop_registration_requires_a_local_desktop_host() {
    use crate::wire::{ClientMessage, FrameReader, ServerFrame, read_frame};

    let host = crate::wire::ClientCapabilities {
        desktop_host: true,
        ..Default::default()
    };
    let renderer = crate::wire::ClientCapabilities {
        presence: true,
        browser_renderer: true,
        ..Default::default()
    };
    for (local, client) in [(false, host), (true, renderer)] {
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
            client,
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
            if request_id.as_ref() == "register" && code == "desktop_control")
        );
        assert!(connection.is_none());
        assert!(
            control
                .connect(&SessionId::new("session").unwrap())
                .is_err()
        );
    }
}

#[tokio::test]
async fn desktop_serializes_evaluations_and_rejects_stale_replies() {
    let control = Arc::new(DesktopControl::default());
    assert!(
        control
            .connect(&SessionId::new("session").unwrap())
            .is_err()
    );
    let mut app = control.attach().expect("local runtime");
    assert!(control.attach().is_err());
    let mut worker = control
        .connect(&SessionId::new("session").unwrap())
        .expect("evaluation");
    assert!(
        control
            .connect(&SessionId::new("other-session").unwrap())
            .is_err()
    );
    let request = serde_json::to_vec(&json!({"action": "apps"})).expect("request");
    worker
        .write_u32(u32::try_from(request.len()).expect("size fits"))
        .await
        .expect("size");
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
    assert_eq!(session_id.as_str(), "session");
    assert_eq!(request, json!({"action": "apps"}));
    assert!(app.reply("stale", json!({"result": []})).is_err());
    app.reply(&request_id, json!({"result": []}))
        .expect("reply");
    let size = usize::try_from(worker.read_u32().await.expect("reply size")).expect("size fits");
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
    let worker = control
        .connect(&SessionId::new("next").unwrap())
        .expect("released lease");
    drop(worker);
}
