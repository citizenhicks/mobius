use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;

use image::ColorType;
use image::ImageEncoder;
use image::codecs::png::PngEncoder;
use mobius::protocol::{SessionFileLimits, SessionFileReference};
use mobius_gateway::wire::ClientMessage;
use mobius_gateway::wire::ServerMessage;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

const MAX_SAFE_ATTACHMENT_REFERENCES: usize = 16;
const MAX_SAFE_FILE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_SAFE_SESSION_FILES: usize = 128;
const MAX_SAFE_SESSION_BYTES: u64 = 250 * 1024 * 1024;
const MAX_SAFE_UPLOAD_CHUNK_BYTES: usize = 256 * 1024;

fn client_limits(limits: &SessionFileLimits) -> SessionFileLimits {
    SessionFileLimits {
        max_attachment_references: limits
            .max_attachment_references
            .min(MAX_SAFE_ATTACHMENT_REFERENCES),
        max_file_bytes: limits.max_file_bytes.min(MAX_SAFE_FILE_BYTES),
        max_session_files: limits.max_session_files.min(MAX_SAFE_SESSION_FILES),
        max_session_bytes: limits.max_session_bytes.min(MAX_SAFE_SESSION_BYTES),
        max_upload_chunk_bytes: limits
            .max_upload_chunk_bytes
            .min(MAX_SAFE_UPLOAD_CHUNK_BYTES),
    }
}

pub(super) fn read_clipboard(
    existing: &[SessionFileReference],
    limits: &SessionFileLimits,
) -> Result<Vec<UploadCandidate>, String> {
    let limits = client_limits(limits);
    let remaining = limits
        .max_attachment_references
        .saturating_sub(existing.len());
    if remaining == 0 {
        return Err(format!(
            "a message cannot contain more than {} attachments",
            limits.max_attachment_references
        ));
    }

    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("clipboard unavailable: {error}"))?;
    let files = clipboard.get().file_list().unwrap_or_default();
    if !files.is_empty() {
        return file_candidates(files, existing, remaining, &limits);
    }

    let image = clipboard
        .get_image()
        .map_err(|error| format!("clipboard has no files or image: {error}"))?;
    let candidate = bitmap_candidate(image.width, image.height, image.bytes.as_ref(), &limits)?;
    validate_total_size(existing, std::slice::from_ref(&candidate), &limits)?;
    Ok(vec![candidate])
}

fn file_candidates(
    paths: Vec<PathBuf>,
    existing: &[SessionFileReference],
    remaining: usize,
    limits: &SessionFileLimits,
) -> Result<Vec<UploadCandidate>, String> {
    if paths.len() > remaining {
        return Err(format!(
            "pasting {} files would exceed the {}-attachment message limit",
            paths.len(),
            limits.max_attachment_references
        ));
    }
    let candidates = paths
        .into_iter()
        .map(|path| UploadCandidate::from_file(path, limits))
        .collect::<Result<Vec<_>, _>>()?;
    validate_total_size(existing, &candidates, limits)?;
    Ok(candidates)
}

fn bitmap_candidate(
    width: usize,
    height: usize,
    rgba: &[u8],
    limits: &SessionFileLimits,
) -> Result<UploadCandidate, String> {
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "clipboard image dimensions are too large".to_string())?;
    if width == 0 || height == 0 || rgba.len() != expected {
        return Err("clipboard image has invalid RGBA data".into());
    }
    let width = u32::try_from(width).map_err(|_| "clipboard image is too wide".to_string())?;
    let height = u32::try_from(height).map_err(|_| "clipboard image is too tall".to_string())?;
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(rgba, width, height, ColorType::Rgba8.into())
        .map_err(|error| format!("could not encode clipboard image: {error}"))?;
    UploadCandidate::from_bytes("clipboard.png", "image/png", png, limits)
}

fn validate_total_size(
    existing: &[SessionFileReference],
    candidates: &[UploadCandidate],
    limits: &SessionFileLimits,
) -> Result<(), String> {
    let existing = existing.iter().try_fold(0_u64, |total, file| {
        total
            .checked_add(file.size)
            .ok_or_else(|| "attachment sizes overflowed".to_string())
    })?;
    let total = candidates.iter().try_fold(existing, |total, candidate| {
        total
            .checked_add(candidate.size)
            .ok_or_else(|| "attachment sizes overflowed".to_string())
    })?;
    if total > limits.max_session_bytes {
        return Err(format!(
            "pasted attachments exceed the {}-byte session limit",
            limits.max_session_bytes
        ));
    }
    Ok(())
}

