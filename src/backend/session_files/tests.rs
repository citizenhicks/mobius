use super::*;
use crate::SessionId;
use cap_std::{ambient_authority, fs::Dir};

fn session(value: &str) -> SessionId {
    SessionId::new(value).unwrap()
}

#[tokio::test]
async fn upload_round_trip_is_session_scoped_and_atomic() {
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path(), None);
    let id = &session("thread:not-a-uuid");
    let mut pending = store
        .begin_upload(id, "notes.txt".into(), 5, "text/plain".into())
        .await
        .expect("begin");
    pending.append(0, b"hello").await.expect("append");
    let file = pending.finish().await.expect("finish");

    store.verify_upload(id, &file).await.expect("verify upload");

    let bytes = store
        .read_chunk(id, &file.id, 0, MAX_READ_CHUNK_BYTES)
        .await
        .expect("read");

    assert_eq!(bytes.data, b"hello");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let session = store.session_dir(id);
        let directory = session.join(&file.id);
        for path in [&*store.state.root, &session, &directory] {
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
async fn delete_session_removes_only_that_sessions_files() {
    let deleted = session("deleted");
    let retained = session("retained");
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path(), None);
    for id in [&deleted, &retained] {
        store
            .publish_artifact(id, "result.txt".into(), "text/plain".into(), b"result")
            .await
            .expect("publish artifact");
    }

    store
        .delete_session(&deleted)
        .await
        .expect("delete session files");

    assert!(
        store
            .list_files(
                &deleted,
                &[crate::backend::session_files::SessionFileOrigin::Artifact]
            )
            .await
            .expect("deleted artifacts")
            .is_empty()
    );
    assert_eq!(
        store
            .list_files(
                &retained,
                &[crate::backend::session_files::SessionFileOrigin::Artifact]
            )
            .await
            .expect("retained artifacts")
            .len(),
        1
    );
    assert_eq!(blob_entries(&store), 1);
    store
        .delete_session(&retained)
        .await
        .expect("delete retained");
    assert_eq!(blob_entries(&store), 0);
}

