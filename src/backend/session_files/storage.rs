use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex as StdMutex;

use base64::Engine as _;
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
use uuid::Uuid;

use super::{
    ATTACHMENT_WORKSPACE_FILE, BLOB_DIR, BlobValidationStamp, MAX_FILE_BYTES, MAX_READ_CHUNK_BYTES,
    MAX_SESSION_ID_BYTES, MAX_VALIDATED_BLOBS, METADATA_FILE, SessionFileChunk,
    StoredAttachmentWorkspace, StoredSessionFile,
};
use crate::protocol::SessionFileReference;
use crate::{Error, Result};

pub(super) async fn read_resolved_chunk(
    file: SessionFileReference,
    path: PathBuf,
    offset: u64,
    max_bytes: usize,
) -> Result<SessionFileChunk> {
    if max_bytes == 0 || max_bytes > MAX_READ_CHUNK_BYTES {
        return Err(Error::Tool(format!(
            "session file read size must be 1–{MAX_READ_CHUNK_BYTES} bytes"
        )));
    }
    if offset > file.size {
        return Err(Error::Tool("session file offset exceeds file size".into()));
    }
    let mut handle = tokio::fs::File::open(path).await?;
    handle.seek(std::io::SeekFrom::Start(offset)).await?;
    let remaining = file.size.saturating_sub(offset);
    let length = usize::try_from(remaining.min(max_bytes as u64))
        .map_err(|_| Error::Tool("session file range is unsupported".into()))?;
    let mut data = vec![0; length];
    handle.read_exact(&mut data).await?;
    let end = offset.saturating_add(length as u64);
    Ok(SessionFileChunk {
        offset,
        data,
        next_offset: (end < file.size).then_some(end),
    })
}

pub(super) async fn cleanup_stale_files(root: &Path) -> Result<()> {
    let blob_root = root.join(BLOB_DIR);
    ensure_private_dir(&blob_root).await?;
    let mut sessions = tokio::fs::read_dir(root).await?;
    while let Some(session) = sessions.next_entry().await? {
        if session.file_name() == BLOB_DIR {
            continue;
        }
        let file_type = session.file_type().await?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let mut entries = tokio::fs::read_dir(session.path()).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if file_type.is_file() && name.starts_with(".tmp") {
                tokio::fs::remove_file(entry.path()).await?;
                continue;
            }
            let staging_id = name
                .strip_prefix('.')
                .and_then(|name| name.strip_suffix("-partial"));
            if file_type.is_dir()
                && !file_type.is_symlink()
                && staging_id.is_some_and(|id| Uuid::parse_str(id).is_ok())
            {
                tokio::fs::remove_dir_all(entry.path()).await?;
            }
        }
    }
    let mut blobs = tokio::fs::read_dir(&blob_root).await?;
    while let Some(blob) = blobs.next_entry().await? {
        let file_type = blob.file_type().await?;
        let Some(name) = blob.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if file_type.is_file() && name.starts_with(".tmp") {
            tokio::fs::remove_file(blob.path()).await?;
        }
    }
    gc_unreferenced_blobs(root).await
}

pub(super) async fn list_completed(
    session_dir: &Path,
    blob_root: &Path,
) -> Result<Vec<StoredSessionFile>> {
    match tokio::fs::symlink_metadata(session_dir).await {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(Error::Tool("session file path is not a directory".into()));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    }
    let mut records = Vec::new();
    let mut entries = tokio::fs::read_dir(session_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let file_type = entry.file_type().await?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let id = entry.file_name();
        let Some(id) = id.to_str() else {
            continue;
        };
        if validate_file_id(id).is_err() {
            continue;
        }
        let record = load_metadata(&entry.path().join(METADATA_FILE)).await?;
        validate_stored_file(&record)?;
        if record.file.id != id {
            return Err(Error::Tool(
                "session file directory and metadata IDs differ".into(),
            ));
        }
        let path = blob_root.join(&record.content_hash);
        validate_content_blob_metadata(&path, record.file.size).await?;
        records.push(record);
    }
    records.sort_by(|left, right| {
        left.file
            .name
            .cmp(&right.file.name)
            .then(left.file.id.cmp(&right.file.id))
    });
    Ok(records)
}

