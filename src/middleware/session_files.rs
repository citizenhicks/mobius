//! Protected, session-bound storage shared by uploads and agent artifacts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tempfile::TempPath;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{Mutex, OnceCell, OwnedMutexGuard};
use uuid::Uuid;

use crate::protocol::{
    SessionFileLimits, SessionFileOrigin as ProtocolFileOrigin, SessionFileRecord,
    SessionFileReference,
};
use crate::{Error, Result};

mod storage;

#[cfg(test)]
use storage::remember_validated_blob;
pub(crate) use storage::session_storage_key;
use storage::{
    cleanup_stale_files, create_private_dir, ensure_private_dir, gc_unreferenced_blobs, hash_file,
    list_completed, load_attachment_workspace, load_metadata, load_optional_attachment_workspace,
    read_resolved_chunk, remove_staged_attachments, require_directory, save_attachment_workspace,
    save_metadata, set_private_file, validate_content_blob, validate_content_hash,
    validate_file_id, validate_media_type, validate_name, validate_session_id,
    validate_stored_file,
};

const MAX_ATTACHMENT_REFERENCES: usize = 16;
pub(crate) const MAX_FILE_BYTES: u64 = 250 * 1024 * 1024;
pub(crate) const MAX_SESSION_BYTES: u64 = 250 * 1024 * 1024;
const MAX_UPLOAD_CHUNK_BYTES: usize = 256 * 1024;
pub(crate) const MAX_READ_CHUNK_BYTES: usize = 256 * 1024;
const MAX_SESSION_FILES: usize = 128;
const MAX_SESSION_ID_BYTES: usize = 4 * 1024;
const MAX_VALIDATED_BLOBS: usize = 1_024;
const BLOB_DIR: &str = "blobs";
const ATTACHMENT_WORKSPACE_FILE: &str = ".attachment-workspace.json";
const METADATA_FILE: &str = ".session-file.json";

/// Returns the file policy enforced by storage and agent input validation.
#[must_use]
pub const fn session_file_limits() -> SessionFileLimits {
    SessionFileLimits {
        max_attachment_references: MAX_ATTACHMENT_REFERENCES,
        max_file_bytes: MAX_FILE_BYTES,
        max_session_files: MAX_SESSION_FILES,
        max_session_bytes: MAX_SESSION_BYTES,
        max_upload_chunk_bytes: MAX_UPLOAD_CHUNK_BYTES,
    }
}

/// One bounded range read from a stored session file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFileChunk {
    pub offset: u64,
    pub data: Vec<u8>,
    pub next_offset: Option<u64>,
}

/// Protected immutable file storage shared by inbound and outbound transports.
///
/// Display names live only in metadata; payloads always use an internal filename.
#[derive(Clone)]
pub struct SessionFileStore {
    root: Arc<PathBuf>,
    // ponytail: one commit lock keeps quota checks and publication atomic.
    commits: Arc<Mutex<()>>,
    reservations: Arc<StdMutex<BTreeMap<String, ReservationTotals>>>,
    // ponytail: immutable private blobs reuse one verified SHA-256 while metadata is unchanged.
    validated_blobs: Arc<StdMutex<BTreeMap<String, BlobValidationStamp>>>,
    initialized: Arc<OnceCell<()>>,
}

