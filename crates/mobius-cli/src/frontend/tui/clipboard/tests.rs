use super::*;

async fn handled(
    uploads: &mut ClipboardUploads,
    message: ServerMessage,
    session_id: &mobius::SessionId,
) -> Option<Result<UploadAdvance, String>> {
    if !uploads.claims(&message) {
        return None;
    }
    Some(uploads.handle(message, session_id).await)
}

const UPLOAD_ID: &str = "6752c95f-f2f6-4845-928d-93db92ee0e2a";

#[test]
fn local_attachments_must_be_regular_files() {
    let directory = tempfile::tempdir().expect("tempdir");
    assert!(UploadCandidate::from_file(directory.path()).is_err());
}

#[tokio::test]
async fn canceled_preparation_keeps_the_native_clipboard_gate_until_release() {
    let (started_sender, started_receiver) = std::sync::mpsc::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
    let first = spawn_preparation(move || {
        started_sender.send(()).expect("started signal");
        release_receiver.recv().expect("release signal");
        finished_sender.send(()).expect("finished signal");
        Ok(Vec::new())
    })
    .expect("first preparation");
    started_receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("first worker acquired the gate");
    drop(first);

    let Err(error) = spawn_preparation(|| Ok(Vec::new())) else {
        panic!("second preparation unexpectedly started");
    };
    assert_eq!(error, CLIPBOARD_PREPARATION_BUSY);

    release_sender.send(()).expect("release first worker");
    finished_receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("first worker finished");
    let permit = CLIPBOARD_PREPARATION_GATE
        .acquire()
        .await
        .expect("clipboard preparation gate");
    drop(permit);
    let third = spawn_preparation(|| Ok(Vec::new())).expect("third preparation");
    assert!(third.await.expect("third preparation result").is_ok());
}

#[tokio::test]
async fn upload_machine_correlates_chunks_and_starts_the_next_file() {
    let session_id = mobius::SessionId::new("session").expect("session");
    let first = UploadCandidate::from_bytes("one.txt", "text/plain", b"abc".to_vec())
        .expect("first candidate");
    let second = UploadCandidate::from_bytes("two.txt", "text/plain", b"d".to_vec())
        .expect("second candidate");
    let mut uploads = ClipboardUploads::default();
    let begin = uploads
        .start(vec![first, second], &session_id)
        .expect("begin upload");
    let ClientMessage::BeginSessionFileUpload { request_id, .. } = begin else {
        panic!("begin message");
    };

    assert!(
        handled(
            &mut uploads,
            ServerMessage::SessionFileUploadReady {
                request_id: "other".into(),
                session_id: session_id.share(),
                upload_id: UPLOAD_ID.into(),
                max_chunk_bytes: 2,
            },
            &session_id,
        )
        .await
        .is_none()
    );
    let ready = handled(
        &mut uploads,
        ServerMessage::SessionFileUploadReady {
            request_id,
            session_id: session_id.share(),
            upload_id: UPLOAD_ID.into(),
            max_chunk_bytes: 2,
        },
        &session_id,
    )
    .await
    .expect("matched ready")
    .expect("first chunk");
    let ClientMessage::UploadSessionFileChunk {
        request_id,
        offset,
        data,
        ..
    } = ready.message.expect("chunk message")
    else {
        panic!("chunk message");
    };
    assert_eq!((offset, data.as_slice()), (0, b"ab".as_slice()));

    let chunk = handled(
        &mut uploads,
        ServerMessage::SessionFileUploadChunkAccepted {
            request_id,
            session_id: session_id.share(),
            upload_id: UPLOAD_ID.into(),
            next_offset: 2,
        },
        &session_id,
    )
    .await
    .expect("matched chunk")
    .expect("second chunk");
    let ClientMessage::UploadSessionFileChunk {
        request_id,
        offset,
        data,
        ..
    } = chunk.message.expect("second chunk message")
    else {
        panic!("second chunk message");
    };
    assert_eq!((offset, data.as_slice()), (2, b"c".as_slice()));

    let finish = handled(
        &mut uploads,
        ServerMessage::SessionFileUploadChunkAccepted {
            request_id,
            session_id: session_id.share(),
            upload_id: UPLOAD_ID.into(),
            next_offset: 3,
        },
        &session_id,
    )
    .await
    .expect("matched final chunk")
    .expect("finish");
    let ClientMessage::FinishSessionFileUpload { request_id, .. } =
        finish.message.expect("finish message")
    else {
        panic!("finish message");
    };

    let completed = handled(
        &mut uploads,
        ServerMessage::SessionFileUploadCompleted {
            request_id,
            session_id: session_id.share(),
            file: SessionFileReference {
                id: "file-1".into(),
                name: "one.txt".into(),
                size: 3,
                media_type: "text/plain".into(),
            },
        },
        &session_id,
    )
    .await
    .expect("matched completion")
    .expect("completed upload");

    assert_eq!(completed.attachment.expect("attachment").name, "one.txt");
    assert!(matches!(
        completed.message,
        Some(ClientMessage::BeginSessionFileUpload { name, .. }) if name == "two.txt"
    ));
}
