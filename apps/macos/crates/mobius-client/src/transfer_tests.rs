use std::sync::Arc;

use mobius::SessionId;
use mobius::backend::model::provider::BaseUrl;
use mobius_gateway::client::{GatewayClient, GatewayEvents};
use mobius_gateway::server::GatewayServer;
use mobius_gateway::wire::{
    AgentComposition, ClientCapabilities, ClientMessage, ProviderEndpointAuth, ServerMessage,
};

use super::{Transfer, Transfers};
use crate::Update;

#[path = "preview_tests.rs"]
mod previews;

/// Serves `server` until the returned sender fires.
fn serve(
    server: GatewayServer,
) -> (
    mobius_gateway::client::Endpoint,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<mobius_gateway::Result<()>>,
) {
    let endpoint = format!("tcp://{}", server.listen_addr()).parse().unwrap();
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    (
        endpoint,
        shutdown,
        tokio::spawn(server.serve_until(until(signal))),
    )
}

async fn until(signal: tokio::sync::oneshot::Receiver<()>) {
    if let Err(error) = signal.await {
        tracing::debug!(%error, "the test gateway's shutdown sender was dropped");
    }
}

#[tokio::test]
async fn attachment_lifecycle_uploads_cancels_deletes_and_preserves_downloads() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let (server, grant) =
        GatewayServer::bootstrap(root.path().join("gateway"), "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
    let address = server.listen_addr();
    let endpoint = format!("tcp://{address}").parse().unwrap();
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(until(signal)));
    let (client, identity) = pair(&endpoint, grant.code, "Attachment test").await;
    let (sender, mut events) = client.into_parts();
    let (session_id, conversation) = create_session(&sender, &mut events, workspace.clone()).await;
    let transfers = Transfers::new(endpoint, identity.token);
    previews::assert_workspace_previews(&transfers, &session_id, &workspace).await;
    assert_active_upload_cancelled(&transfers, &session_id).await;
    let link = transfers.link().await.unwrap();
    let source = root.path().join("source.txt");
    // Several chunks, so uploads and downloads keep a window of requests in flight.
    let content: Vec<u8> = (0..1_300_000_u32)
        .map(|index| index.to_le_bytes()[0])
        .collect();
    std::fs::write(&source, &content).unwrap();
    let transfer = Transfer::Upload {
        id: "upload".into(),
        session_id: session_id.share(),
        path: source,
        cancellation: Default::default(),
    };
    let Update::Uploaded {
        result: Ok(Some(reference)),
        ..
    } = transfer.run(&transfers).await.unwrap().unwrap()
    else {
        panic!("completed upload expected");
    };
    assert_storage_export_and_artifact_delete(
        &transfers,
        &session_id,
        root.path(),
        &reference,
        &content,
    )
    .await;
    let destination = root.path().join("download.txt");
    let transfer = Transfer::Download {
        session_id: session_id.share(),
        file: reference.clone(),
        destination: destination.clone(),
    };
    transfer.run(&transfers).await.unwrap();
    assert_eq!(std::fs::read(&destination).unwrap(), content);
    assert!(transfer.run(&transfers).await.is_err());
    assert_eq!(std::fs::read(&destination).unwrap(), content);
    assert_completed_file_removed(
        &transfers,
        &session_id,
        reference,
        root.path().join("deleted.txt"),
    )
    .await;
    assert_eq!(std::fs::read(&destination).unwrap(), content);
    assert!(
        Arc::ptr_eq(&link, &transfers.link().await.unwrap()),
        "every transfer shares one connection"
    );
    drop(link);
    assert_unconfirmed_upload_removed(address, &transfers, &session_id, root.path()).await;
    let cancelled = Transfer::Upload {
        id: "cancelled".into(),
        session_id,
        path: root.path().join("never-opened"),
        cancellation: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    assert!(matches!(
        cancelled.run(&transfers).await.unwrap().unwrap(),
        Update::Uploaded {
            result: Ok(None),
            ..
        }
    ));
    sender
        .send(ClientMessage::OpenSession {
            request_id: "conversation".into(),
            session_id: conversation.share(),
            last_sequence: None,
        })
        .await
        .unwrap();
    let ServerMessage::SessionOpened { payload, .. } = wait_for(&mut events, |message| matches!(message, ServerMessage::SessionOpened { request_id, .. } if &**request_id == "conversation")).await else { unreachable!() };
    assert!(payload.workspace.is_none());
    assert_conversation_attachments(&transfers, &conversation, root.path()).await;
    shutdown.send(()).unwrap();
    serving.await.unwrap().unwrap();
}