/// Prepared deletion whose commit lock prevents new uploads until cleanup finishes.
pub struct SessionFileDeletion {
    store: SessionFileStore,
    session_ids: Vec<String>,
    _commit: OwnedMutexGuard<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct BlobValidationStamp {
    size: u64,
    modified: SystemTime,
}

#[derive(Default)]
struct ReservationTotals {
    files: usize,
    bytes: u64,
}

struct SessionFileReservation {
    reservations: Arc<StdMutex<BTreeMap<String, ReservationTotals>>>,
    session_id: String,
    size: u64,
    active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionFileOrigin {
    Upload,
    Artifact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSessionFile {
    origin: SessionFileOrigin,
    file: SessionFileReference,
    content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAttachmentWorkspace {
    path: PathBuf,
}

impl SessionFileStore {
    /// Creates a store below the gateway's already protected state directory.
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self {
            root: Arc::new(state_dir.join("session-files")),
            commits: Arc::new(Mutex::new(())),
            reservations: Arc::new(StdMutex::new(BTreeMap::new())),
            validated_blobs: Arc::new(StdMutex::new(BTreeMap::new())),
            initialized: Arc::new(OnceCell::new()),
        }
    }

    /// Starts one connection-owned user upload.
    pub async fn begin_upload(
        &self,
        session_id: &str,
        name: String,
        size: u64,
        media_type: String,
    ) -> Result<PendingSessionFileWrite> {
        self.begin(
            session_id,
            name,
            size,
            media_type,
            SessionFileOrigin::Upload,
        )
        .await
    }

    /// Publishes one immutable agent artifact.
    pub async fn publish_artifact(
        &self,
        session_id: &str,
        name: String,
        media_type: String,
        bytes: &[u8],
    ) -> Result<SessionFileReference> {
        let size = u64::try_from(bytes.len())
            .map_err(|_| Error::Tool("artifact size is unsupported".into()))?;
        let mut pending = self
            .begin(
                session_id,
                name,
                size,
                media_type,
                SessionFileOrigin::Artifact,
            )
            .await?;
        for chunk in bytes.chunks(MAX_UPLOAD_CHUNK_BYTES) {
            let offset = pending.written;
            pending.append(offset, chunk).await?;
        }
        pending.finish().await
    }

    /// Lists completed user uploads for one session.
    pub async fn list_uploads(&self, session_id: &str) -> Result<Vec<SessionFileReference>> {
        self.list_origin(session_id, SessionFileOrigin::Upload)
            .await
    }

    /// Lists completed agent artifacts for one session.
    pub async fn list_artifacts(&self, session_id: &str) -> Result<Vec<SessionFileReference>> {
        self.list_origin(session_id, SessionFileOrigin::Artifact)
            .await
    }

    /// Lists every completed file together with the side that produced it.
    pub async fn list_files(&self, session_id: &str) -> Result<Vec<SessionFileRecord>> {
        validate_session_id(session_id)?;
        self.ensure_initialized().await?;
        Ok(
            list_completed(&self.session_dir(session_id), &self.blob_dir())
                .await?
                .into_iter()
                .map(|record| SessionFileRecord {
                    origin: match record.origin {
                        SessionFileOrigin::Upload => ProtocolFileOrigin::User,
                        SessionFileOrigin::Artifact => ProtocolFileOrigin::Agent,
                    },
                    file: record.file,
                })
                .collect(),
        )
    }

    pub(crate) async fn register_attachment_workspace(
        &self,
        session_id: &str,
        workspace: &Path,
    ) -> Result<()> {
        validate_session_id(session_id)?;
        let workspace = tokio::fs::canonicalize(workspace).await?;
        if !workspace.is_dir() {
            return Err(Error::Config(
                "attachment workspace is not a directory".into(),
            ));
        }
        self.ensure_initialized().await?;
        let _commit = self.commits.lock().await;
        let directory = self.session_dir(session_id);
        ensure_private_dir(&directory).await?;
        let stored = StoredAttachmentWorkspace { path: workspace };
        let destination = directory.join(ATTACHMENT_WORKSPACE_FILE);
        match load_attachment_workspace(&destination).await {
            Ok(existing) if existing == stored => Ok(()),
            Ok(_) => Err(Error::Config(
                "attachment workspace changed for the active session".into(),
            )),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                save_attachment_workspace(&directory, &stored).await
            }
            Err(error) => Err(error),
        }
    }

    /// Permanently removes every upload and artifact owned by one idle session.
    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let mut deletion = self
            .prepare_delete_sessions(&[session_id.to_owned()])
            .await?;
        deletion.delete().await
    }

