use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use mobius::SessionId;
use mobius::protocol::SessionFileReference;
use mobius_gateway::wire::{ClientMessage, ServerMessage};
use mobius_gateway::{Error, Result};
use tokio::io::AsyncReadExt;

use super::files::{TransferLink, WINDOW};

#[derive(Debug)]
pub struct UploadFailure {
    pub storage_full: bool,
    pub message: String,
    pub cleanup_file_id: Option<String>,
}

impl From<Error> for UploadFailure {
    fn from(error: Error) -> Self {
        Self {
            storage_full: matches!(error, Error::Mobius(mobius::Error::StorageFull)),
            message: super::describe(&error),
            cleanup_file_id: None,
        }
    }
}

pub(super) async fn upload(
    link: &TransferLink,
    session_id: &SessionId,
    path: &Path,
    cancellation: &AtomicBool,
) -> std::result::Result<Option<SessionFileReference>, UploadFailure> {
    if cancellation.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let metadata = tokio::fs::metadata(path).await.map_err(Error::from)?;
    regular_file(&metadata)?;
    let mut input = tokio::fs::File::open(path).await.map_err(Error::from)?;
    let metadata = input.metadata().await.map_err(Error::from)?;
    regular_file(&metadata)?;
    let size = metadata.len();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Config("The file needs a valid UTF-8 name.".into()))?;
    let media_type = mime_guess::from_path(path).first_or_octet_stream();
    let response = link
        .request(|request_id| ClientMessage::BeginSessionFileUpload {
            request_id,
            session_id: session_id.share(),
            name: name.into(),
            size,
            media_type: media_type.essence_str().into(),
        })
        .await?;
    let ServerMessage::SessionFileUploadReady {
        upload_id,
        max_chunk_bytes: chunk_size,
        ..
    } = response
    else {
        return Err(unexpected().into());
    };
    let reading = Reading {
        input: &mut input,
        offset: 0,
        size,
        chunk_size,
        cancellation,
    };
    // The unfold keeps a read in progress between polls, so a slow disk never holds up the
    // chunks already on their way and a chunk leaves as soon as it is read.
    let mut written = stream::try_unfold(reading, next_chunk)
        // Boxed because the compiler cannot prove this borrowing step `Send` otherwise.
        .boxed()
        .map_ok(|(offset, data)| write_chunk(link, session_id, &upload_id, offset, data))
        .try_buffered(WINDOW);
    while written.try_next().await?.is_some() {}
    drop(written);
    if discard_if_requested(link, session_id, &upload_id, cancellation).await? {
        return Ok(None);
    }
    let after = input.metadata().await.map_err(Error::from)?;
    if after.len() != size || after.modified().ok() != metadata.modified().ok() {
        return Err(
            Error::Config("The file changed during upload; attach it again.".into()).into(),
        );
    }
    let response = link
        .request(|request_id| ClientMessage::FinishSessionFileUpload {
            request_id,
            session_id: session_id.share(),
            upload_id: upload_id.as_str().into(),
        })
        .await;
    match response {
        // Always publish a completed reference, including a late cancellation.
        // The draft retains it until the separate delete is acknowledged.
        Ok(ServerMessage::SessionFileUploadCompleted { file, .. }) => Ok(Some(file)),
        Ok(_) => Err(unexpected()),
        Err(error) => Err(error),
    }
    .map_err(|error| {
        let mut failure = UploadFailure::from(error);
        // Finish can commit before its acknowledgement is lost. Keep the known ID
        // for deletion, without treating the unconfirmed file as ready to submit.
        failure.cleanup_file_id = Some(upload_id);
        failure
    })
}

/// How far the upload has read its file.
struct Reading<'a> {
    input: &'a mut tokio::fs::File,
    offset: u64,
    size: u64,
    chunk_size: usize,
    cancellation: &'a AtomicBool,
}

/// The next chunk and where it starts, until the file ends or the upload is cancelled.
async fn next_chunk(mut reading: Reading<'_>) -> Result<Option<((u64, Vec<u8>), Reading<'_>)>> {
    let offset = reading.offset;
    if offset >= reading.size || reading.cancellation.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let length = usize::try_from(reading.size - offset).map_or(reading.chunk_size, |remaining| {
        remaining.min(reading.chunk_size)
    });
    let mut data = vec![0; length];
    reading.input.read_exact(&mut data).await?;
    reading.offset += length as u64;
    Ok(Some(((offset, data), reading)))
}

pub(super) async fn discard_if_requested(
    link: &TransferLink,
    session_id: &SessionId,
    upload_id: &str,
    cancellation: &AtomicBool,
) -> Result<bool> {
    if !cancellation.load(Ordering::Relaxed) {
        return Ok(false);
    }
    super::files::delete(link, session_id, upload_id).await?;
    Ok(true)
}

async fn write_chunk(
    link: &TransferLink,
    session_id: &SessionId,
    upload_id: &str,
    offset: u64,
    data: Vec<u8>,
) -> Result<u64> {
    let next_offset = offset + data.len() as u64;
    let response = link
        .request(|request_id| ClientMessage::UploadSessionFileChunk {
            request_id,
            session_id: session_id.share(),
            upload_id: upload_id.into(),
            offset,
            data,
        })
        .await?;
    match response {
        ServerMessage::SessionFileUploadChunkAccepted { .. } => Ok(next_offset),
        _ => Err(unexpected()),
    }
}

fn regular_file(metadata: &std::fs::Metadata) -> Result<()> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(Error::Config("Only regular files can be attached.".into()))
    }
}

fn unexpected() -> Error {
    Error::Protocol("Gateway sent an unexpected reply to an attachment upload.".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_regular_files_upload() {
        let directory = tempfile::tempdir().unwrap();
        assert!(super::regular_file(&directory.path().metadata().unwrap()).is_err());
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(super::regular_file(&file.as_file().metadata().unwrap()).is_ok());
    }
}