pub(super) struct UploadCandidate {
    name: String,
    size: u64,
    media_type: String,
    source: UploadSource,
}

impl UploadCandidate {
    fn from_file(path: PathBuf, limits: &SessionFileLimits) -> Result<Self, String> {
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect `{}`: {error}", path.display()))?;
        if !metadata.file_type().is_file() {
            return Err(format!("`{}` is not a regular file", path.display()));
        }
        validate_size(metadata.len(), limits)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("`{}` has a non-UTF-8 filename", path.display()))?
            .to_string();
        validate_name(&name)?;
        let file = std::fs::File::open(&path)
            .map_err(|error| format!("cannot open `{}`: {error}", path.display()))?;
        let opened = file
            .metadata()
            .map_err(|error| format!("cannot inspect `{}`: {error}", path.display()))?;
        if !opened.is_file() || opened.len() != metadata.len() {
            return Err(format!("`{}` changed while being attached", path.display()));
        }
        let media_type = media_type(&name).to_string();
        validate_media_type(&media_type)?;
        Ok(Self {
            name,
            size: metadata.len(),
            media_type,
            source: UploadSource::File {
                file: tokio::fs::File::from_std(file),
                offset: 0,
            },
        })
    }

    fn from_bytes(
        name: &str,
        media_type: &str,
        bytes: Vec<u8>,
        limits: &SessionFileLimits,
    ) -> Result<Self, String> {
        validate_name(name)?;
        validate_media_type(media_type)?;
        let size = u64::try_from(bytes.len()).map_err(|_| "attachment is too large".to_string())?;
        validate_size(size, limits)?;
        Ok(Self {
            name: name.into(),
            size,
            media_type: media_type.into(),
            source: UploadSource::Bytes { bytes, offset: 0 },
        })
    }
}