    /// Permanently removes one completed user upload.
    pub async fn delete_upload(&self, session_id: &str, file_id: &str) -> Result<()> {
        validate_session_id(session_id)?;
        validate_file_id(file_id)?;
        self.ensure_initialized().await?;
        let _commit = self.commits.lock().await;
        let directory = self.session_dir(session_id).join(file_id);
        match tokio::fs::symlink_metadata(&directory).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        self.resolve_upload_record(session_id, file_id).await?;
        tokio::fs::remove_dir_all(directory).await?;
        gc_unreferenced_blobs(&self.root).await
    }

    /// Validates a group deletion and prevents new upload reservations until it completes.
    pub async fn prepare_delete_sessions(
        &self,
        session_ids: &[String],
    ) -> Result<SessionFileDeletion> {
        for session_id in session_ids {
            validate_session_id(session_id)?;
        }
        self.ensure_initialized().await?;
        let commit = Arc::clone(&self.commits).lock_owned().await;
        {
            let reservations = self
                .reservations
                .lock()
                .map_err(|_| Error::Tool("session file reservation lock is poisoned".into()))?;
            if let Some(session_id) = session_ids
                .iter()
                .find(|session_id| reservations.contains_key(*session_id))
            {
                return Err(Error::Tool(format!(
                    "session files for `{session_id}` cannot be deleted while an upload is active"
                )));
            }
        }
        for session_id in session_ids {
            self.validate_session_deletion(session_id).await?;
        }
        Ok(SessionFileDeletion {
            store: self.clone(),
            session_ids: session_ids.to_vec(),
            _commit: commit,
        })
    }

