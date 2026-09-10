use std::io::Cursor;

use image::{AnimationDecoder, ImageDecoder, ImageFormat, ImageReader};

use super::{SessionFileOrigin, SessionFileStore};
use crate::protocol::{ImageDetail, ImageReference, SessionFileReference};
use crate::{Error, Result};

const MAX_PIXELS: u64 = 40_000_000;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;

impl SessionFileStore {
    /// Reopens an owned image and retains its bytes independently of an upload.
    pub async fn inspect_image(
        &self,
        session_id: &str,
        file: &SessionFileReference,
        detail: ImageDetail,
    ) -> Result<ImageReference> {
        let bytes = self.read_file(session_id, file).await?;
        let (media_type, width, height) =
            tokio::task::spawn_blocking(move || validate_image(&bytes))
                .await
                .map_err(|error| Error::Tool(format!("image decoder failed: {error}")))??;
        if file.media_type != media_type {
            return Err(Error::Tool(
                "stored file media type does not match the image".into(),
            ));
        }
        let file = self
            .retain_reference(session_id, session_id, file, SessionFileOrigin::Observation)
            .await?;
        Ok(ImageReference {
            file,
            width,
            height,
            detail,
        })
    }

    /// Publishes owned stored bytes without copying or re-encoding them.
    pub async fn publish_reference(
        &self,
        session_id: &str,
        file: &SessionFileReference,
    ) -> Result<SessionFileReference> {
        self.retain_reference(session_id, session_id, file, SessionFileOrigin::Artifact)
            .await
    }

    /// Grants a trusted fork access to an exact parent file, preserving its ID.
    pub async fn grant_file(
        &self,
        parent: &str,
        child: &str,
        file: &SessionFileReference,
    ) -> Result<SessionFileReference> {
        self.retain_reference(parent, child, file, SessionFileOrigin::Observation)
            .await
    }

    async fn retain_reference(
        &self,
        source: &str,
        target: &str,
        file: &SessionFileReference,
        origin: SessionFileOrigin,
    ) -> Result<SessionFileReference> {
        super::validate_session_id(target)?;
        self.ensure_initialized().await?;
        let _commit = self.commits.lock().await;
        let (mut record, _) = self.resolve(source, &file.id).await?;
        if record.file != *file {
            return Err(Error::Tool("stored reference metadata mismatch".into()));
        }
        if source == target && record.origin == origin {
            return Ok(record.file);
        }
        if source == target {
            record.file.id = uuid::Uuid::new_v4().to_string();
        }
        let session_dir = self.session_dir(target);
        super::ensure_private_dir(&session_dir).await?;
        let directory = session_dir.join(&record.file.id);
        if tokio::fs::symlink_metadata(&directory).await.is_ok() {
            let (existing, _) = self.resolve(target, &record.file.id).await?;
            if existing.file == record.file && existing.content_hash == record.content_hash {
                return Ok(existing.file);
            }
            return Err(Error::Tool(
                "fork file ID conflicts with an existing file".into(),
            ));
        }
        let completed = super::list_completed(&session_dir, &self.blob_dir()).await?;
        let _reservation = self.reserve(target, file.size, &completed)?;
        record.origin = origin;
        let staging = session_dir.join(format!(".{}-partial", record.file.id));
        super::create_private_dir(&staging).await?;
        super::save_metadata(&staging, &record).await?;
        tokio::fs::rename(&staging, directory).await?;
        Ok(record.file)
    }

    /// Validates and records an immutable image without publishing an artifact.
    pub async fn ingest_image(
        &self,
        session_id: &str,
        name: String,
        bytes: Vec<u8>,
        detail: ImageDetail,
    ) -> Result<ImageReference> {
        let (bytes, media_type, width, height) = tokio::task::spawn_blocking(move || {
            let (media_type, width, height) = validate_image(&bytes)?;
            Ok::<_, Error>((bytes, media_type, width, height))
        })
        .await
        .map_err(|error| Error::Tool(format!("image decoder failed: {error}")))??;
        Ok(ImageReference {
            file: self
                .publish_bytes(
                    session_id,
                    name,
                    media_type.into(),
                    &bytes,
                    SessionFileOrigin::Observation,
                )
                .await?,
            width,
            height,
            detail,
        })
    }

    /// Reads an exact file reference authorized by the owning session.
    pub async fn read_file(
        &self,
        session_id: &str,
        file: &SessionFileReference,
    ) -> Result<Vec<u8>> {
        let (record, path) = self.resolve(session_id, &file.id).await?;
        if record.file != *file {
            return Err(Error::Tool(
                "session file metadata does not match the stored file".into(),
            ));
        }
        Ok(tokio::fs::read(path).await?)
    }

    /// Resolves an opaque file ID inside the authorized session.
    pub async fn file_reference(
        &self,
        session_id: &str,
        file_id: &str,
    ) -> Result<SessionFileReference> {
        Ok(self.resolve(session_id, file_id).await?.0.file)
    }
}

/// Retains referenced files before a trusted fork; text-only forks need no file store.
pub async fn grant_context(
    files: Option<&SessionFileStore>,
    parent: &str,
    child: &str,
    input: &[serde_json::Value],
) -> Result<()> {
    for part in input
        .iter()
        .filter_map(crate::protocol::content_parts)
        .flatten()
    {
        let file = match part.get("type").and_then(serde_json::Value::as_str) {
            Some("input_image") => part.get("image").and_then(|image| image.get("file")),
            Some("file") => part.get("file"),
            _ => continue,
        }
        .ok_or_else(|| Error::Config("forked observation requires a file reference".into()))?;
        let files = files
            .ok_or_else(|| Error::Config("forking media requires session file storage".into()))?;
        files
            .grant_file(parent, child, &serde_json::from_value(file.clone())?)
            .await?;
    }
    Ok(())
}

fn validate_image(bytes: &[u8]) -> Result<(&'static str, u32, u32)> {
    let format = image::guess_format(bytes).map_err(image_error)?;
    let media_type = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::WebP => "image/webp",
        ImageFormat::Gif => "image/gif",
        _ => return Err(Error::Tool("image must be PNG, JPEG, WebP, or GIF".into())),
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder().map_err(image_error)?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(Error::Tool("image exceeds the decoded pixel limit".into()));
    }
    decoder.set_limits(limits.clone()).map_err(image_error)?;
    image::DynamicImage::from_decoder(decoder).map_err(image_error)?;
    match format {
        ImageFormat::Png => {
            if image::codecs::png::PngDecoder::new(Cursor::new(bytes))
                .map_err(image_error)?
                .is_apng()
                .map_err(image_error)?
            {
                return Err(Error::Tool("animated PNG images are not supported".into()));
            }
        }
        ImageFormat::Gif => {
            let mut decoder =
                image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(image_error)?;
            decoder.set_limits(limits).map_err(image_error)?;
            let mut frames = decoder.into_frames();
            frames.next().transpose().map_err(image_error)?;
            if frames.next().transpose().map_err(image_error)?.is_some() {
                return Err(Error::Tool("animated GIF images are not supported".into()));
            }
        }
        ImageFormat::WebP => {
            let decoder =
                image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).map_err(image_error)?;
            if decoder.has_animation() {
                return Err(Error::Tool("animated WebP images are not supported".into()));
            }
        }
        _ => {}
    }
    Ok((media_type, width, height))
}

fn image_error(error: image::ImageError) -> Error {
    Error::Tool(format!("invalid or unsupported image: {error}"))
}
