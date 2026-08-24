use super::*;

#[tokio::test]
async fn upload_round_trip_is_session_scoped_and_atomic() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    let session_id = "thread:not-a-uuid";
    let mut pending = store
        .begin_upload(session_id, "notes.txt".into(), 5, "text/plain".into())
        .await
        .expect("begin");
    pending.append(0, b"hello").await.expect("append");
    let file = pending.finish().await.expect("finish");

    store
        .verify_upload(session_id, &file)
        .await
        .expect("verify upload");

    let bytes = store
        .read_chunk(session_id, &file.id, 0, MAX_READ_CHUNK_BYTES)
        .await
        .expect("read");

    assert_eq!(bytes.data, b"hello");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let session = store.session_dir(session_id);
        let directory = session.join(&file.id);
        for path in [store.root.as_path(), &session, &directory] {
            let mode = std::fs::metadata(path)
                .expect("directory mode")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let metadata = load_metadata(&directory.join(METADATA_FILE))
            .await
            .expect("metadata");
        for path in [
            store.blob_path(&metadata.content_hash),
            directory.join(METADATA_FILE),
        ] {
            let mode = std::fs::metadata(path)
                .expect("file mode")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}

#[tokio::test]
async fn file_list_identifies_user_uploads_and_agent_artifacts() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    let mut upload = store
        .begin_upload("session", "input.txt".into(), 1, "text/plain".into())
        .await
        .expect("begin upload");
    upload.append(0, b"u").await.expect("append upload");
    upload.finish().await.expect("finish upload");
    store
        .publish_artifact("session", "output.txt".into(), "text/plain".into(), b"a")
        .await
        .expect("publish artifact");

    let files = store.list_files("session").await.expect("list files");

    assert!(
        files.iter().any(|record| {
            record.origin == ProtocolFileOrigin::User && record.file.name == "input.txt"
        }) && files.iter().any(|record| {
            record.origin == ProtocolFileOrigin::Agent && record.file.name == "output.txt"
        })
    );
}

#[tokio::test]
async fn delete_session_removes_only_that_sessions_files() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    for session_id in ["deleted", "retained"] {
        store
            .publish_artifact(
                session_id,
                "result.txt".into(),
                "text/plain".into(),
                b"result",
            )
            .await
            .expect("publish artifact");
    }

    store
        .delete_session("deleted")
        .await
        .expect("delete session files");

    assert!(
        store
            .list_artifacts("deleted")
            .await
            .expect("deleted artifacts")
            .is_empty()
    );
    assert_eq!(
        store
            .list_artifacts("retained")
            .await
            .expect("retained artifacts")
            .len(),
        1
    );
}

#[tokio::test]
async fn identical_payloads_share_one_content_blob() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    let first = store
        .publish_artifact("first", "one.txt".into(), "text/plain".into(), b"same")
        .await
        .expect("first artifact");
    let second = store
        .publish_artifact("second", "two.txt".into(), "text/plain".into(), b"same")
        .await
        .expect("second artifact");

    assert_ne!(first.id, second.id);
    assert_eq!(blob_entries(&store), 1);
}

#[tokio::test]
async fn tampered_content_blob_is_rejected() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    let file = store
        .publish_artifact("session", "result.txt".into(), "text/plain".into(), b"safe")
        .await
        .expect("artifact");
    let metadata = load_metadata(
        &store
            .session_dir("session")
            .join(&file.id)
            .join(METADATA_FILE),
    )
    .await
    .expect("metadata");
    std::fs::write(store.blob_path(&metadata.content_hash), b"evil").expect("tamper");

    assert!(store.read_chunk("session", &file.id, 0, 4).await.is_err());
}

#[test]
fn validated_blob_cache_is_bounded() {
    let cache = StdMutex::new(BTreeMap::new());
    let stamp = BlobValidationStamp {
        size: 1,
        modified: SystemTime::now(),
    };

    for index in 0..=MAX_VALIDATED_BLOBS {
        remember_validated_blob(&cache, &format!("{index:064x}"), stamp);
    }

    let cache = cache.into_inner().expect("validation cache");
    assert_eq!(cache.len(), MAX_VALIDATED_BLOBS);
    assert!(cache.contains_key(&format!("{:064x}", MAX_VALIDATED_BLOBS)));
}

#[tokio::test]
async fn private_content_identity_round_trips_without_wire_changes() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    let file = store
        .publish_artifact(
            "session",
            "upload.txt".into(),
            "text/plain".into(),
            b"hello",
        )
        .await
        .expect("artifact");
    assert!(store.upload_content_hash("session", &file).await.is_err());

    let mut pending = store
        .begin_upload("session", "upload.txt".into(), 5, "text/plain".into())
        .await
        .expect("upload");
    pending.append(0, b"hello").await.expect("append");
    let upload = pending.finish().await.expect("finish");
    let hash = store
        .upload_content_hash("session", &upload)
        .await
        .expect("private hash");
    assert_eq!(
        store
            .read_content_blob(&hash, upload.size)
            .await
            .expect("private blob"),
        b"hello"
    );
}

#[tokio::test]
async fn deleting_last_reference_garbage_collects_the_blob() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    store
        .publish_artifact("first", "one.txt".into(), "text/plain".into(), b"same")
        .await
        .expect("first artifact");
    store
        .publish_artifact("second", "two.txt".into(), "text/plain".into(), b"same")
        .await
        .expect("second artifact");

    store.delete_session("first").await.expect("delete first");
    assert_eq!(blob_entries(&store), 1);
    store.delete_session("second").await.expect("delete second");
    assert_eq!(blob_entries(&store), 0);
}

