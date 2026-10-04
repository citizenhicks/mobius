//! Protected, session-bound storage shared by uploads and agent artifacts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;

use cap_std::fs::Dir;
#[cfg(unix)]
use cap_std::fs::MetadataExt as _;
use serde::{Deserialize, Serialize};
use tempfile::TempPath;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{Mutex, OnceCell, OwnedMutexGuard};
use uuid::Uuid;

use crate::protocol::{SessionFileLimits, SessionFileReference};
use crate::{Error, Result};

mod images;
pub use images::ImagePresentation;
pub use images::grant_context;
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
const MAX_SESSION_FILES: usize = 512;
const MAX_SESSION_ID_BYTES: usize = 4 * 1024;
const MAX_VALIDATED_BLOBS: usize = 1_024;
const BLOB_DIR: &str = "blobs";
const DELETED_PREFIX: &str = ".deleted-";
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
    /// The offset.
    pub offset: u64,
    /// The data.
    pub data: Vec<u8>,
    /// The next offset.
    pub next_offset: Option<u64>,
}

/// Protected immutable file storage shared by inbound and outbound transports.
///
/// Display names live only in metadata; payloads always use an internal filename.
#[derive(Clone)]
pub struct SessionFileStore {
    root: Arc<PathBuf>,
    limit_bytes: Option<u64>,
    // ponytail: one commit lock keeps quota checks and publication atomic.
    commits: Arc<Mutex<()>>,
    reservations: Arc<StdMutex<BTreeMap<String, ReservationTotals>>>,
    // ponytail: immutable private blobs reuse one verified SHA-256 while metadata is unchanged.
    validated_blobs: Arc<StdMutex<BTreeMap<String, BlobValidationStamp>>>,
    initialized: Arc<OnceCell<()>>,
    image_policy: ImagePresentation,
    image_work: Arc<tokio::sync::Semaphore>,
    renditions: Arc<Mutex<()>>,
}

/// Prepared deletion whose commit lock protects removal from the live file catalog.
pub struct SessionFileDeletion {
    store: SessionFileStore,
    session_ids: Vec<String>,
    selection: SessionFileSelection,
    commit: Option<OwnedMutexGuard<()>>,
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

/// Storage origin used for listing and selective deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFileOrigin {
    /// Hidden screenshots and retained context.
    Observation,
    /// User supplied file.
    Upload,
    /// Agent generated file.
    Artifact,
}