async fn assert_storage_export_and_artifact_delete(
    transfers: &Arc<Transfers>,
    session_id: &SessionId,
    root: &std::path::Path,
    upload: &mobius::protocol::SessionFileReference,
    upload_content: &[u8],
) {
    let files = mobius::backend::session_files::SessionFileStore::new(&root.join("gateway"), None);
    let artifact = files
        .publish_artifact(
            session_id,
            upload.name.clone(),
            "text/plain".into(),
            b"artifact",
        )
        .await
        .unwrap();
    for (name, uploads, artifacts, expected) in [
        ("uploads", true, false, vec![upload_content.to_vec()]),
        ("artifacts", false, true, vec![b"artifact".to_vec()]),
        (
            "both",
            true,
            true,
            vec![upload_content.to_vec(), b"artifact".to_vec()],
        ),
    ] {
        let destination = root.join(name);
        let export = Transfer::Export {
            id: crate::request_id(),
            session_id: session_id.share(),
            destination: destination.clone(),
            uploads,
            artifacts,
        };
        assert!(matches!(
            export.run(transfers).await.unwrap().unwrap(),
            Update::StorageExported { result: Ok(()), .. }
        ));
        let mut actual = std::fs::read_dir(&destination)
            .unwrap()
            .map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
            .collect::<Vec<_>>();
        let mut expected = expected;
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
        export.run(transfers).await.unwrap();
        assert_eq!(
            std::fs::read_dir(&destination).unwrap().count(),
            expected.len() * 2,
            "re-export must preserve existing files even when names collide"
        );
    }
    assert_completed_file_removed(
        transfers,
        session_id,
        artifact,
        root.join("deleted-artifact.txt"),
    )
    .await;
    assert_eq!(
        files.read_file(session_id, upload).await.unwrap(),
        upload_content
    );
}

async fn assert_conversation_attachments(
    transfers: &Arc<Transfers>,
    session: &SessionId,
    root: &std::path::Path,
) {
    let source = root.join("conversation.txt");
    std::fs::write(&source, b"persistent conversation attachment").unwrap();
    let Update::Uploaded {
        result: Ok(Some(file)),
        ..
    } = (Transfer::Upload {
        id: "conversation-upload".into(),
        session_id: session.share(),
        path: source,
        cancellation: Default::default(),
    })
    .run(transfers)
    .await
    .unwrap()
    .unwrap()
    else {
        panic!("conversation upload expected")
    };
    let preview = Transfer::Preview {
        id: "conversation-files".into(),
        session_id: session.share(),
        request: crate::PreviewRequest::ChatFiles,
    }
    .run(transfers)
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(preview, Update::PreviewLoaded { result: Ok(crate::PreviewData::ChatFiles(files)), .. } if files.iter().any(|record| record.file.id == file.id))
    );
    let destination = root.join("conversation-download.txt");
    Transfer::Download {
        session_id: session.share(),
        file: file.clone(),
        destination: destination.clone(),
    }
    .run(transfers)
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(destination).unwrap(),
        b"persistent conversation attachment"
    );
    let preview = Transfer::Preview {
        id: "no-project-files".into(),
        session_id: session.share(),
        request: crate::PreviewRequest::Files,
    }
    .run(transfers)
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        preview,
        Update::PreviewLoaded { result: Err(_), .. }
    ));
    assert_completed_file_removed(
        transfers,
        session,
        file,
        root.join("conversation-deleted.txt"),
    )
    .await;
}