#[tokio::test]
async fn accepts_50_mib_and_rejects_larger_files() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    let limits = session_file_limits();
    let pending = store
        .begin_upload(
            "session",
            "large.bin".into(),
            limits.max_file_bytes,
            "application/octet-stream".into(),
        )
        .await
        .expect("50 MiB upload");
    drop(pending);
    assert!(
        store
            .begin_upload(
                "session",
                "too-large.bin".into(),
                limits.max_file_bytes + 1,
                "application/octet-stream".into(),
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn delete_session_rejects_an_active_upload() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    let pending = store
        .begin_upload("session", "pending.txt".into(), 1, "text/plain".into())
        .await
        .expect("begin upload");

    assert!(store.delete_session("session").await.is_err());

    drop(pending);
    store
        .delete_session("session")
        .await
        .expect("delete released session files");
}

#[tokio::test]
async fn display_names_never_select_internal_storage_paths() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());

    for name in [METADATA_FILE, ".SESSION-FILE.JSON"] {
        let mut pending = store
            .begin_upload("session", name.into(), 1, "application/octet-stream".into())
            .await
            .expect("begin");
        pending.append(0, b"x").await.expect("append");
        let file = pending.finish().await.expect("finish");

        assert_eq!(file.name, name);
        assert_eq!(
            store
                .read_chunk("session", &file.id, 0, MAX_READ_CHUNK_BYTES)
                .await
                .expect("read")
                .data,
            b"x"
        );
    }
}

#[tokio::test]
async fn artifacts_are_downloadable_but_excluded_from_upload_access() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    let file = store
        .publish_artifact(
            "session",
            "report.xlsx".into(),
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".into(),
            &[0, 255, 1],
        )
        .await
        .expect("publish");

    assert!(
        store
            .list_uploads("session")
            .await
            .expect("uploads")
            .is_empty()
    );
    assert_eq!(
        store
            .read_chunk("session", &file.id, 0, 16)
            .await
            .expect("chunk")
            .data,
        [0, 255, 1]
    );
    assert!(
        store
            .read_chunk("another-session", &file.id, 0, 16)
            .await
            .is_err()
    );
    assert!(store.verify_upload("session", &file).await.is_err());
    let reopened = SessionFileStore::new(state.path());
    assert_eq!(
        reopened
            .list_artifacts("session")
            .await
            .expect("reopened artifacts"),
        [file]
    );
}

#[tokio::test]
async fn upload_rejects_traversal_names() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());

    assert!(
        store
            .begin_upload("session", "../secret".into(), 1, "text/plain".into())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn pending_uploads_reserve_session_quota_and_release_on_drop() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    let limits = session_file_limits();
    let mut pending = Vec::new();
    for index in 0..(limits.max_session_bytes / limits.max_file_bytes) {
        pending.push(
            store
                .begin_upload(
                    "session",
                    format!("{index}.bin"),
                    limits.max_file_bytes,
                    "application/octet-stream".into(),
                )
                .await
                .expect("reserve upload"),
        );
    }

    assert!(
        store
            .begin_upload(
                "session",
                "overflow.bin".into(),
                1,
                "application/octet-stream".into(),
            )
            .await
            .is_err()
    );

    drop(pending.pop());
    assert!(
        store
            .begin_upload(
                "session",
                "replacement.bin".into(),
                limits.max_file_bytes,
                "application/octet-stream".into(),
            )
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn advertised_file_count_is_enforced() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    let limits = session_file_limits();
    let mut pending = Vec::new();
    for index in 0..limits.max_session_files {
        pending.push(
            store
                .begin_upload(
                    "session",
                    format!("{index}.bin"),
                    1,
                    "application/octet-stream".into(),
                )
                .await
                .expect("reserve file slot"),
        );
    }

    assert!(
        store
            .begin_upload(
                "session",
                "overflow.bin".into(),
                1,
                "application/octet-stream".into(),
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn advertised_upload_chunk_size_is_enforced() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    let limits = session_file_limits();
    let accepted = vec![0; limits.max_upload_chunk_bytes];
    let rejected = vec![0; limits.max_upload_chunk_bytes + 1];
    let mut upload = store
        .begin_upload(
            "session",
            "large.bin".into(),
            u64::try_from(rejected.len()).expect("upload size"),
            "application/octet-stream".into(),
        )
        .await
        .expect("begin upload");

    upload.append(0, &accepted).await.expect("maximum chunk");
    let offset = u64::try_from(accepted.len()).expect("chunk offset");
    assert!(upload.append(offset, &rejected).await.is_err());
}

#[tokio::test]
async fn first_store_access_removes_crash_leftovers() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path());
    let session_id = Uuid::new_v4().to_string();
    let session = store.session_dir(&session_id);
    std::fs::create_dir_all(&session).expect("session directory");
    let temporary = session.join(".tmp-upload");
    std::fs::write(&temporary, b"partial").expect("temporary upload");
    let staging = session.join(format!(".{}-partial", Uuid::new_v4()));
    std::fs::create_dir(&staging).expect("staging directory");
    std::fs::write(staging.join("payload"), b"partial").expect("staged file");

    assert!(
        store
            .list_uploads(&session_id)
            .await
            .expect("list")
            .is_empty()
    );
    assert!(!temporary.exists());
    assert!(!staging.exists());
}

fn blob_entries(store: &SessionFileStore) -> usize {
    std::fs::read_dir(store.blob_dir())
        .expect("blob directory")
        .count()
}