/// Explicit scope of a session-file deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFileSelection {
    /// Remove the entire session's files when deleting its history.
    All,
    /// Remove references of the selected origins, preserving history.
    Origins(Vec<SessionFileOrigin>),
    /// Remove named references from exactly one session.
    Ids(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSessionFile {
    origin: SessionFileOrigin,
    file: SessionFileReference,
    content_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image_dimensions: Option<[u32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image_orientation: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image_rendition_key: Option<String>,
    #[serde(default)]
    protected_from_cleanup: bool,
}

impl StoredSessionFile {
    fn protected_from_cleanup(&self) -> bool {
        self.protected_from_cleanup
            || (self.origin == SessionFileOrigin::Artifact
                && self.file.name.starts_with("generated-image.")
                && self.file.media_type.starts_with("image/"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAttachmentWorkspace {
    path: PathBuf,
    identity: AttachmentWorkspaceIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachmentWorkspaceIdentity {
    #[cfg(all(unix, not(target_os = "macos")))]
    device: u64,
    #[cfg(target_os = "macos")]
    volume_uuid: String,
    #[cfg(unix)]
    inode: u64,
}

impl AttachmentWorkspaceIdentity {
    fn from_workspace(workspace: &Dir, _path: &Path) -> Result<Self> {
        let metadata = workspace.dir_metadata()?;
        #[cfg(target_os = "macos")]
        {
            let location = whichdisk::resolve(_path)?;
            let volume_uuid = match location
                .volume_identity()
                .filter(whichdisk::IdentityReading::is_vouched)
                .map(|reading| reading.identity())
            {
                Some(whichdisk::VolumeIdentity::FsUuid(bytes)) => {
                    uuid::Uuid::from_bytes(bytes).to_string()
                }
                _ => {
                    return Err(Error::Config(
                        "attachment workspace volume has no persistent UUID".into(),
                    ));
                }
            };
            let path_metadata = std::fs::metadata(_path)?;
            use std::os::unix::fs::MetadataExt as _;
            if metadata.dev() != path_metadata.dev() || metadata.ino() != path_metadata.ino() {
                return Err(Error::Config(
                    "attachment workspace changed while resolving its identity".into(),
                ));
            }
            Ok(Self {
                volume_uuid,
                inode: metadata.ino(),
            })
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Err(Error::Config(
                "attachment workspace identity is unavailable".into(),
            ))
        }
    }

    fn matches(&self, workspace: &Dir, path: &Path) -> Result<bool> {
        Ok(self == &Self::from_workspace(workspace, path)?)
    }
}

impl SessionFileStore {
    /// Creates a store below the gateway's already protected state directory.
    /// `limit_bytes` bounds committed blobs plus reserved writes; `None` is unlimited.
    #[must_use]
    pub fn new(state_dir: &Path, limit_bytes: Option<u64>) -> Self {
        Self {
            root: Arc::new(state_dir.join("session-files")),
            limit_bytes,
            commits: Arc::new(Mutex::new(())),
            reservations: Arc::new(StdMutex::new(BTreeMap::new())),
            validated_blobs: Arc::new(StdMutex::new(BTreeMap::new())),
            initialized: Arc::new(OnceCell::new()),
            image_policy: ImagePresentation::default(),
            image_work: Arc::new(tokio::sync::Semaphore::new(2)),
            renditions: Arc::new(Mutex::new(())),
        }
    }

    /// Starts one connection-owned user upload.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
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
            false,
        )
        .await
    }

    /// Publishes one immutable agent artifact.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn publish_artifact(
        &self,
        session_id: &str,
        name: String,
        media_type: String,
        bytes: &[u8],
    ) -> Result<SessionFileReference> {
        self.publish_bytes(
            session_id,
            name,
            media_type,
            bytes,
            SessionFileOrigin::Artifact,
            false,
        )
        .await
    }

    /// Lists completed references of the requested origins.
    /// # Errors
    /// Returns an error if file metadata cannot be read or validated.
    pub async fn list_files(
        &self,
        session_id: &str,
        origins: &[SessionFileOrigin],
    ) -> Result<Vec<(SessionFileOrigin, SessionFileReference)>> {
        self.list_files_for(session_id, origins, false).await
    }

    /// Lists only files that selective cleanup can remove while retaining chat history.
    /// # Errors
    /// Returns an error if file metadata cannot be read or validated.
    pub async fn list_cleanup_files(
        &self,
        session_id: &str,
        origins: &[SessionFileOrigin],
    ) -> Result<Vec<(SessionFileOrigin, SessionFileReference)>> {
        self.list_files_for(session_id, origins, true).await
    }

    async fn list_files_for(
        &self,
        session_id: &str,
        origins: &[SessionFileOrigin],
        cleanup_only: bool,
    ) -> Result<Vec<(SessionFileOrigin, SessionFileReference)>> {
        validate_session_id(session_id)?;
        self.ensure_initialized().await?;
        Ok(
            list_completed(&self.session_dir(session_id), &self.blob_dir())
                .await?
                .into_iter()
                .filter(|record| {
                    origins.contains(&record.origin)
                        && (!cleanup_only || !record.protected_from_cleanup())
                })
                .map(|record| (record.origin, record.file))
                .collect(),
        )
    }

    pub(crate) async fn register_attachment_workspace(
        &self,
        session_id: &str,
        workspace: &Dir,
        workspace_path: &Path,
    ) -> Result<()> {
        validate_session_id(session_id)?;
        let identity = AttachmentWorkspaceIdentity::from_workspace(workspace, workspace_path)?;
        self.ensure_initialized().await?;
        let _commit = self.commits.lock().await;
        let directory = self.session_dir(session_id);
        ensure_private_dir(&directory).await?;
        let stored = StoredAttachmentWorkspace {
            path: workspace_path.to_owned(),
            identity,
        };
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
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let mut deletion = self
            .prepare_delete_sessions(&[session_id.to_owned()], SessionFileSelection::All)
            .await?;
        deletion.delete().await
    }

    /// Validates a batch deletion and prevents new upload reservations until it completes.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn prepare_delete_sessions(
        &self,
        session_ids: &[String],
        selection: SessionFileSelection,
    ) -> Result<SessionFileDeletion> {
        if matches!(&selection, SessionFileSelection::Ids(ids) if ids.is_empty())
            || matches!(&selection, SessionFileSelection::Origins(origins) if origins.is_empty())
        {
            return Err(Error::Tool("select at least one file ID or origin".into()));
        }
        if let SessionFileSelection::Ids(ids) = &selection {
            if session_ids.len() != 1 {
                return Err(Error::Tool("file IDs require exactly one session".into()));
            }
            for id in ids {
                validate_file_id(id)?;
            }
        }
        if session_ids.is_empty() {
            return Ok(SessionFileDeletion {
                store: self.clone(),
                session_ids: Vec::new(),
                selection,
                commit: None,
            });
        }
        for session_id in session_ids {
            validate_session_id(session_id)?;
        }
        self.ensure_initialized().await?;
        let commit = Arc::clone(&self.commits).lock_owned().await;
        if selection == SessionFileSelection::All {
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
            selection,
            commit: Some(commit),
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

    /// Retries workspace cleanup left by completed session deletions.
    ///
    /// Run outside gateway mutation locks: a workspace may be unavailable.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn cleanup_deleted_sessions(&self) -> Result<()> {
        self.ensure_initialized().await?;
        let mut entries = tokio::fs::read_dir(self.root.as_ref()).await?;
        // ponytail: sequential retries; bound parallel cleanup if unavailable mounts delay reclamation.
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let Some(key) = name
                .to_str()
                .and_then(|name| name.strip_prefix(DELETED_PREFIX))
            else {
                continue;
            };
            if key.len() != 43
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                return Err(Error::Tool("invalid deleted session file key".into()));
            }
            self.cleanup_deleted_directory(key).await?;
        }
        let _commit = self.commits.lock().await;
        gc_unreferenced_blobs(&self.root).await
    }

    async fn cleanup_deleted_directory(&self, key: &str) -> Result<()> {
        let directory = self.root.join(format!("{DELETED_PREFIX}{key}"));
        match tokio::fs::symlink_metadata(&directory).await {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                let workspace = load_optional_attachment_workspace(&directory).await?;
                if let Some(workspace) = workspace {
                    remove_staged_attachments(&workspace, key).await?;
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
        Ok(())
    }

    /// Reads one bounded byte range from either kind of stored session file.
    /// # Errors
    ///
    /// Returns an error if the resource cannot be read, decoded, or validated.
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
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
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
        protected_from_cleanup: bool,
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
        self.validate_storage_capacity(size).await?;
        let reservation = self.reserve_session(session_id, size, &existing)?;
        let record = StoredSessionFile {
            origin,
            file: SessionFileReference {
                id: Uuid::new_v4().to_string(),
                name,
                size,
                media_type,
            },
            content_hash: String::new(),
            image_dimensions: None,
            image_orientation: None,
            image_rendition_key: None,
            protected_from_cleanup,
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

    async fn publish_bytes(
        &self,
        session_id: &str,
        name: String,
        media_type: String,
        bytes: &[u8],
        origin: SessionFileOrigin,
        protected_from_cleanup: bool,
    ) -> Result<SessionFileReference> {
        let size = u64::try_from(bytes.len())
            .map_err(|_| Error::Tool("file size is unsupported".into()))?;
        let pending = self
            .begin(
                session_id,
                name,
                size,
                media_type,
                origin,
                protected_from_cleanup,
            )
            .await?;
        pending.complete_bytes(bytes).await
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

    pub(crate) async fn file_is_missing(&self, session_id: &str, file_id: &str) -> Result<bool> {
        validate_session_id(session_id)?;
        validate_file_id(file_id)?;
        self.ensure_initialized().await?;
        match tokio::fs::symlink_metadata(self.session_dir(session_id).join(file_id)).await {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(false),
            Ok(_) => Err(Error::Tool("session file path is not a directory".into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error.into()),
        }
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

    /// Measures content bytes charged against the gateway allowance.
    /// # Errors
    /// Returns an error if the protected blob directory cannot be measured.
    pub async fn stored_bytes(&self) -> Result<u64> {
        self.ensure_initialized().await?;
        let _commit = self.commits.lock().await;
        self.blob_bytes().await
    }

    async fn blob_bytes(&self) -> Result<u64> {
        let mut entries = match tokio::fs::read_dir(self.blob_dir()).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = 0_u64;
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                return Err(Error::Tool("invalid content blob entry".into()));
            }
            bytes = bytes
                .checked_add(entry.metadata().await?.len())
                .ok_or_else(|| Error::Tool("session file quota overflow".into()))?;
        }
        Ok(bytes)
    }

    // Call while holding the commit lock so the scan and reservation admission agree.
    async fn validate_storage_capacity(&self, size: u64) -> Result<()> {
        let Some(limit) = self.limit_bytes else {
            return Ok(());
        };
        // ponytail: one flat blob scan per write; use a durable counter if blob counts grow large.
        let mut charged = self
            .blob_bytes()
            .await?
            .checked_add(size)
            .ok_or_else(|| Error::Tool("session file quota overflow".into()))?;
        let reservations = self
            .reservations
            .lock()
            .map_err(|_| Error::Tool("session file reservation state is unavailable".into()))?;
        for pending in reservations.values() {
            charged = charged
                .checked_add(pending.bytes)
                .ok_or_else(|| Error::Tool("session file quota overflow".into()))?;
        }
        if charged > limit {
            return Err(Error::StorageFull);
        }
        Ok(())
    }

    fn reserve_session(
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
    /// Durably removes files from the live catalog without opening any workspace.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn stage(&mut self) -> Result<()> {
        if self.commit.is_none() {
            return Ok(());
        }
        if self.selection != SessionFileSelection::All {
            return Ok(());
        }
        for session_id in &self.session_ids {
            let key = session_storage_key(session_id);
            let destination = self.store.root.join(format!("{DELETED_PREFIX}{key}"));
            match tokio::fs::rename(self.store.session_dir(session_id), destination).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        std::fs::File::open(self.store.root.as_ref())?.sync_all()?;
        self.commit.take();
        Ok(())
    }

    /// Finishes staged workspace cleanup without holding the file commit lock.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn delete(&mut self) -> Result<()> {
        if self.session_ids.is_empty() {
            self.commit.take();
            return Ok(());
        }
        if self.selection != SessionFileSelection::All {
            return self.delete_selected().await;
        }
        self.stage().await?;
        for session_id in &self.session_ids {
            self.store
                .cleanup_deleted_directory(&session_storage_key(session_id))
                .await?;
        }
        let _commit = self.store.commits.lock().await;
        gc_unreferenced_blobs(&self.store.root).await
    }
    async fn delete_selected(&mut self) -> Result<()> {
        let Some(_commit) = self.commit.take() else {
            return Ok(());
        };
        if let SessionFileSelection::Ids(ids) = &self.selection {
            let directory = self.store.session_dir(&self.session_ids[0]);
            for id in ids {
                let path = directory.join(id);
                if tokio::fs::symlink_metadata(&path)
                    .await
                    .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
                    && let Ok(record) = load_metadata(&path.join(METADATA_FILE)).await
                    && record.protected_from_cleanup()
                {
                    return Err(Error::Tool(
                        "generated images cannot be removed while their chat history remains"
                            .into(),
                    ));
                }
            }
        }
        for session_id in &self.session_ids {
            let directory = self.store.session_dir(session_id);
            let workspace = load_optional_attachment_workspace(&directory).await?;
            let mut entries = match tokio::fs::read_dir(&directory).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            while let Some(entry) = entries.next_entry().await? {
                if !entry.file_type().await?.is_dir() {
                    continue;
                }
                let id = entry.file_name();
                let Some(id) = id.to_str().filter(|id| validate_file_id(id).is_ok()) else {
                    continue;
                };
                let record = load_metadata(&entry.path().join(METADATA_FILE)).await;
                let selected = match &self.selection {
                    SessionFileSelection::All => unreachable!("full deletion is staged"),
                    SessionFileSelection::Origins(origins) => record.as_ref().is_ok_and(|record| {
                        origins.contains(&record.origin) && !record.protected_from_cleanup()
                    }),
                    SessionFileSelection::Ids(ids) => ids.iter().any(|selected| selected == id),
                };
                if !selected {
                    continue;
                }
                // IDs can remove unreadable metadata; conservatively clean any staged upload copy.
                if record
                    .as_ref()
                    .map_or(true, |record| record.origin == SessionFileOrigin::Upload)
                    && let Some(workspace) = &workspace
                {
                    remove_staged_attachments(
                        workspace,
                        &format!("{}/{id}", session_storage_key(session_id)),
                    )
                    .await?;
                }
                tokio::fs::remove_dir_all(entry.path()).await?;
            }
        }
        gc_unreferenced_blobs(&self.store.root).await
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
    async fn complete_bytes(mut self, bytes: &[u8]) -> Result<SessionFileReference> {
        for chunk in bytes.chunks(MAX_UPLOAD_CHUNK_BYTES) {
            self.append(self.written, chunk).await?;
        }
        self.finish().await
    }

    #[must_use]
    /// Returns the pending file identifier.
    pub fn id(&self) -> &str {
        &self.record.file.id
    }

    /// Appends the next exact chunk.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
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
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
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
        let mut reservations = crate::sync::recover_lock(&self.reservations);
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