fn validate_size(size: u64, limits: &SessionFileLimits) -> Result<(), String> {
    if !(1..=limits.max_file_bytes).contains(&size) {
        return Err(format!(
            "attachment size must be 1–{} bytes",
            limits.max_file_bytes
        ));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
        || Path::new(name).file_name().and_then(|value| value.to_str()) != Some(name)
    {
        return Err("attachment name must be one safe 1–255 byte filename".into());
    }
    Ok(())
}

fn validate_media_type(media_type: &str) -> Result<(), String> {
    let Some((kind, subtype)) = media_type.split_once('/') else {
        return Err("attachment media type must be type/subtype".into());
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
        return Err("attachment media type is invalid".into());
    }
    Ok(())
}

fn media_type(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("csv") => "text/csv",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

enum UploadSource {
    File { file: tokio::fs::File, offset: u64 },
    Bytes { bytes: Vec<u8>, offset: usize },
}

impl UploadSource {
    fn offset(&self) -> u64 {
        match self {
            Self::File { offset, .. } => *offset,
            Self::Bytes { offset, .. } => u64::try_from(*offset).unwrap_or(u64::MAX),
        }
    }

    async fn read(&mut self, length: usize) -> Result<Vec<u8>, String> {
        match self {
            Self::File { file, offset } => {
                let mut bytes = vec![0; length];
                file.read_exact(&mut bytes)
                    .await
                    .map_err(|error| format!("could not read attachment: {error}"))?;
                *offset = offset
                    .checked_add(u64::try_from(length).unwrap_or(u64::MAX))
                    .ok_or_else(|| "attachment offset overflowed".to_string())?;
                Ok(bytes)
            }
            Self::Bytes { bytes, offset } => {
                let end = offset
                    .checked_add(length)
                    .filter(|end| *end <= bytes.len())
                    .ok_or_else(|| "clipboard image ended unexpectedly".to_string())?;
                let chunk = bytes[*offset..end].to_vec();
                *offset = end;
                Ok(chunk)
            }
        }
    }
}

enum UploadPhase {
    Begin {
        request_id: String,
    },
    Chunk {
        request_id: String,
        upload_id: String,
        max_chunk_bytes: usize,
    },
    Finish {
        request_id: String,
    },
}

impl UploadPhase {
    fn request_id(&self) -> &str {
        match self {
            Self::Begin { request_id }
            | Self::Chunk { request_id, .. }
            | Self::Finish { request_id } => request_id,
        }
    }
}

struct ActiveUpload {
    candidate: UploadCandidate,
    phase: UploadPhase,
}

#[derive(Default)]
pub(super) struct ClipboardUploads {
    queued: VecDeque<UploadCandidate>,
    current: Option<ActiveUpload>,
}

pub(super) struct UploadAdvance {
    pub(super) attachment: Option<SessionFileReference>,
    pub(super) message: Option<ClientMessage>,
}

impl ClipboardUploads {
    pub(super) fn is_active(&self) -> bool {
        self.current.is_some()
    }

    pub(super) fn start(
        &mut self,
        candidates: Vec<UploadCandidate>,
        session_id: &str,
    ) -> Result<ClientMessage, String> {
        if self.is_active() {
            return Err("an attachment upload is already in progress".into());
        }
        if candidates.is_empty() {
            return Err("clipboard has no files or image".into());
        }
        self.queued = candidates.into();
        self.begin_next(session_id)
            .ok_or_else(|| "clipboard has no files or image".into())
    }

    pub(super) fn abort(&mut self) {
        self.current = None;
        self.queued.clear();
    }

    pub(super) async fn handle(
        &mut self,
        message: &ServerMessage,
        session_id: &str,
        limits: &SessionFileLimits,
    ) -> Option<Result<UploadAdvance, String>> {
        let expected_request_id = self.current.as_ref()?.phase.request_id().to_owned();
        if let ServerMessage::Rejected {
            request_id,
            message,
            ..
        } = message
            && request_id == &expected_request_id
        {
            let name = self.current.as_ref()?.candidate.name.clone();
            let message = message.clone();
            self.abort();
            return Some(Err(format!("could not attach `{name}`: {message}")));
        }

        let response_request_id = match message {
            ServerMessage::SessionFileUploadReady { request_id, .. }
            | ServerMessage::SessionFileUploadChunkAccepted { request_id, .. }
            | ServerMessage::SessionFileUploadCompleted { request_id, .. } => request_id,
            _ => return None,
        };
        if response_request_id != &expected_request_id {
            return None;
        }

        enum Response {
            Ready {
                upload_id: String,
                max_chunk_bytes: usize,
            },
            Chunk {
                upload_id: String,
                max_chunk_bytes: usize,
                next_offset: u64,
            },
            Completed(SessionFileReference),
        }

        let response = match (&self.current.as_ref()?.phase, message) {
            (
                UploadPhase::Begin { request_id },
                ServerMessage::SessionFileUploadReady {
                    request_id: actual,
                    session_id: actual_session,
                    upload_id,
                    max_chunk_bytes,
                },
            ) if actual == request_id && actual_session == session_id => Response::Ready {
                upload_id: upload_id.clone(),
                max_chunk_bytes: *max_chunk_bytes,
            },
            (
                UploadPhase::Chunk {
                    request_id,
                    upload_id,
                    max_chunk_bytes,
                },
                ServerMessage::SessionFileUploadChunkAccepted {
                    request_id: actual,
                    session_id: actual_session,
                    upload_id: actual_upload,
                    next_offset,
                },
            ) if actual == request_id
                && actual_session == session_id
                && actual_upload == upload_id =>
            {
                Response::Chunk {
                    upload_id: upload_id.clone(),
                    max_chunk_bytes: *max_chunk_bytes,
                    next_offset: *next_offset,
                }
            }
            (
                UploadPhase::Finish { request_id },
                ServerMessage::SessionFileUploadCompleted {
                    request_id: actual,
                    session_id: actual_session,
                    file,
                },
            ) if actual == request_id && actual_session == session_id => {
                Response::Completed(file.clone())
            }
            _ => {
                self.abort();
                return Some(Err(
                    "gateway returned an invalid attachment upload response".into(),
                ));
            }
        };

        let result = match response {
            Response::Ready {
                upload_id,
                max_chunk_bytes,
            } => self
                .next_transfer(session_id, upload_id, max_chunk_bytes, 0, limits)
                .await
                .map(|message| UploadAdvance {
                    attachment: None,
                    message: Some(message),
                }),
            Response::Chunk {
                upload_id,
                max_chunk_bytes,
                next_offset,
            } => self
                .next_transfer(session_id, upload_id, max_chunk_bytes, next_offset, limits)
                .await
                .map(|message| UploadAdvance {
                    attachment: None,
                    message: Some(message),
                }),
            Response::Completed(file) => {
                let candidate = &self.current.as_ref()?.candidate;
                if Uuid::parse_str(&file.id).is_err()
                    || file.name != candidate.name
                    || file.size != candidate.size
                    || file.media_type != candidate.media_type
                {
                    Err("gateway returned mismatched attachment metadata".into())
                } else {
                    self.current = None;
                    Ok(UploadAdvance {
                        attachment: Some(file),
                        message: self.begin_next(session_id),
                    })
                }
            }
        };
        if result.is_err() {
            self.abort();
        }
        Some(result)
    }

    fn begin_next(&mut self, session_id: &str) -> Option<ClientMessage> {
        let candidate = self.queued.pop_front()?;
        let request_id = Uuid::new_v4().to_string();
        let message = ClientMessage::BeginSessionFileUpload {
            request_id: request_id.clone(),
            session_id: session_id.into(),
            name: candidate.name.clone(),
            size: candidate.size,
            media_type: candidate.media_type.clone(),
        };
        self.current = Some(ActiveUpload {
            candidate,
            phase: UploadPhase::Begin { request_id },
        });
        Some(message)
    }

    async fn next_transfer(
        &mut self,
        session_id: &str,
        upload_id: String,
        max_chunk_bytes: usize,
        next_offset: u64,
        limits: &SessionFileLimits,
    ) -> Result<ClientMessage, String> {
        let limits = client_limits(limits);
        if Uuid::parse_str(&upload_id).is_err() {
            return Err("gateway returned an invalid attachment upload ID".into());
        }
        if !(1..=limits.max_upload_chunk_bytes).contains(&max_chunk_bytes) {
            return Err("gateway returned an invalid attachment upload chunk limit".into());
        }
        let current = self
            .current
            .as_mut()
            .ok_or_else(|| "attachment upload is not active".to_string())?;
        if next_offset != current.candidate.source.offset() {
            return Err("gateway returned an unexpected attachment offset".into());
        }
        if next_offset == current.candidate.size {
            let request_id = Uuid::new_v4().to_string();
            current.phase = UploadPhase::Finish {
                request_id: request_id.clone(),
            };
            return Ok(ClientMessage::FinishSessionFileUpload {
                request_id,
                session_id: session_id.into(),
                upload_id,
            });
        }
        if next_offset > current.candidate.size {
            return Err("gateway advanced beyond the attachment size".into());
        }
        let remaining = current.candidate.size - next_offset;
        let length = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(max_chunk_bytes);
        let data = current.candidate.source.read(length).await?;
        let request_id = Uuid::new_v4().to_string();
        current.phase = UploadPhase::Chunk {
            request_id: request_id.clone(),
            upload_id: upload_id.clone(),
            max_chunk_bytes,
        };
        Ok(ClientMessage::UploadSessionFileChunk {
            request_id,
            session_id: session_id.into(),
            upload_id,
            offset: next_offset,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPLOAD_ID: &str = "6752c95f-f2f6-4845-928d-93db92ee0e2a";
    const TEST_LIMITS: SessionFileLimits = SessionFileLimits {
        max_attachment_references: 16,
        max_file_bytes: 50 * 1024 * 1024,
        max_session_files: 128,
        max_session_bytes: 250 * 1024 * 1024,
        max_upload_chunk_bytes: 256 * 1024,
    };

    #[test]
    fn bitmap_is_encoded_as_png() {
        let candidate =
            bitmap_candidate(1, 1, &[1, 2, 3, 255], &TEST_LIMITS).expect("PNG candidate");

        assert_eq!(candidate.name, "clipboard.png");
        assert_eq!(candidate.media_type, "image/png");
        let UploadSource::Bytes { bytes, .. } = candidate.source else {
            panic!("bitmap bytes");
        };
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[tokio::test]
    async fn copied_files_are_validated_and_preserved() {
        let directory = tempfile::tempdir().expect("tempdir");
        let first = directory.path().join("one.png");
        let second = directory.path().join("two.pdf");
        std::fs::write(&first, b"one").expect("first file");
        std::fs::write(&second, b"two").expect("second file");

        let candidates = file_candidates(
            vec![first, second],
            &[],
            TEST_LIMITS.max_attachment_references,
            &TEST_LIMITS,
        )
        .expect("file candidates");

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].name, "one.png");
        assert_eq!(candidates[0].media_type, "image/png");
        assert_eq!(candidates[1].name, "two.pdf");
        assert_eq!(candidates[1].media_type, "application/pdf");
    }

    #[test]
    fn local_attachment_bounds_reject_unsafe_inputs() {
        assert!(validate_name("../secret").is_err());
        assert!(validate_name("bad\nname").is_err());
        assert!(validate_name(&"a".repeat(256)).is_err());
        assert!(validate_size(0, &TEST_LIMITS).is_err());
        assert!(validate_size(TEST_LIMITS.max_file_bytes + 1, &TEST_LIMITS).is_err());
        assert!(validate_media_type("image/png; charset=binary").is_err());

        let directory = tempfile::tempdir().expect("tempdir");
        assert!(
            file_candidates(
                vec![directory.path().to_path_buf()],
                &[],
                TEST_LIMITS.max_attachment_references,
                &TEST_LIMITS,
            )
            .is_err()
        );
        let one_reference = SessionFileLimits {
            max_attachment_references: 1,
            ..TEST_LIMITS
        };
        assert!(
            file_candidates(
                vec![PathBuf::new(), PathBuf::new()],
                &[],
                one_reference.max_attachment_references,
                &one_reference,
            )
            .is_err()
        );
    }

    #[test]
    fn advertised_file_and_session_sizes_are_applied() {
        let limits = SessionFileLimits {
            max_file_bytes: 2,
            max_session_bytes: 3,
            ..TEST_LIMITS
        };
        assert!(
            UploadCandidate::from_bytes(
                "large.bin",
                "application/octet-stream",
                vec![0; 3],
                &limits
            )
            .is_err()
        );
        let candidate = UploadCandidate::from_bytes(
            "small.bin",
            "application/octet-stream",
            vec![0; 2],
            &limits,
        )
        .expect("candidate");
        let existing = [SessionFileReference {
            id: Uuid::new_v4().to_string(),
            name: "existing.bin".into(),
            size: 2,
            media_type: "application/octet-stream".into(),
        }];

        assert!(validate_total_size(&existing, &[candidate], &limits).is_err());
    }

    #[test]
    fn advertised_limits_cannot_expand_client_safety_bounds() {
        let limits = client_limits(&SessionFileLimits {
            max_attachment_references: usize::MAX,
            max_file_bytes: u64::MAX,
            max_session_files: usize::MAX,
            max_session_bytes: u64::MAX,
            max_upload_chunk_bytes: usize::MAX,
        });

        assert_eq!(limits, TEST_LIMITS);
    }

    #[tokio::test]
    async fn upload_machine_correlates_chunks_and_starts_the_next_file() {
        let first =
            UploadCandidate::from_bytes("one.txt", "text/plain", b"abc".to_vec(), &TEST_LIMITS)
                .expect("first candidate");
        let second =
            UploadCandidate::from_bytes("two.txt", "text/plain", b"d".to_vec(), &TEST_LIMITS)
                .expect("second candidate");
        let mut uploads = ClipboardUploads::default();
        let begin = uploads
            .start(vec![first, second], "session")
            .expect("begin upload");
        let ClientMessage::BeginSessionFileUpload { request_id, .. } = begin else {
            panic!("begin message");
        };

        assert!(
            uploads
                .handle(
                    &ServerMessage::SessionFileUploadReady {
                        request_id: "other".into(),
                        session_id: "session".into(),
                        upload_id: UPLOAD_ID.into(),
                        max_chunk_bytes: 2,
                    },
                    "session",
                    &TEST_LIMITS,
                )
                .await
                .is_none()
        );
        let ready = uploads
            .handle(
                &ServerMessage::SessionFileUploadReady {
                    request_id,
                    session_id: "session".into(),
                    upload_id: UPLOAD_ID.into(),
                    max_chunk_bytes: 2,
                },
                "session",
                &TEST_LIMITS,
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

        let chunk = uploads
            .handle(
                &ServerMessage::SessionFileUploadChunkAccepted {
                    request_id,
                    session_id: "session".into(),
                    upload_id: UPLOAD_ID.into(),
                    next_offset: 2,
                },
                "session",
                &TEST_LIMITS,
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

        let finish = uploads
            .handle(
                &ServerMessage::SessionFileUploadChunkAccepted {
                    request_id,
                    session_id: "session".into(),
                    upload_id: UPLOAD_ID.into(),
                    next_offset: 3,
                },
                "session",
                &TEST_LIMITS,
            )
            .await
            .expect("matched final chunk")
            .expect("finish");
        let ClientMessage::FinishSessionFileUpload { request_id, .. } =
            finish.message.expect("finish message")
        else {
            panic!("finish message");
        };

        let completed = uploads
            .handle(
                &ServerMessage::SessionFileUploadCompleted {
                    request_id,
                    session_id: "session".into(),
                    file: SessionFileReference {
                        id: Uuid::new_v4().to_string(),
                        name: "one.txt".into(),
                        size: 3,
                        media_type: "text/plain".into(),
                    },
                },
                "session",
                &TEST_LIMITS,
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

    #[tokio::test]
    async fn upload_machine_aborts_a_correlated_scope_mismatch() {
        let candidate =
            UploadCandidate::from_bytes("one.txt", "text/plain", b"a".to_vec(), &TEST_LIMITS)
                .expect("candidate");
        let mut uploads = ClipboardUploads::default();
        let begin = uploads.start(vec![candidate], "session").expect("begin");
        let ClientMessage::BeginSessionFileUpload { request_id, .. } = begin else {
            panic!("begin message");
        };

        let result = uploads
            .handle(
                &ServerMessage::SessionFileUploadReady {
                    request_id,
                    session_id: "old-session".into(),
                    upload_id: UPLOAD_ID.into(),
                    max_chunk_bytes: 1,
                },
                "session",
                &TEST_LIMITS,
            )
            .await
            .expect("correlated response");

        assert!(result.is_err());
        assert!(!uploads.is_active());
    }

    #[tokio::test]
    async fn upload_machine_enforces_advertised_and_client_local_chunk_limits() {
        let cases = [
            (
                SessionFileLimits {
                    max_upload_chunk_bytes: 1,
                    ..TEST_LIMITS
                },
                2,
            ),
            (
                SessionFileLimits {
                    max_upload_chunk_bytes: MAX_SAFE_UPLOAD_CHUNK_BYTES + 1,
                    ..TEST_LIMITS
                },
                MAX_SAFE_UPLOAD_CHUNK_BYTES + 1,
            ),
        ];

        for (limits, max_chunk_bytes) in cases {
            let candidate =
                UploadCandidate::from_bytes("one.txt", "text/plain", b"a".to_vec(), &limits)
                    .expect("candidate");
            let mut uploads = ClipboardUploads::default();
            let begin = uploads.start(vec![candidate], "session").expect("begin");
            let ClientMessage::BeginSessionFileUpload { request_id, .. } = begin else {
                panic!("begin message");
            };

            let result = uploads
                .handle(
                    &ServerMessage::SessionFileUploadReady {
                        request_id,
                        session_id: "session".into(),
                        upload_id: UPLOAD_ID.into(),
                        max_chunk_bytes,
                    },
                    "session",
                    &limits,
                )
                .await
                .expect("correlated response");

            assert!(result.is_err());
            assert!(!uploads.is_active());
        }
    }

    #[tokio::test]
    async fn upload_machine_aborts_on_an_unexpected_acknowledged_offset() {
        let candidate =
            UploadCandidate::from_bytes("one.txt", "text/plain", b"abc".to_vec(), &TEST_LIMITS)
                .expect("candidate");
        let mut uploads = ClipboardUploads::default();
        let begin = uploads.start(vec![candidate], "session").expect("begin");
        let ClientMessage::BeginSessionFileUpload { request_id, .. } = begin else {
            panic!("begin message");
        };
        let chunk = uploads
            .handle(
                &ServerMessage::SessionFileUploadReady {
                    request_id,
                    session_id: "session".into(),
                    upload_id: UPLOAD_ID.into(),
                    max_chunk_bytes: 2,
                },
                "session",
                &TEST_LIMITS,
            )
            .await
            .expect("matched ready")
            .expect("chunk");
        let ClientMessage::UploadSessionFileChunk { request_id, .. } =
            chunk.message.expect("chunk message")
        else {
            panic!("chunk message");
        };

        let result = uploads
            .handle(
                &ServerMessage::SessionFileUploadChunkAccepted {
                    request_id,
                    session_id: "session".into(),
                    upload_id: UPLOAD_ID.into(),
                    next_offset: 1,
                },
                "session",
                &TEST_LIMITS,
            )
            .await
            .expect("matched chunk");
        let Err(error) = result else {
            panic!("unexpected offset was accepted");
        };

        assert!(error.contains("unexpected attachment offset"));
        assert!(!uploads.is_active());
    }
}