pub(super) async fn gc_unreferenced_blobs(root: &Path) -> Result<()> {
    let blob_root = root.join(BLOB_DIR);
    let mut referenced = BTreeMap::new();
    let mut sessions = tokio::fs::read_dir(root).await?;
    while let Some(session) = sessions.next_entry().await? {
        if session.file_name() == BLOB_DIR {
            continue;
        }
        let file_type = session.file_type().await?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        for record in list_completed(&session.path(), &blob_root).await? {
            referenced.insert(record.content_hash, ());
        }
    }

    let mut blobs = tokio::fs::read_dir(&blob_root).await?;
    while let Some(blob) = blobs.next_entry().await? {
        let file_type = blob.file_type().await?;
        if file_type.is_symlink() {
            return Err(Error::Tool("session blob path is a symbolic link".into()));
        }
        if !file_type.is_file() {
            return Err(Error::Tool(
                "session blob path is not a regular file".into(),
            ));
        }
        let file_name = blob.file_name();
        let name = file_name
            .to_str()
            .ok_or_else(|| Error::Tool("session blob name is not valid UTF-8".into()))?;
        if name.starts_with(".tmp") {
            tokio::fs::remove_file(blob.path()).await?;
            continue;
        }
        validate_content_hash(name)?;
        if !referenced.contains_key(name) {
            tokio::fs::remove_file(blob.path()).await?;
        }
    }
    Ok(())
}

pub(super) async fn load_metadata(path: &Path) -> Result<StoredSessionFile> {
    require_regular_file(path).await?;
    let bytes = tokio::fs::read(path).await?;
    if bytes.len() > 4 * 1024 {
        return Err(Error::Tool(
            "session file metadata exceeds size limit".into(),
        ));
    }
    serde_json::from_slice(&bytes).map_err(Into::into)
}

pub(super) async fn load_attachment_workspace(path: &Path) -> Result<StoredAttachmentWorkspace> {
    require_regular_file(path).await?;
    let bytes = tokio::fs::read(path).await?;
    if bytes.len() > 4 * 1024 {
        return Err(Error::Tool(
            "attachment workspace metadata exceeds size limit".into(),
        ));
    }
    serde_json::from_slice(&bytes).map_err(Into::into)
}

pub(super) async fn load_optional_attachment_workspace(
    directory: &Path,
) -> Result<Option<StoredAttachmentWorkspace>> {
    let path = directory.join(ATTACHMENT_WORKSPACE_FILE);
    match load_attachment_workspace(&path).await {
        Ok(workspace) => Ok(Some(workspace)),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) async fn save_attachment_workspace(
    directory: &Path,
    workspace: &StoredAttachmentWorkspace,
) -> Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut temporary, workspace)?;
    temporary.as_file().sync_all()?;
    let destination = directory.join(ATTACHMENT_WORKSPACE_FILE);
    temporary
        .persist_noclobber(&destination)
        .map_err(|error| error.error)?;
    set_private_file(&destination).await?;
    sync_directory(directory)
}

pub(super) async fn remove_staged_attachments(
    workspace: &StoredAttachmentWorkspace,
    storage_key: &str,
) -> Result<()> {
    let workspace = workspace.clone();
    let staged = PathBuf::from(".mobius")
        .join("attachments")
        .join(storage_key);
    tokio::task::spawn_blocking(move || {
        let directory = match Dir::open_ambient_dir(&workspace.path, ambient_authority()) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::from(error)),
        };
        if !workspace.identity.matches(&directory.dir_metadata()?)? {
            return Ok(());
        }
        match directory.remove_dir_all(staged) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    })
    .await
    .map_err(|error| Error::Tool(format!("attachment cleanup task failed: {error}")))?
}

pub(super) async fn save_metadata(directory: &Path, file: &StoredSessionFile) -> Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut temporary, file)?;
    temporary.as_file().sync_all()?;
    let destination = directory.join(METADATA_FILE);
    temporary
        .persist_noclobber(&destination)
        .map_err(|error| error.error)?;
    set_private_file(&destination).await?;
    sync_directory(directory)
}