    async fn validate_session_deletion(&self, session_id: &str) -> Result<()> {
        let directory = self.session_dir(session_id);
        match tokio::fs::symlink_metadata(&directory).await {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                load_optional_attachment_workspace(&directory).await?;
            }
            Ok(_) => {
                return Err(Error::Tool(
                    "session file directory is not a protected directory".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    async fn delete_session_locked(&self, session_id: &str) -> Result<()> {
        let directory = self.session_dir(session_id);
        match tokio::fs::symlink_metadata(&directory).await {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                let workspace = load_optional_attachment_workspace(&directory).await?;
                if let Some(workspace) = workspace {
                    remove_staged_attachments(&workspace.path, session_id).await?;
                }
                tokio::fs::remove_dir_all(directory).await?;
            }
            Ok(_) => {
                return Err(Error::Tool(
                    "session file directory is not a protected directory".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        gc_unreferenced_blobs(&self.root).await?;
        Ok(())
    }

    /// Reads one bounded byte range from either kind of stored session file.
    pub async fn read_chunk(
        &self,
        session_id: &str,
        file_id: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<SessionFileChunk> {
        let (record, path) = self.resolve(session_id, file_id).await?;
        read_resolved_chunk(record.file, path, offset, max_bytes).await
    }

    /// Verifies that a frontend reference names the exact user upload.
    pub async fn verify_upload(
        &self,
        session_id: &str,
        expected: &SessionFileReference,
    ) -> Result<()> {
        let (actual, _) = self.resolve_upload(session_id, &expected.id).await?;
        if &actual != expected {
            return Err(Error::Tool(
                "session file metadata does not match the uploaded file".into(),
            ));
        }
        Ok(())
    }

    /// Resolves an owned upload to its private content-addressed identity.
    pub(crate) async fn upload_content_hash(
        &self,
        session_id: &str,
        expected: &SessionFileReference,
    ) -> Result<String> {
        let (record, _) = self.resolve_upload_record(session_id, &expected.id).await?;
        if &record.file != expected {
            return Err(Error::Tool(
                "session file metadata does not match the uploaded file".into(),
            ));
        }
        Ok(record.content_hash)
    }

    /// Reads a content-addressed blob after validating its size and SHA-256 identity.
    pub(crate) async fn read_content_blob(&self, content_hash: &str, size: u64) -> Result<Vec<u8>> {
        let path = self.content_blob_path(content_hash, size).await?;
        Ok(tokio::fs::read(path).await?)
    }

    /// Resolves a validated content blob for workspace staging.
    pub(crate) async fn content_blob_path(&self, content_hash: &str, size: u64) -> Result<PathBuf> {
        validate_content_hash(content_hash)?;
        let path = self.blob_path(content_hash);
        validate_content_blob(&path, content_hash, size, &self.validated_blobs).await?;
        Ok(path)
    }

    async fn begin(
        &self,
        session_id: &str,
        name: String,
        size: u64,
        media_type: String,
        origin: SessionFileOrigin,
    ) -> Result<PendingSessionFileWrite> {
        validate_session_id(session_id)?;
        validate_name(&name)?;
        validate_media_type(&media_type)?;
        if !(1..=MAX_FILE_BYTES).contains(&size) {
            return Err(Error::Tool(format!(
                "session file size must be 1–{MAX_FILE_BYTES} bytes"
            )));
        }
        self.ensure_initialized().await?;
        let _commit = self.commits.lock().await;
        let session_dir = self.session_dir(session_id);
        ensure_private_dir(&session_dir).await?;
        let existing = list_completed(&session_dir, &self.blob_dir()).await?;
        let reservation = self.reserve(session_id, size, &existing)?;
        let record = StoredSessionFile {
            origin,
            file: SessionFileReference {
                id: Uuid::new_v4().to_string(),
                name,
                size,
                media_type,
            },
            content_hash: String::new(),
        };
        let temporary = tempfile::NamedTempFile::new_in(&session_dir)?;
        set_private_file(temporary.path()).await?;
        let (file, path) = temporary.into_parts();
        Ok(PendingSessionFileWrite {
            store: self.clone(),
            session_id: session_id.into(),
            record,
            reservation,
            written: 0,
            file: Some(tokio::fs::File::from_std(file)),
            path: Some(path),
        })
    }

    async fn list_origin(
        &self,
        session_id: &str,
        origin: SessionFileOrigin,
    ) -> Result<Vec<SessionFileReference>> {
        validate_session_id(session_id)?;
        self.ensure_initialized().await?;
        Ok(
            list_completed(&self.session_dir(session_id), &self.blob_dir())
                .await?
                .into_iter()
                .filter(|record| record.origin == origin)
                .map(|record| record.file)
                .collect(),
        )
    }

    async fn resolve(
        &self,
        session_id: &str,
        file_id: &str,
    ) -> Result<(StoredSessionFile, PathBuf)> {
        validate_session_id(session_id)?;
        validate_file_id(file_id)?;
        self.ensure_initialized().await?;
        let directory = self.session_dir(session_id).join(file_id);
        require_directory(&directory).await?;
        let metadata = load_metadata(&directory.join(METADATA_FILE)).await?;
        if metadata.file.id != file_id {
            return Err(Error::Tool(
                "session file metadata has an invalid ID".into(),
            ));
        }
        validate_stored_file(&metadata)?;
        let path = self.blob_path(&metadata.content_hash);
        validate_content_blob(
            &path,
            &metadata.content_hash,
            metadata.file.size,
            &self.validated_blobs,
        )
        .await?;
        Ok((metadata, path))
    }

    async fn resolve_upload(
        &self,
        session_id: &str,
        file_id: &str,
    ) -> Result<(SessionFileReference, PathBuf)> {
        let (record, path) = self.resolve_upload_record(session_id, file_id).await?;
        Ok((record.file, path))
    }

    async fn resolve_upload_record(
        &self,
        session_id: &str,
        file_id: &str,
    ) -> Result<(StoredSessionFile, PathBuf)> {
        let (record, path) = self.resolve(session_id, file_id).await?;
        if record.origin != SessionFileOrigin::Upload {
            return Err(Error::Tool(
                "session file is not a file uploaded by the user".into(),
            ));
        }
        Ok((record, path))
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_storage_key(session_id))
    }

    fn blob_dir(&self) -> PathBuf {
        self.root.join(BLOB_DIR)
    }

    fn blob_path(&self, content_hash: &str) -> PathBuf {
        self.blob_dir().join(content_hash)
    }

    async fn ensure_initialized(&self) -> Result<()> {
        self.initialized
            .get_or_try_init(|| async {
                ensure_private_dir(&self.root).await?;
                cleanup_stale_files(&self.root).await
            })
            .await
            .map(|_| ())
    }

    fn reserve(
        &self,
        session_id: &str,
        size: u64,
        completed: &[StoredSessionFile],
    ) -> Result<SessionFileReservation> {
        let completed_bytes = completed.iter().try_fold(0_u64, |total, item| {
            total
                .checked_add(item.file.size)
                .ok_or_else(|| Error::Tool("session file quota overflow".into()))
        })?;
        let mut reservations = self
            .reservations
            .lock()
            .map_err(|_| Error::Tool("session file reservation state is unavailable".into()))?;
        let pending_files = reservations
            .get(session_id)
            .map_or(0, |pending| pending.files);
        let pending_bytes = reservations
            .get(session_id)
            .map_or(0, |pending| pending.bytes);
        if completed.len().saturating_add(pending_files) >= MAX_SESSION_FILES {
            return Err(Error::Tool(format!(
                "session cannot contain more than {MAX_SESSION_FILES} files"
            )));
        }
        let reserved_bytes = completed_bytes
            .checked_add(pending_bytes)
            .and_then(|total| total.checked_add(size))
            .ok_or_else(|| Error::Tool("session file quota overflow".into()))?;
        if reserved_bytes > MAX_SESSION_BYTES {
            return Err(Error::Tool(format!(
                "session files exceed {MAX_SESSION_BYTES} bytes"
            )));
        }
        let pending = reservations.entry(session_id.into()).or_default();
        pending.files += 1;
        pending.bytes += size;
        Ok(SessionFileReservation {
            reservations: Arc::clone(&self.reservations),
            session_id: session_id.into(),
            size,
            active: true,
        })
    }

    fn validate_reserved_capacity(
        &self,
        session_id: &str,
        completed: &[StoredSessionFile],
    ) -> Result<()> {
        let completed_bytes = completed.iter().try_fold(0_u64, |total, item| {
            total
                .checked_add(item.file.size)
                .ok_or_else(|| Error::Tool("session file quota overflow".into()))
        })?;
        let reservations = self
            .reservations
            .lock()
            .map_err(|_| Error::Tool("session file reservation state is unavailable".into()))?;
        let pending = reservations.get(session_id);
        let pending_files = pending.map_or(0, |pending| pending.files);
        let pending_bytes = pending.map_or(0, |pending| pending.bytes);
        if completed.len().saturating_add(pending_files) > MAX_SESSION_FILES
            || completed_bytes
                .checked_add(pending_bytes)
                .is_none_or(|bytes| bytes > MAX_SESSION_BYTES)
        {
            return Err(Error::Tool("session file reservation exceeds quota".into()));
        }
        Ok(())
    }
}

impl SessionFileDeletion {
    /// Removes every prepared session while keeping new uploads excluded.
    pub async fn delete(&mut self) -> Result<()> {
        for session_id in &self.session_ids {
            self.store.delete_session_locked(session_id).await?;
        }
        Ok(())
    }
}

/// An incomplete immutable session-file write.
pub struct PendingSessionFileWrite {
    store: SessionFileStore,
    session_id: String,
    record: StoredSessionFile,
    reservation: SessionFileReservation,
    written: u64,
    file: Option<tokio::fs::File>,
    path: Option<TempPath>,
}

impl PendingSessionFileWrite {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.record.file.id
    }

    /// Appends the next exact chunk.
    pub async fn append(&mut self, offset: u64, data: &[u8]) -> Result<u64> {
        if data.is_empty() || data.len() > MAX_UPLOAD_CHUNK_BYTES {
            return Err(Error::Tool(format!(
                "session file chunk must be 1–{MAX_UPLOAD_CHUNK_BYTES} bytes"
            )));
        }
        if offset != self.written {
            return Err(Error::Tool(format!(
                "session file offset must be {}",
                self.written
            )));
        }
        let next = self
            .written
            .checked_add(data.len() as u64)
            .ok_or_else(|| Error::Tool("session file size overflow".into()))?;
        if next > self.record.file.size {
            return Err(Error::Tool(
                "chunk exceeds declared session file size".into(),
            ));
        }
        self.file
            .as_mut()
            .ok_or_else(|| Error::Tool("session file upload is already finished".into()))?
            .write_all(data)
            .await?;
        self.written = next;
        Ok(next)
    }

    /// Atomically publishes a complete session file.
    pub async fn finish(mut self) -> Result<SessionFileReference> {
        if self.written != self.record.file.size {
            return Err(Error::Tool(format!(
                "session file upload has {} of {} bytes",
                self.written, self.record.file.size
            )));
        }
        let mut file = self
            .file
            .take()
            .ok_or_else(|| Error::Tool("session file upload is already finished".into()))?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        let _guard = self.store.commits.lock().await;
        let session_dir = self.store.session_dir(&self.session_id);
        let existing = list_completed(&session_dir, &self.store.blob_dir()).await?;
        self.store
            .validate_reserved_capacity(&self.session_id, &existing)?;

        let directory = session_dir.join(&self.record.file.id);
        if tokio::fs::symlink_metadata(&directory).await.is_ok() {
            return Err(Error::Tool("session file ID already exists".into()));
        }
        let source = self
            .path
            .take()
            .ok_or_else(|| Error::Tool("session file temporary file is missing".into()))?;
        let source_path = source.to_path_buf();
        let content_hash = hash_file(&source_path).await?;
        validate_content_hash(&content_hash)?;
        self.record.content_hash = content_hash.clone();

        let blob_dir = self.store.blob_dir();
        ensure_private_dir(&blob_dir).await?;
        let blob_path = self.store.blob_path(&content_hash);
        match tokio::fs::hard_link(&source_path, &blob_path).await {
            Ok(()) => set_private_file(&blob_path).await?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                validate_content_blob(
                    &blob_path,
                    &content_hash,
                    self.record.file.size,
                    &self.store.validated_blobs,
                )
                .await?;
            }
            Err(error) => return Err(error.into()),
        }
        tokio::fs::remove_file(&source_path).await?;

        let staging = session_dir.join(format!(".{}-partial", self.record.file.id));
        create_private_dir(&staging).await?;
        if let Err(error) = save_metadata(&staging, &self.record).await {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            let _ = gc_unreferenced_blobs(&self.store.root).await;
            return Err(error);
        }
        if let Err(error) = tokio::fs::rename(&staging, &directory).await {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            let _ = gc_unreferenced_blobs(&self.store.root).await;
            return Err(error.into());
        }
        self.reservation.release();
        Ok(self.record.file.clone())
    }
}

impl SessionFileReservation {
    fn release(&mut self) {
        if !self.active {
            return;
        }
        let mut reservations = self
            .reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remove = if let Some(pending) = reservations.get_mut(&self.session_id) {
            pending.files = pending.files.saturating_sub(1);
            pending.bytes = pending.bytes.saturating_sub(self.size);
            pending.files == 0
        } else {
            false
        };
        if remove {
            reservations.remove(&self.session_id);
        }
        self.active = false;
    }
}

impl Drop for SessionFileReservation {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests;