#[tokio::test]
async fn tampered_content_blob_is_rejected() {
    let id = session("session");
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path(), None);
    let file = store
        .publish_artifact(&id, "result.txt".into(), "text/plain".into(), b"safe")
        .await
        .expect("artifact");
    let metadata = load_metadata(&store.session_dir(&id).join(&file.id).join(METADATA_FILE))
        .await
        .expect("metadata");
    std::fs::write(store.blob_path(&metadata.content_hash), b"evil").expect("tamper");

    assert!(store.read_chunk(&id, &file.id, 0, 4).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn attachment_cleanup_remains_safe_after_symlink_workspace_replacement_and_restart() {
    use std::os::unix::fs::symlink;

    let state = tempfile::tempdir().expect("state");
    let root = tempfile::tempdir().expect("workspace root");
    let workspace = root.path().join("workspace");
    let outside = root.path().join("outside");
    let id = &session("session");
    let outside_staged = outside
        .join(".mobius")
        .join("attachments")
        .join(session_storage_key(id));
    std::fs::create_dir_all(
        workspace
            .join(".mobius")
            .join("attachments")
            .join(session_storage_key(id)),
    )
    .expect("staged attachments");
    std::fs::create_dir_all(&outside_staged).expect("outside attachments");
    let pinned_workspace = Dir::open_ambient_dir(&workspace, ambient_authority()).expect("pin");

    SessionFileStore::new(state.path(), None)
        .register_attachment_workspace(id, &pinned_workspace, &workspace)
        .await
        .expect("register workspace");
    std::fs::rename(&workspace, root.path().join("original")).expect("move original");
    symlink(&outside, &workspace).expect("install symlink");

    SessionFileStore::new(state.path(), None)
        .delete_session(id)
        .await
        .expect("delete session");

    assert!(outside_staged.is_dir());
}

#[tokio::test]
async fn staged_deletion_releases_uploads_and_resumes_after_restart() {
    let deleted = session("deleted");
    let retained = session("retained");
    let state = tempfile::tempdir().expect("state");
    let workspace = tempfile::tempdir().expect("workspace");
    let staged = workspace
        .path()
        .join(".mobius/attachments")
        .join(session_storage_key(&deleted));
    std::fs::create_dir_all(&staged).expect("staged attachment directory");
    let directory =
        Dir::open_ambient_dir(workspace.path(), ambient_authority()).expect("workspace");
    let store = SessionFileStore::new(state.path(), None);
    store
        .register_attachment_workspace(&deleted, &directory, workspace.path())
        .await
        .expect("register");
    let mut deletion = store
        .prepare_delete_sessions(&[deleted.share()], SessionFileSelection::All)
        .await
        .expect("prepare");
    deletion.stage().await.expect("stage deletion");

    assert!(
        staged.is_dir(),
        "staging does not touch the external workspace"
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        store.publish_artifact(
            &retained,
            "result.txt".into(),
            "text/plain".into(),
            b"result",
        ),
    )
    .await
    .expect("other uploads remain available")
    .expect("publish");
    drop(deletion);
    let reopened = SessionFileStore::new(state.path(), None);
    reopened
        .cleanup_deleted_sessions()
        .await
        .expect("resume cleanup");
    assert!(!staged.exists());
    assert_eq!(
        reopened
            .list_files(
                &retained,
                &[crate::backend::session_files::SessionFileOrigin::Artifact]
            )
            .await
            .expect("retained files")
            .len(),
        1
    );
}

#[tokio::test]
async fn display_names_never_select_internal_storage_paths() {
    let id = session("session");
    let state = tempfile::tempdir().expect("state");
    let store = SessionFileStore::new(state.path(), None);

    for name in [METADATA_FILE, ".SESSION-FILE.JSON"] {
        let mut pending = store
            .begin_upload(&id, name.into(), 1, "application/octet-stream".into())
            .await
            .expect("begin");
        pending.append(0, b"x").await.expect("append");
        let file = pending.finish().await.expect("finish");

        assert_eq!(file.name, name);
        assert_eq!(
            store
                .read_chunk(&id, &file.id, 0, MAX_READ_CHUNK_BYTES)
                .await
                .expect("read")
                .data,
            b"x"
        );
    }
    assert!(
        store
            .begin_upload(&id, "../secret".into(), 1, "text/plain".into())
            .await
            .is_err()
    );
}

fn blob_entries(store: &SessionFileStore) -> usize {
    std::fs::read_dir(store.blob_dir())
        .expect("blob directory")
        .count()
}

#[tokio::test]
async fn origin_purge_preserves_uploads_and_shared_fork_blobs_and_removes_staged_copies() {
    let parent = session("parent");
    let fork = session("fork");
    let source = session("source");
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = SessionFileStore::new(state.path(), None);
    let mut pending = store
        .begin_upload(&parent, "input.txt".into(), 5, "text/plain".into())
        .await
        .unwrap();
    pending.append(0, b"input").await.unwrap();
    let upload = pending.finish().await.unwrap();
    store
        .publish_artifact(&parent, "result.txt".into(), "text/plain".into(), b"result")
        .await
        .unwrap();
    store.grant_file(&parent, &fork, &upload).await.unwrap();
    let observation = store
        .publish_artifact(
            &source,
            "observed.txt".into(),
            "text/plain".into(),
            b"observed",
        )
        .await
        .unwrap();
    store
        .grant_file(&source, &parent, &observation)
        .await
        .unwrap();
    store.delete_session(&source).await.unwrap();
    assert_eq!(
        store
            .list_files(&parent, &[SessionFileOrigin::Observation])
            .await
            .unwrap()
            .len(),
        1
    );
    let pinned = Dir::open_ambient_dir(workspace.path(), ambient_authority()).unwrap();
    store
        .register_attachment_workspace(&parent, &pinned, workspace.path())
        .await
        .unwrap();
    let staged = workspace
        .path()
        .join(".mobius/attachments")
        .join(session_storage_key(&parent))
        .join(&upload.id);
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::write(staged.join("input.txt"), b"input").unwrap();
    store
        .prepare_delete_sessions(
            &[parent.share()],
            SessionFileSelection::Origins(vec![
                SessionFileOrigin::Artifact,
                SessionFileOrigin::Observation,
            ]),
        )
        .await
        .unwrap()
        .delete()
        .await
        .unwrap();
    assert_eq!(
        store
            .list_files(&parent, &[SessionFileOrigin::Upload])
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .list_files(
                &parent,
                &[SessionFileOrigin::Artifact, SessionFileOrigin::Observation]
            )
            .await
            .unwrap()
            .is_empty()
    );
    store
        .prepare_delete_sessions(
            &[parent.share()],
            SessionFileSelection::Origins(vec![SessionFileOrigin::Upload]),
        )
        .await
        .unwrap()
        .delete()
        .await
        .unwrap();
    assert!(!staged.exists());
    assert_eq!(store.blob_bytes().await.unwrap(), 5);
    assert_eq!(
        store
            .list_files(&fork, &[SessionFileOrigin::Observation])
            .await
            .unwrap()
            .len(),
        1
    );
    store.delete_session(&fork).await.unwrap();
    assert_eq!(store.blob_bytes().await.unwrap(), 0);
}

#[tokio::test]
async fn simultaneous_upload_admission_cannot_overbook_gateway_capacity() {
    let state = tempfile::tempdir().unwrap();
    let store = SessionFileStore::new(state.path(), Some(5));
    let first_id = session("first");
    let second_id = session("second");
    let (first, second) = tokio::join!(
        store.begin_upload(&first_id, "input.txt".into(), 5, "text/plain".into()),
        store.begin_upload(&second_id, "input.txt".into(), 5, "text/plain".into()),
    );
    assert!(matches!(
        (&first, &second),
        (Ok(_), Err(Error::StorageFull)) | (Err(Error::StorageFull), Ok(_))
    ));
    drop((first, second));
    store
        .begin_upload(
            &session("retry"),
            "input.txt".into(),
            5,
            "text/plain".into(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn generated_images_survive_selective_cleanup_until_the_chat_is_deleted() {
    let id = session("session");
    for legacy in [false, true] {
        let state = tempfile::tempdir().unwrap();
        let store = SessionFileStore::new(state.path(), None);
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2))
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let bytes = encoded.into_inner();
        let name = if legacy {
            "generated-image.png"
        } else {
            "custom-name.png"
        };
        let generated = store
            .publish_image(&id, name.into(), "image/png", bytes.clone())
            .await
            .unwrap()
            .file;
        if legacy {
            let path = store
                .session_dir(&id)
                .join(&generated.id)
                .join(METADATA_FILE);
            let mut metadata: serde_json::Value =
                serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
            metadata
                .as_object_mut()
                .unwrap()
                .remove("protected_from_cleanup");
            tokio::fs::write(path, serde_json::to_vec(&metadata).unwrap())
                .await
                .unwrap();
        }
        let ordinary = store
            .publish_artifact(&id, "ordinary.png".into(), "image/png".into(), &bytes)
            .await
            .unwrap();
        let origins = [SessionFileOrigin::Artifact];
        assert_eq!(store.list_files(&id, &origins).await.unwrap().len(), 2);
        assert_eq!(store.read_image(&id, &generated).await.unwrap(), bytes);
        assert_eq!(
            store.list_cleanup_files(&id, &origins).await.unwrap(),
            vec![(SessionFileOrigin::Artifact, ordinary.clone())]
        );

        let mut deletion = store
            .prepare_delete_sessions(
                &[id.share()],
                SessionFileSelection::Ids(vec![ordinary.id.clone(), generated.id.clone()]),
            )
            .await
            .unwrap();
        for _ in 0..2 {
            let error = deletion.delete().await.unwrap_err();
            assert!(error.to_string().contains("generated images"));
        }
        drop(deletion);
        assert_eq!(store.list_files(&id, &origins).await.unwrap().len(), 2);
        assert_eq!(store.read_image(&id, &ordinary).await.unwrap(), bytes);
        assert_eq!(store.read_image(&id, &generated).await.unwrap(), bytes);

        store
            .prepare_delete_sessions(
                &[id.share()],
                SessionFileSelection::Origins(origins.to_vec()),
            )
            .await
            .unwrap()
            .delete()
            .await
            .unwrap();
        assert_eq!(
            store.list_files(&id, &origins).await.unwrap(),
            vec![(SessionFileOrigin::Artifact, generated.clone())]
        );
        assert!(
            store
                .list_cleanup_files(&id, &origins)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.read_image(&id, &generated).await.unwrap(), bytes);
        assert!(store.read_image(&id, &ordinary).await.is_err());

        store.delete_session(&id).await.unwrap();
        assert!(store.list_files(&id, &origins).await.unwrap().is_empty());
        assert!(store.read_image(&id, &generated).await.is_err());
        assert_eq!(store.stored_bytes().await.unwrap(), 0);
    }
}
