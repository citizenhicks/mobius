use mobius::middleware::artifacts::media_type;
use std::collections::VecDeque;
use std::path::PathBuf;

use image::ColorType;
use image::ImageEncoder;
use image::codecs::png::PngEncoder;
use mobius::protocol::{SessionFileLimits, SessionFileReference};
use mobius_gateway::wire::ClientMessage;
use mobius_gateway::wire::ServerMessage;
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;
use tokio::sync::oneshot;
use uuid::Uuid;

const CLIPBOARD_PREPARATION_BUSY: &str = "clipboard preparation is already in progress";

static CLIPBOARD_PREPARATION_GATE: Semaphore = Semaphore::const_new(1);

pub(super) type ClipboardPreparation = oneshot::Receiver<Result<Vec<UploadCandidate>, String>>;

pub(super) fn prepare_clipboard(
    existing: &[SessionFileReference],
    limits: SessionFileLimits,
) -> Result<ClipboardPreparation, String> {
    let count = existing.len();
    spawn_preparation(move || read_clipboard(count, &limits))
}

fn spawn_preparation(
    prepare: impl FnOnce() -> Result<Vec<UploadCandidate>, String> + Send + 'static,
) -> Result<ClipboardPreparation, String> {
    let permit = CLIPBOARD_PREPARATION_GATE
        .try_acquire()
        .map_err(|_| CLIPBOARD_PREPARATION_BUSY.to_string())?;
    let (sender, receiver) = oneshot::channel();
    // ponytail: native clipboard calls are not cancellable; one detached worker is enough.
    std::thread::Builder::new()
        .name("mobius-clipboard-preparation".into())
        .spawn(move || {
            let _permit = permit;
            let _ = sender.send(prepare());
        })
        .map_err(|error| format!("could not start clipboard preparation: {error}"))?;
    Ok(receiver)
}

fn read_clipboard(
    existing_count: usize,
    limits: &SessionFileLimits,
) -> Result<Vec<UploadCandidate>, String> {
    let remaining = limits
        .max_attachment_references
        .saturating_sub(existing_count);
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
        return file_candidates(files, remaining, limits);
    }

    let image = clipboard
        .get_image()
        .map_err(|error| format!("clipboard has no files or image: {error}"))?;
    Ok(vec![bitmap_candidate(
        image.width,
        image.height,
        image.bytes.as_ref(),
    )?])
}

fn file_candidates(
    paths: Vec<PathBuf>,
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
    paths.into_iter().map(UploadCandidate::from_file).collect()
}

fn bitmap_candidate(width: usize, height: usize, rgba: &[u8]) -> Result<UploadCandidate, String> {
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
    UploadCandidate::from_bytes("clipboard.png", "image/png", png)
}

pub(super) struct UploadCandidate {
    name: String,
    size: u64,
    media_type: String,
    source: UploadSource,
}

impl UploadCandidate {
    fn from_file(path: PathBuf) -> Result<Self, String> {
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect `{}`: {error}", path.display()))?;
        if !metadata.file_type().is_file() {
            return Err(format!("`{}` is not a regular file", path.display()));
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("`{}` has a non-UTF-8 filename", path.display()))?
            .to_string();
        let file = std::fs::File::open(&path)
            .map_err(|error| format!("cannot open `{}`: {error}", path.display()))?;
        let opened = file
            .metadata()
            .map_err(|error| format!("cannot inspect `{}`: {error}", path.display()))?;
        if !opened.is_file() || opened.len() != metadata.len() {
            return Err(format!("`{}` changed while being attached", path.display()));
        }
        let media_type = media_type(&name).to_string();
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

    fn from_bytes(name: &str, media_type: &str, bytes: Vec<u8>) -> Result<Self, String> {
        let size = u64::try_from(bytes.len()).map_err(|_| "attachment is too large".to_string())?;
        Ok(Self {
            name: name.into(),
            size,
            media_type: media_type.into(),
            source: UploadSource::Bytes { bytes, offset: 0 },
        })
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
    ) -> Option<Result<UploadAdvance, String>> {
        let expected_request_id = self.current.as_ref()?.phase.request_id();
        if let ServerMessage::Rejected {
            request_id,
            message,
            ..
        } = message
            && request_id == expected_request_id
        {
            let name = &self.current.as_ref()?.candidate.name;
            let error = format!("could not attach `{name}`: {message}");
            self.abort();
            return Some(Err(error));
        }

        let response_request_id = match message {
            ServerMessage::SessionFileUploadReady { request_id, .. }
            | ServerMessage::SessionFileUploadChunkAccepted { request_id, .. }
            | ServerMessage::SessionFileUploadCompleted { request_id, .. } => request_id,
            _ => return None,
        };
        if response_request_id != expected_request_id {
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
                .next_transfer(session_id, upload_id, max_chunk_bytes, 0)
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
                .next_transfer(session_id, upload_id, max_chunk_bytes, next_offset)
                .await
                .map(|message| UploadAdvance {
                    attachment: None,
                    message: Some(message),
                }),
            Response::Completed(file) => {
                self.current = None;
                Ok(UploadAdvance {
                    attachment: Some(file),
                    message: self.begin_next(session_id),
                })
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
    ) -> Result<ClientMessage, String> {
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
        max_file_bytes: 250 * 1024 * 1024,
        max_session_files: 128,
        max_session_bytes: 250 * 1024 * 1024,
        max_upload_chunk_bytes: 256 * 1024,
    };

    #[test]
    fn bitmap_is_encoded_as_png() {
        let candidate = bitmap_candidate(1, 1, &[1, 2, 3, 255]).expect("PNG candidate");

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
    fn local_files_must_be_regular_and_fit_the_attachment_count() {
        let directory = tempfile::tempdir().expect("tempdir");
        assert!(
            file_candidates(
                vec![directory.path().to_path_buf()],
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
                one_reference.max_attachment_references,
                &one_reference,
            )
            .is_err()
        );
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

        let error = match spawn_preparation(|| Ok(Vec::new())) {
            Err(error) => error,
            Ok(_) => panic!("second preparation unexpectedly started"),
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
        let first = UploadCandidate::from_bytes("one.txt", "text/plain", b"abc".to_vec())
            .expect("first candidate");
        let second = UploadCandidate::from_bytes("two.txt", "text/plain", b"d".to_vec())
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
            UploadCandidate::from_bytes("one.txt", "text/plain", b"a".to_vec()).expect("candidate");
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
            )
            .await
            .expect("correlated response");

        assert!(result.is_err());
        assert!(!uploads.is_active());
    }

    #[tokio::test]
    async fn upload_machine_aborts_on_an_unexpected_acknowledged_offset() {
        let candidate = UploadCandidate::from_bytes("one.txt", "text/plain", b"abc".to_vec())
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