fn sync_directory(directory: &Path) -> Result<()> {
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

fn validate_reference(file: &SessionFileReference) -> Result<()> {
    validate_file_id(&file.id)?;
    validate_name(&file.name)?;
    validate_media_type(&file.media_type)?;
    if !(1..=MAX_FILE_BYTES).contains(&file.size) {
        return Err(Error::Tool(
            "session file metadata has an invalid size".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_stored_file(file: &StoredSessionFile) -> Result<()> {
    validate_reference(&file.file)?;
    validate_content_hash(&file.content_hash)
}

pub(super) fn validate_content_hash(content_hash: &str) -> Result<()> {
    if content_hash.len() != 64
        || !content_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(Error::Tool("session file content hash is invalid".into()));
    }
    Ok(())
}

pub(crate) fn session_storage_key(session_id: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(session_id.as_bytes()))
}

#[cfg(test)]
tokio::task_local! {
    pub(super) static HASH_FILE_CALLS: std::cell::Cell<usize>;
}

pub(super) async fn hash_file(path: &Path) -> Result<String> {
    #[cfg(test)]
    let _ = HASH_FILE_CALLS.try_with(|calls| calls.set(calls.get() + 1));
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_hash(&hasher.finalize()))
}

pub(super) async fn validate_content_blob(
    path: &Path,
    content_hash: &str,
    size: u64,
    validated_blobs: &StdMutex<BTreeMap<String, BlobValidationStamp>>,
) -> Result<()> {
    let metadata = validate_content_blob_metadata(path, size).await?;
    let stamp = metadata
        .modified()
        .ok()
        .map(|modified| BlobValidationStamp {
            size: metadata.len(),
            modified,
        });
    let cached = stamp.is_some_and(|stamp| {
        validated_blobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(content_hash)
            == Some(&stamp)
    });
    if cached {
        return Ok(());
    }
    if hash_file(path).await? != content_hash {
        return Err(Error::Tool(
            "session file content hash does not match metadata".into(),
        ));
    }
    if let Some(stamp) = stamp {
        remember_validated_blob(validated_blobs, content_hash, stamp);
    }
    Ok(())
}

pub(super) fn remember_validated_blob(
    validated_blobs: &StdMutex<BTreeMap<String, BlobValidationStamp>>,
    content_hash: &str,
    stamp: BlobValidationStamp,
) {
    let mut validated_blobs = validated_blobs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if validated_blobs.len() >= MAX_VALIDATED_BLOBS && !validated_blobs.contains_key(content_hash) {
        validated_blobs.pop_first();
    }
    validated_blobs.insert(content_hash.into(), stamp);
}

async fn validate_content_blob_metadata(path: &Path, size: u64) -> Result<std::fs::Metadata> {
    let metadata = require_regular_file(path).await?;
    if metadata.len() != size {
        return Err(Error::Tool(
            "session file size does not match metadata".into(),
        ));
    }
    Ok(metadata)
}

fn hex_hash(hash: &[u8]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn validate_session_id(id: &str) -> Result<()> {
    if id.trim().is_empty() || id.len() > MAX_SESSION_ID_BYTES {
        return Err(Error::Tool(format!(
            "session ID must be 1–{MAX_SESSION_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

pub(super) fn validate_file_id(id: &str) -> Result<()> {
    Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| Error::Tool("session file ID must be a UUID".into()))
}

pub(super) fn validate_name(name: &str) -> Result<()> {
    let path = Path::new(name);
    let mut components = path.components();
    let one_normal =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if !one_normal
        || name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
        || name.len() > 255
    {
        return Err(Error::Tool(
            "session file name must be one safe 1–255 byte filename".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_media_type(media_type: &str) -> Result<()> {
    let Some((kind, subtype)) = media_type.split_once('/') else {
        return Err(Error::Tool(
            "session file media type must be type/subtype".into(),
        ));
    };
    let token = |value: &str| {
        !value.is_empty()
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                    )
            })
    };
    if media_type.len() > 127 || !token(kind) || !token(subtype) {
        return Err(Error::Tool("session file media type is invalid".into()));
    }
    Ok(())
}

pub(super) async fn require_directory(path: &Path) -> Result<()> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(Error::Tool(
            "session file path is not a regular directory".into(),
        ))
    }
}

async fn require_regular_file(path: &Path) -> Result<std::fs::Metadata> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
        Ok(metadata)
    } else {
        Err(Error::Tool(
            "session file path is not a regular file".into(),
        ))
    }
}

pub(super) async fn create_private_dir(path: &Path) -> Result<()> {
    tokio::fs::create_dir(path).await?;
    set_private_dir(path).await
}

pub(super) async fn ensure_private_dir(path: &Path) -> Result<()> {
    tokio::fs::create_dir_all(path).await?;
    require_directory(path).await?;
    set_private_dir(path).await
}

#[cfg(unix)]
async fn set_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_private_dir(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(super) async fn set_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
pub(super) async fn set_private_file(_path: &Path) -> Result<()> {
    Ok(())
}