#[tokio::test]
async fn transfer_answers_arrive_behind_any_number_of_unrelated_frames() {
    use mobius_gateway::wire::{ClientFrame, FrameReader, ServerFrame, read_frame, write_frame};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("tcp://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let gateway = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = socket.into_split();
        let mut reader = FrameReader::new(reader);
        read_frame::<serde_json::Value>(&mut reader).await.unwrap();
        write_frame(&mut writer, &ServerFrame::new(ServerMessage::Authenticated))
            .await
            .unwrap();
        loop {
            let frame = read_frame::<ClientFrame>(&mut reader)
                .await
                .unwrap()
                .unwrap();
            let ClientMessage::ListSessionFiles {
                request_id,
                session_id,
            } = frame.message
            else {
                continue;
            };
            // Far past the 1024 frames the per-transfer connections gave up after.
            for index in 0..3_000 {
                let unrelated = ServerMessage::Accepted {
                    request_id: format!("unrelated-{index}").into(),
                };
                write_frame(&mut writer, &ServerFrame::new(unrelated))
                    .await
                    .unwrap();
            }
            let files = ServerMessage::SessionFiles {
                request_id,
                session_id,
                files: Vec::new(),
            };
            write_frame(&mut writer, &ServerFrame::new(files))
                .await
                .unwrap();
            return reader;
        }
    });
    let transfers = Transfers::new(endpoint, "test-only".into());
    let link = transfers.link().await.unwrap();
    let response = link
        .request(|request_id| ClientMessage::ListSessionFiles {
            request_id,
            session_id: SessionId::new("chat").unwrap(),
        })
        .await
        .unwrap();
    assert!(matches!(response, ServerMessage::SessionFiles { .. }));
    drop(gateway.await.unwrap());
}

async fn assert_unconfirmed_upload_removed(
    address: std::net::SocketAddr,
    transfers: &Arc<Transfers>,
    session_id: &SessionId,
    root: &std::path::Path,
) {
    use mobius_gateway::wire::{FrameReader, ServerFrame, read_frame, write_frame};
    use tokio::net::{TcpListener, TcpStream};

    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let interrupted = format!("tcp://{}", proxy.local_addr().unwrap())
        .parse()
        .unwrap();
    let relay = tokio::spawn(async move {
        let (mut client, _) = proxy.accept().await.unwrap();
        let mut server = TcpStream::connect(address).await.unwrap();
        let (mut client_read, mut client_write) = client.split();
        let (server_read, mut server_write) = server.split();
        let mut server_read = FrameReader::new(server_read);
        tokio::select! {
            _ = tokio::io::copy(&mut client_read, &mut server_write) => {
                panic!("client disconnected before upload committed");
            }
            file = async {
                loop {
                    let frame = read_frame::<ServerFrame>(&mut server_read).await.unwrap().unwrap();
                    if let ServerMessage::SessionFileUploadCompleted { file, .. } = frame.message {
                        // The server committed successfully, but the client never receives its ACK.
                        return file;
                    }
                    write_frame(&mut client_write, &frame).await.unwrap();
                }
            } => file,
        }
    });
    let upload = Transfer::Upload {
        id: "lost-ack".into(),
        session_id: session_id.share(),
        path: root.join("source.txt"),
        cancellation: Default::default(),
    };
    let interrupted = Transfers::new(interrupted, transfers.token.as_str().into());
    let Update::Uploaded {
        result: Err(failure),
        ..
    } = upload.run(&interrupted).await.unwrap().unwrap()
    else {
        panic!("interrupted finalization must not publish a ready attachment");
    };
    let committed = relay.await.unwrap();
    assert_eq!(
        failure.cleanup_file_id.as_deref(),
        Some(committed.id.as_str())
    );
    assert!(!failure.message.is_empty());
    let downloaded = root.join("unconfirmed.txt");
    Transfer::Download {
        session_id: session_id.share(),
        file: committed.clone(),
        destination: downloaded.clone(),
    }
    .run(transfers)
    .await
    .expect("lost acknowledgement must leave the committed file readable");
    assert_completed_file_removed(transfers, session_id, committed, root.join("removed.txt")).await;
    assert!(downloaded.exists());
}

