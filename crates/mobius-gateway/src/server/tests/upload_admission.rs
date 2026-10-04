use super::*;
use crate::telemetry::{Telemetry, TelemetrySink};

async fn request_body(stream: &mut tokio::net::TcpStream) -> serde_json::Value {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = stream.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() <= 64 * 1024 + 4096);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let size: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .unwrap()
                .parse()
                .unwrap();
            if bytes.len() >= end + 4 + size {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + size]).unwrap();
            }
        }
    }
}

#[tokio::test]
async fn upload_admission_requires_a_valid_correlated_collector_decision() {
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sink: TelemetrySink = serde_json::from_value(serde_json::json!({
        "id":"admission", "url":format!("http://{}", listener.local_addr().unwrap()),
        "every_seconds":15, "upload_admission":true, "fields":{"gateway_id":"test"}
    }))
    .unwrap();
    server
        .host
        .configure_telemetry(0, vec![sink], &[])
        .await
        .unwrap();
    let receiver = tokio::spawn(async move {
        for mode in [
            "allow",
            "full",
            "mismatched",
            "empty",
            "oversized",
            "unavailable",
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let body = request_body(&mut stream).await;
            assert_eq!(body["reason"], "upload");
            assert_eq!(body["upload"]["bytes"], 1);
            assert_eq!(body["protocol_version"], crate::wire::PROTOCOL_VERSION);
            assert!(body["storage"]["used_bytes"].is_u64());
            assert!(body.get("events").is_none());
            let request_id = &body["upload"]["request_id"];
            let (status, reply) = match mode {
                "allow" => (200, serde_json::json!({"request_id":request_id,"allowed":true}).to_string()),
                "full" => (200, serde_json::json!({"request_id":request_id,"allowed":false,"code":"storage_full","message":"Cloud storage is full."}).to_string()),
                "mismatched" => (200, serde_json::json!({"request_id":"wrong","allowed":true}).to_string()),
                "empty" => (202, String::new()),
                "oversized" => (200, " ".repeat(4097)),
                "unavailable" => (503, String::new()),
                _ => unreachable!(),
            };
            stream.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).as_bytes()).await.unwrap();
        }
    });
    for expected in [
        None,
        Some("storage_full"),
        Some("upload_admission_unavailable"),
        Some("upload_admission_unavailable"),
        Some("upload_admission_unavailable"),
        Some("upload_admission_unavailable"),
    ] {
        let result = Telemetry::admit_upload(&server.host, &Uuid::new_v4().to_string(), 1).await;
        assert_eq!(result.err().map(|rejection| rejection.code), expected);
    }
    receiver.await.unwrap();
    let files = server.host.session_file_store().await;
    files
        .publish_artifact("chat", "result.txt".into(), "text/plain".into(), b"result")
        .await
        .unwrap();
    // The collector is closed: generated observations still use only the local store.
    files
        .ingest_image(
            "chat",
            "screenshot.png".into(),
            include_bytes!("../../computer_runtime/remote_desktop/logo.png").to_vec(),
            mobius::protocol::ImageDetail::Auto,
        )
        .await
        .unwrap();
    server.host.shutdown().await;
}

#[tokio::test]
async fn a_reported_allowance_does_not_enforce_self_hosted_storage() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let (server, _) = configured_test_server(state.clone()).await;
    server.host.shutdown().await;
    drop(server);
    let (store, mut config) = ConfigStore::open(state.clone()).unwrap();
    config.runtime.storage_limit_bytes = Some(64 * 1024 * 1024);
    store.save(&config).unwrap();
    let server = GatewayServer::open(state).await.unwrap();
    let files = server.host.session_file_store().await;
    let pending = files
        .begin_upload(
            "chat",
            "large.bin".into(),
            64 * 1024 * 1024 + 1,
            "application/octet-stream".into(),
        )
        .await
        .unwrap();
    Telemetry::admit_upload(&server.host, pending.id(), 64 * 1024 * 1024 + 1)
        .await
        .unwrap();
    drop(pending);
    server.host.shutdown().await;
}