async fn assert_completed_file_removed(
    transfers: &Arc<Transfers>,
    session_id: &SessionId,
    file: mobius::protocol::SessionFileReference,
    destination: std::path::PathBuf,
) {
    let deleted = Transfer::Delete {
        id: "remove".into(),
        session_id: session_id.share(),
        file_id: file.id.clone(),
    };
    assert!(matches!(
        deleted.run(transfers).await.unwrap().unwrap(),
        Update::AttachmentDeleted { result: Ok(()), .. }
    ));
    let download = Transfer::Download {
        session_id: session_id.share(),
        file,
        destination: destination.clone(),
    };
    assert!(download.run(transfers).await.is_err());
    assert!(
        !destination.exists(),
        "deleted reference must not be downloadable"
    );
}

async fn assert_active_upload_cancelled(transfers: &Arc<Transfers>, session_id: &SessionId) {
    let link = transfers.link().await.unwrap();
    let _selected = link.select(session_id).await.unwrap();
    let response = link
        .request(|request_id| ClientMessage::BeginSessionFileUpload {
            request_id,
            session_id: session_id.share(),
            name: "cancelled.txt".into(),
            media_type: "text/plain".into(),
            size: 1,
        })
        .await
        .unwrap();
    let ServerMessage::SessionFileUploadReady { upload_id, .. } = response else {
        panic!("ready expected")
    };
    link.request(|request_id| ClientMessage::UploadSessionFileChunk {
        request_id,
        session_id: session_id.share(),
        upload_id: upload_id.as_str().into(),
        offset: 0,
        data: vec![b'x'],
    })
    .await
    .unwrap();
    let cancellation = std::sync::atomic::AtomicBool::new(true);
    assert!(
        crate::upload::discard_if_requested(&link, session_id, &upload_id, &cancellation)
            .await
            .unwrap()
    );
    let finish = link
        .request(|request_id| ClientMessage::FinishSessionFileUpload {
            request_id,
            session_id: session_id.share(),
            upload_id,
        })
        .await;
    assert!(finish.is_err(), "cancelled upload must not be finishable");
}

async fn create_session(
    sender: &mobius_gateway::client::GatewaySender,
    events: &mut GatewayEvents,
    workspace: std::path::PathBuf,
) -> (SessionId, SessionId) {
    let mut provider = AgentComposition::default().provider;
    provider.provider = "openrouter".into();
    provider.instance = "test".into();
    provider.model = "test-model".into();
    provider.base_url = Some(BaseUrl::new("https://example.invalid/v1").unwrap());
    provider.endpoint_auth = ProviderEndpointAuth::Credentialless;
    provider.reasoning_effort = None;
    sender
        .send(ClientMessage::RegisterProvider {
            request_id: "provider".into(),
            config: provider,
            label: Some("Test".into()),
            tint: None,
            models: vec![mobius_gateway::wire::ConfiguredModel {
                id: "test-model".into(),
                ..Default::default()
            }],
            image_model_ids: None,
            operator_managed: false,
            preserve_selection: false,
            if_configured: false,
        })
        .await
        .unwrap();
    wait_for(events, |message| matches!(message, ServerMessage::GatewayConfigured { request_id, .. } if &**request_id == "provider")).await;
    sender
        .send(ClientMessage::CreateBot {
            request_id: "bot".into(),
            harness: mobius_gateway::wire::HarnessKind::Mobius,
            name: "File transfer".into(),
            description: "Isolated test".into(),
        })
        .await
        .unwrap();
    let ServerMessage::Bots { bots, .. } = wait_for(events, |message| {
        matches!(message, ServerMessage::Bots { request_id: Some(id), .. } if &**id == "bot")
    }).await else { unreachable!() };
    let bot = bots
        .into_iter()
        .find(|bot| &*bot.name == "File transfer")
        .unwrap();
    let conversation = bot.conversation_session_id.share();
    sender
        .send(ClientMessage::CreateSession {
            request_id: "session".into(),
            workspace,
            bot_id: bot.id,
        })
        .await
        .unwrap();
    let ServerMessage::SessionOpened { payload, .. } = wait_for(events, |message| {
        matches!(message, ServerMessage::SessionOpened { request_id, .. } if &**request_id == "session")
    }).await else { unreachable!() };
    (payload.session.session_id.share(), conversation)
}

async fn wait_for(
    events: &mut GatewayEvents,
    matches: impl Fn(&ServerMessage) -> bool + Send + Sync,
) -> ServerMessage {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let message = events.next().await.unwrap().unwrap().message;
            if let ServerMessage::Rejected { message, .. } | ServerMessage::Error { message, .. } =
                &message
            {
                panic!("gateway rejected test setup: {message}");
            }
            if matches(&message) {
                return message;
            }
        }
    })
    .await
    .expect("gateway response timeout")
}

#[tokio::test]
async fn cloud_upload_rejection_preserves_the_free_space_action() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("gateway");
    let (server, grant) = GatewayServer::bootstrap(state.clone(), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    drop(server);
    let (config_store, mut config) =
        mobius_gateway::config::ConfigStore::open(state.clone()).unwrap();
    let collector = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.telemetry.sinks = vec![
        serde_json::from_value(serde_json::json!({
            "id":"cloud-test", "url":format!("http://{}", collector.local_addr().unwrap()),
            "every_seconds":60, "upload_admission":true
        }))
        .unwrap(),
    ];
    config_store.save(&config.validated().unwrap()).unwrap();
    let decision = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        loop {
            let (mut stream, _) = collector.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            let body: serde_json::Value = loop {
                let count = stream.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let size: usize = String::from_utf8_lossy(&bytes[..end])
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
                        break serde_json::from_slice(&bytes[end + 4..end + 4 + size]).unwrap();
                    }
                }
            };
            let upload = body["reason"] == "upload";
            let (status, reply) = if upload {
                (
                    200,
                    serde_json::json!({"request_id":body["upload"]["request_id"],
                    "allowed":false,"code":"storage_full","message":"Cloud storage is full."})
                    .to_string(),
                )
            } else {
                (202, String::new())
            };
            stream.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).as_bytes()).await.unwrap();
            if upload {
                break;
            }
        }
    });
    let (endpoint, shutdown, serving) = serve(GatewayServer::open(state).await.unwrap());
    let (client, identity) = pair(&endpoint, grant.code, "Quota test").await;
    let (sender, mut events) = client.into_parts();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let (session_id, _) = create_session(&sender, &mut events, workspace).await;
    let transfers = Transfers::new(endpoint, identity.token);
    let link = transfers.link().await.unwrap();
    let selected = link.select(&session_id).await.unwrap();
    let error = link
        .request(|request_id| ClientMessage::BeginSessionFileUpload {
            request_id,
            session_id: session_id.share(),
            name: "over-limit.bin".into(),
            size: 1,
            media_type: "application/octet-stream".into(),
        })
        .await
        .unwrap_err();
    drop(selected);
    let failure = crate::UploadFailure::from(error);
    assert!(failure.storage_full);
    assert!(failure.cleanup_file_id.is_none());
    decision.await.unwrap();
    shutdown.send(()).unwrap();
    serving.await.unwrap().unwrap();
}

/// Pairs a client that does nothing for the gateway but transfer.
async fn pair(
    endpoint: &mobius_gateway::client::Endpoint,
    code: String,
    label: &str,
) -> (GatewayClient, mobius_gateway::client::PairedClient) {
    GatewayClient::pair(endpoint, code, label, ClientCapabilities::default())
        .await
        .unwrap()
}
