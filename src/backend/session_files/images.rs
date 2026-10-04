use std::io::Cursor;
use std::sync::Arc;

use image::metadata::Orientation;
use image::{AnimationDecoder, ImageDecoder, ImageFormat, ImageReader};

use super::{SessionFileOrigin, SessionFileStore};
use crate::protocol::{ImageDetail, ImageReference, ImageRendition, SessionFileReference};
use crate::{Error, Result};

const MAX_PIXELS: u64 = 40_000_000;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;

crate::embedded_config! {
    copy;
    /// Model presentation dimensions, independent of encoded request admission.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ImagePresentation {
        /// Longest permitted image side.
        pub max_dimension: u32,
        /// Maximum number of 32 by 32 pixel patches.
        pub max_patches: u32,
        /// Try JPEG quality 85 for opaque PNGs at least this large; keep the smaller encoding.
        pub png_jpeg_threshold_bytes: u64,
    }
    defaults = include_str!("images.toml");
}

impl ImagePresentation {
    /// Whether these pixels fit without resizing.
    #[must_use]
    pub fn fits(self, width: u32, height: u32) -> bool {
        width > 0
            && height > 0
            && width.max(height) <= self.max_dimension
            && u64::from(width.div_ceil(32)) * u64::from(height.div_ceil(32))
                <= u64::from(self.max_patches)
    }

    /// Fits both limits, preserving aspect ratio to pixel rounding without enlargement.
    #[must_use]
    pub fn dimensions(self, width: u32, height: u32) -> (u32, u32) {
        if self.fits(width, height) {
            return (width, height);
        }
        // Integer search avoids rounding a patch grid back over its budget.
        let longest = width.max(height).max(1);
        let scaled = |side: u32| {
            (
                u32::try_from((u64::from(width) * u64::from(side) / u64::from(longest)).max(1))
                    .unwrap_or(width.max(1)),
                u32::try_from((u64::from(height) * u64::from(side) / u64::from(longest)).max(1))
                    .unwrap_or(height.max(1)),
            )
        };
        let (mut low, mut high) = (1, longest.min(self.max_dimension).max(1));
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            let (w, h) = scaled(mid);
            if self.fits(w, h) {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        scaled(low)
    }
}

impl SessionFileStore {
    /// Sets model presentation policy for this store.
    /// # Errors
    /// Rejects zero or decoder-exceeding dimensions and patch budgets.
    pub fn image_presentation(mut self, policy: ImagePresentation) -> Result<Self> {
        if policy.max_dimension == 0
            || policy.max_dimension > 16_384
            || policy.max_patches == 0
            || policy.png_jpeg_threshold_bytes == 0
        {
            return Err(Error::Config("invalid image presentation limits".into()));
        }
        self.image_policy = policy;
        Ok(self)
    }

    /// Resolves or creates model pixels without changing the original observation.
    /// # Errors
    /// Returns authorization, decoding, or storage errors.
    pub async fn prepare_image(
        &self,
        session_id: &str,
        image: &ImageReference,
    ) -> Result<Option<ImageRendition>> {
        let record = self.resolve_image(session_id, image).await?;
        let policy = self.image_policy;
        let key = format!(
            "{}-{}-{}-{}-v3",
            record.content_hash,
            policy.max_dimension,
            policy.max_patches,
            policy.png_jpeg_threshold_bytes
        );
        let (source_width, source_height) = oriented_dimensions(
            image.width,
            image.height,
            record.image_orientation.unwrap_or(1),
        );
        let (width, height) = policy.dimensions(source_width, source_height);
        if record.image_rendition_key.as_deref() == Some(key.as_str()) {
            return Ok(Some(ImageRendition {
                file: record.file,
                width,
                height,
            }));
        }
        if let Some(rendition) = &image.rendition
            && let Some(rendition) = self
                .resolve_rendition(session_id, rendition, &key, [width, height])
                .await?
        {
            return Ok(Some(rendition));
        }
        if policy.fits(image.width, image.height)
            && !(image.file.media_type == "image/png"
                && image.file.size >= policy.png_jpeg_threshold_bytes)
        {
            return Ok(None);
        }
        // ponytail: legacy references scan session records under one preparation lock; add a bounded
        // in-memory key-to-reference cache if repeated legacy replay scans become a bottleneck.
        let _preparation = self.renditions.lock().await;
        // Session-file records are the cache: authorization, restart recovery and cleanup share one owner.
        if let Some(record) = super::list_completed(&self.session_dir(session_id), &self.blob_dir())
            .await?
            .into_iter()
            .find(|record| {
                record.origin == SessionFileOrigin::Observation
                    && record.image_rendition_key.as_deref() == Some(key.as_str())
            })
        {
            if model_dimensions(&record) != Some((width, height)) {
                return Err(Error::Tool(
                    "invalid cached image rendition dimensions".into(),
                ));
            }
            return Ok(Some(ImageRendition {
                file: record.file,
                width,
                height,
            }));
        }
        let permit = Arc::clone(&self.image_work)
            .acquire_owned()
            .await
            .map_err(|_| Error::Tool("image preparation unavailable".into()))?;
        let bytes = self.read_file(session_id, &image.file).await?;
        let prepared = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            prepare_pixels(bytes, width, height, policy)
        })
        .await
        .map_err(|error| Error::Tool(format!("image preparation failed: {error}")))??;
        let Some((bytes, media_type)) = prepared else {
            self.remember_image_preparation(
                session_id,
                &image.file,
                [image.width, image.height],
                record.image_orientation.unwrap_or(1),
                Some(key),
            )
            .await?;
            return Ok(Some(ImageRendition {
                file: record.file,
                width,
                height,
            }));
        };
        let size = u64::try_from(bytes.len())
            .map_err(|_| Error::Tool("image rendition size overflow".into()))?;
        let mut pending = self
            .begin(
                session_id,
                "model-rendition".into(),
                size,
                media_type.into(),
                SessionFileOrigin::Observation,
                false,
            )
            .await?;
        pending.record.image_rendition_key = Some(key);
        pending.record.image_dimensions = Some([width, height]);
        pending.record.image_orientation = Some(1);
        let file = pending.complete_bytes(&bytes).await?;
        Ok(Some(ImageRendition {
            file,
            width,
            height,
        }))
    }

    async fn resolve_image(
        &self,
        session_id: &str,
        image: &ImageReference,
    ) -> Result<super::StoredSessionFile> {
        let (mut record, _) = self.resolve(session_id, &image.file.id).await?;
        if record.file != image.file {
            return Err(Error::Tool("image reference metadata mismatch".into()));
        }
        let dimensions = match (record.image_dimensions, record.image_orientation) {
            (Some(dimensions), Some(_)) => dimensions,
            _ => {
                let bytes = self.read_file(session_id, &image.file).await?;
                let (_, media_type, width, height, orientation) =
                    validated_image(bytes, &self.image_work).await?;
                if media_type != image.file.media_type {
                    return Err(Error::Tool("image reference media type mismatch".into()));
                }
                self.remember_image_preparation(
                    session_id,
                    &image.file,
                    [width, height],
                    orientation,
                    None,
                )
                .await?;
                record.image_dimensions = Some([width, height]);
                record.image_orientation = Some(orientation);
                [width, height]
            }
        };
        if dimensions != [image.width, image.height] {
            return Err(Error::Tool("image reference dimensions mismatch".into()));
        }
        Ok(record)
    }

    async fn resolve_rendition(
        &self,
        session_id: &str,
        rendition: &ImageRendition,
        key: &str,
        [width, height]: [u32; 2],
    ) -> Result<Option<ImageRendition>> {
        if self.file_is_missing(session_id, &rendition.file.id).await? {
            return Ok(None);
        }
        let (record, _) = self.resolve(session_id, &rendition.file.id).await?;
        if record.file != rendition.file {
            return Err(Error::Tool("invalid image rendition".into()));
        }
        if record.image_rendition_key.as_deref() != Some(key) {
            return Ok(None);
        }
        if (rendition.width, rendition.height) != (width, height)
            || model_dimensions(&record) != Some((width, height))
        {
            return Err(Error::Tool("invalid image rendition dimensions".into()));
        }
        Ok(Some(ImageRendition {
            file: record.file,
            width,
            height,
        }))
    }

    async fn remember_image_preparation(
        &self,
        session_id: &str,
        file: &SessionFileReference,
        dimensions: [u32; 2],
        orientation: u8,
        key: Option<String>,
    ) -> Result<()> {
        let _commit = self.commits.lock().await;
        let (mut record, _) = self.resolve(session_id, &file.id).await?;
        if record.file != *file
            || record
                .image_dimensions
                .is_some_and(|stored| stored != dimensions)
            || record
                .image_orientation
                .is_some_and(|stored| stored != orientation)
        {
            return Err(Error::Tool("image reference metadata mismatch".into()));
        }
        record.image_dimensions = Some(dimensions);
        record.image_orientation = Some(orientation);
        if let Some(key) = key {
            record.image_rendition_key = Some(key);
        }
        super::storage::replace_metadata(&self.session_dir(session_id).join(&file.id), &record)
            .await
    }

    /// Reads and validates an owned image without creating another stored reference.
    /// # Errors
    ///
    /// Returns an error if the file is unauthorized, malformed, or not an image.
    pub async fn read_image(
        &self,
        session_id: &str,
        file: &SessionFileReference,
    ) -> Result<Vec<u8>> {
        let bytes = self.read_file(session_id, file).await?;
        let (bytes, media_type, _, _, _) = validated_image(bytes, &self.image_work).await?;
        if file.media_type != media_type {
            return Err(Error::Tool(
                "stored file media type does not match the image".into(),
            ));
        }
        Ok(bytes)
    }

    /// Reopens an owned image and retains its bytes independently of an upload.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn inspect_image(
        &self,
        session_id: &str,
        file: &SessionFileReference,
        detail: ImageDetail,
    ) -> Result<ImageReference> {
        let bytes = self.read_file(session_id, file).await?;
        let (_, media_type, width, height, orientation) =
            validated_image(bytes, &self.image_work).await?;
        if file.media_type != media_type {
            return Err(Error::Tool(
                "stored file media type does not match the image".into(),
            ));
        }
        let file = self
            .retain_reference(session_id, session_id, file, SessionFileOrigin::Observation)
            .await?;
        self.remember_image_preparation(session_id, &file, [width, height], orientation, None)
            .await?;
        let mut image = ImageReference {
            file,
            width,
            height,
            detail,
            rendition: None,
        };
        image.rendition = self.prepare_image(session_id, &image).await?;
        Ok(image)
    }

    /// Publishes owned stored bytes without copying or re-encoding them.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn publish_reference(
        &self,
        session_id: &str,
        file: &SessionFileReference,
    ) -> Result<SessionFileReference> {
        self.retain_reference(session_id, session_id, file, SessionFileOrigin::Artifact)
            .await
    }

    /// Grants a trusted fork access to an exact parent file, preserving its ID.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn grant_file(
        &self,
        parent: &str,
        child: &str,
        file: &SessionFileReference,
    ) -> Result<SessionFileReference> {
        self.retain_reference(parent, child, file, SessionFileOrigin::Observation)
            .await
    }

    /// Grants an authenticated chat's exact upload to one trusted agent execution.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn grant_upload(
        &self,
        source: &str,
        target: &str,
        file: &SessionFileReference,
    ) -> Result<SessionFileReference> {
        self.verify_upload(source, file).await?;
        self.retain_reference(source, target, file, SessionFileOrigin::Upload)
            .await
    }

    /// Publishes an execution's exact artifact in its owning chat without copying bytes.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn share_artifact(
        &self,
        source: &str,
        target: &str,
        file: &SessionFileReference,
    ) -> Result<SessionFileReference> {
        self.retain_reference(source, target, file, SessionFileOrigin::Artifact)
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
        let _reservation = self.reserve_session(target, file.size, &completed)?;
        record.origin = origin;
        let staging = session_dir.join(format!(".{}-partial", record.file.id));
        super::create_private_dir(&staging).await?;
        super::save_metadata(&staging, &record).await?;
        tokio::fs::rename(&staging, directory).await?;
        Ok(record.file)
    }

    /// Validates and records an immutable image without publishing an artifact.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn ingest_image(
        &self,
        session_id: &str,
        name: String,
        bytes: Vec<u8>,
        detail: ImageDetail,
    ) -> Result<ImageReference> {
        self.ingest_observation(session_id, name, bytes, detail, false)
            .await
    }

    /// Records a coordinate-sensitive capture only if its pixels already fit presentation limits.
    /// # Errors
    /// Rejects oversized captures before any resizing or storage.
    pub async fn ingest_screenshot(
        &self,
        session_id: &str,
        name: String,
        bytes: Vec<u8>,
        detail: ImageDetail,
    ) -> Result<ImageReference> {
        self.ingest_observation(session_id, name, bytes, detail, true)
            .await
    }

    async fn ingest_observation(
        &self,
        session_id: &str,
        name: String,
        bytes: Vec<u8>,
        detail: ImageDetail,
        coordinates: bool,
    ) -> Result<ImageReference> {
        let (bytes, media_type, width, height, orientation) =
            validated_image(bytes, &self.image_work).await?;
        if coordinates && !self.image_policy.fits(width, height) {
            return Err(Error::Tool("computer capture exceeds presentation dimensions; reduce the viewport or capture size to preserve click coordinates".into()));
        }
        let size = u64::try_from(bytes.len())
            .map_err(|_| Error::Tool("image file size overflow".into()))?;
        let mut pending = self
            .begin(
                session_id,
                name,
                size,
                media_type.into(),
                SessionFileOrigin::Observation,
                false,
            )
            .await?;
        pending.record.image_dimensions = Some([width, height]);
        pending.record.image_orientation = Some(orientation);
        let mut image = ImageReference {
            file: pending.complete_bytes(&bytes).await?,
            width,
            height,
            detail,
            rendition: None,
        };
        image.rendition = self.prepare_image(session_id, &image).await?;
        Ok(image)
    }

    /// Validates and publishes one image directly as an agent artifact.
    /// # Errors
    ///
    /// Returns an error if the image is malformed, mislabeled, or cannot be stored.
    pub async fn publish_image(
        &self,
        session_id: &str,
        name: String,
        declared_media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<SessionFileReference> {
        let (bytes, media_type, _, _, _) = validated_image(bytes, &self.image_work).await?;
        if declared_media_type != media_type {
            return Err(Error::Tool(
                "generated image media type does not match its bytes".into(),
            ));
        }
        self.publish_bytes(
            session_id,
            name,
            media_type.into(),
            &bytes,
            SessionFileOrigin::Artifact,
            true,
        )
        .await
    }

    /// Reads an exact file reference authorized by the owning session.
    /// # Errors
    ///
    /// Returns an error if the resource cannot be read, decoded, or validated.
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
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn file_reference(
        &self,
        session_id: &str,
        file_id: &str,
    ) -> Result<SessionFileReference> {
        Ok(self.resolve(session_id, file_id).await?.0.file)
    }
}

fn prepare_pixels(
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    policy: ImagePresentation,
) -> Result<Option<(Vec<u8>, &'static str)>> {
    let (media_type, mut decoded, orientation) = decode_image(&bytes)?;
    decoded.apply_orientation(orientation);
    let source_png = media_type == "image/png";
    let try_jpeg = source_png
        && u64::try_from(bytes.len()).is_ok_and(|size| size >= policy.png_jpeg_threshold_bytes)
        && opaque_pixels(&decoded);
    let resized = (decoded.width(), decoded.height()) != (width, height);
    let prepared = if resized {
        decoded.resize_exact(width, height, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };
    let (format, media_type) = match media_type {
        "image/jpeg" => (ImageFormat::Jpeg, "image/jpeg"),
        "image/webp" => (ImageFormat::WebP, "image/webp"),
        _ => (ImageFormat::Png, "image/png"),
    };
    let output = if !resized && source_png {
        bytes
    } else {
        encode_pixels(&prepared, format)?
    };
    if try_jpeg {
        let jpeg = encode_pixels(&prepared, ImageFormat::Jpeg)?;
        if jpeg.len() < output.len() {
            return Ok(Some((jpeg, "image/jpeg")));
        }
    }
    Ok(resized.then_some((output, media_type)))
}

fn encode_pixels(image: &image::DynamicImage, format: ImageFormat) -> Result<Vec<u8>> {
    let mut output = Cursor::new(Vec::new());
    if format == ImageFormat::Jpeg {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 85)
            .encode_image(image)
            .map_err(image_error)?;
    } else {
        image.write_to(&mut output, format).map_err(image_error)?;
    }
    Ok(output.into_inner())
}

fn opaque_pixels(image: &image::DynamicImage) -> bool {
    // Inspect native alpha precision; converting 16-bit alpha to 8-bit can hide transparency.
    match image {
        image::DynamicImage::ImageLumaA8(pixels) => {
            pixels.pixels().all(|pixel| pixel.0[1] == u8::MAX)
        }
        image::DynamicImage::ImageRgba8(pixels) => {
            pixels.pixels().all(|pixel| pixel.0[3] == u8::MAX)
        }
        image::DynamicImage::ImageLumaA16(pixels) => {
            pixels.pixels().all(|pixel| pixel.0[1] == u16::MAX)
        }
        image::DynamicImage::ImageRgba16(pixels) => {
            pixels.pixels().all(|pixel| pixel.0[3] == u16::MAX)
        }
        _ => !image.color().has_alpha(),
    }
}

async fn validated_image(
    bytes: Vec<u8>,
    work: &Arc<tokio::sync::Semaphore>,
) -> Result<(Vec<u8>, &'static str, u32, u32, u8)> {
    let permit = Arc::clone(work)
        .acquire_owned()
        .await
        .map_err(|_| Error::Tool("image decoder unavailable".into()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let (media_type, width, height, orientation) = validate_image(&bytes)?;
        Ok::<_, Error>((bytes, media_type, width, height, orientation))
    })
    .await
    .map_err(|error| Error::Tool(format!("image decoder failed: {error}")))?
}

/// Retains referenced files before a trusted fork; text-only forks need no file store.
/// # Errors
///
/// Returns an error if validation or an operation required by this function fails.
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
        let kind = part.get("type").and_then(serde_json::Value::as_str);
        if !matches!(kind, Some("input_image" | "file")) {
            continue;
        }
        let files = files
            .ok_or_else(|| Error::Config("forking media requires session file storage".into()))?;
        if kind == Some("input_image") {
            let image = part.get("image").ok_or_else(|| {
                Error::Config("forked observation requires an image reference".into())
            })?;
            let image: ImageReference = serde::Deserialize::deserialize(image)?;
            files.grant_file(parent, child, &image.file).await?;
            if let Some(rendition) = files.prepare_image(parent, &image).await? {
                files.grant_file(parent, child, &rendition.file).await?;
            }
        } else {
            let file = part.get("file").ok_or_else(|| {
                Error::Config("forked observation requires a file reference".into())
            })?;
            files
                .grant_file(parent, child, &serde::Deserialize::deserialize(file)?)
                .await?;
        }
    }
    Ok(())
}

fn validate_image(bytes: &[u8]) -> Result<(&'static str, u32, u32, u8)> {
    let (media_type, decoded, orientation) = decode_image(bytes)?;
    Ok((
        media_type,
        decoded.width(),
        decoded.height(),
        orientation.to_exif(),
    ))
}

fn oriented_dimensions(width: u32, height: u32, orientation: u8) -> (u32, u32) {
    if (5..=8).contains(&orientation) {
        (height, width)
    } else {
        (width, height)
    }
}

fn model_dimensions(record: &super::StoredSessionFile) -> Option<(u32, u32)> {
    record.image_dimensions.map(|[width, height]| {
        oriented_dimensions(width, height, record.image_orientation.unwrap_or(1))
    })
}

fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits
}

pub(super) fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0
        || height == 0
        || width.max(height) > 16_384
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(Error::Tool("image exceeds the decoded pixel limit".into()));
    }
    Ok(())
}

fn decode_image(bytes: &[u8]) -> Result<(&'static str, image::DynamicImage, Orientation)> {
    let format = image::guess_format(bytes).map_err(image_error)?;
    let media_type = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::WebP => "image/webp",
        ImageFormat::Gif => "image/gif",
        _ => return Err(Error::Tool("image must be PNG, JPEG, WebP, or GIF".into())),
    };
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(decode_limits());
    let mut decoder = reader.into_decoder().map_err(image_error)?;
    let (width, height) = decoder.dimensions();
    validate_dimensions(width, height)?;
    decoder.set_limits(decode_limits()).map_err(image_error)?;
    let orientation = decoder.orientation().map_err(image_error)?;
    let decoded = image::DynamicImage::from_decoder(decoder).map_err(image_error)?;
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
            decoder.set_limits(decode_limits()).map_err(image_error)?;
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
    Ok((media_type, decoded, orientation))
}

fn image_error(error: image::ImageError) -> Error {
    Error::Tool(format!("invalid or unsupported image: {error}"))
}

#[cfg(test)]
mod presentation_tests {
    use super::*;
    use image::ImageEncoder as _;

    #[test]
    fn fitting_preserves_aspect_ratio_and_both_budgets_without_enlarging() {
        let policy = ImagePresentation::default();
        assert_eq!(policy.dimensions(4032, 3024), (1835, 1376));
        for (width, height) in [
            (1, 1),
            (1365, 768),
            (1170, 2532),
            (4032, 3024),
            (16384, 1),
            (1800, 1800),
        ] {
            let (w, h) = policy.dimensions(width, height);
            assert!(policy.fits(w, h));
            assert!(w <= width && h <= height);
            assert!(
                (f64::from(w) / f64::from(width) - f64::from(h) / f64::from(height)).abs()
                    <= 1.0 / f64::from(width.min(height))
            );
            if policy.fits(width, height) {
                assert_eq!((w, h), (width, height));
            }
        }
    }

    #[test]
    fn png_transparency_survives_jpeg_selection_at_native_alpha_precision() {
        let policy = ImagePresentation {
            png_jpeg_threshold_bytes: 1,
            ..ImagePresentation::default()
        };
        for alpha in [u16::MAX - 1, u16::MAX / 2, 0] {
            let pixels = image::ImageBuffer::from_pixel(
                32,
                32,
                image::Rgba([10000_u16, 20000, 30000, alpha]),
            );
            let mut png = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba16(pixels)
                .write_to(&mut png, ImageFormat::Png)
                .expect("PNG");
            let original = png.into_inner();
            assert!(
                prepare_pixels(original, 32, 32, policy)
                    .expect("prepare")
                    .is_none()
            );
        }
        let pixels = image::DynamicImage::new_rgb8(32, 32);
        let png = encode_pixels(&pixels, ImageFormat::Png).expect("small PNG");
        assert!(
            prepare_pixels(png, 32, 32, policy)
                .expect("size comparison")
                .is_none(),
            "do not enlarge a compact PNG with JPEG"
        );
    }

    #[tokio::test]
    async fn unchanged_pixels_cache_on_the_original_without_another_quota_charge() {
        let state = tempfile::tempdir().expect("state");
        let policy = ImagePresentation {
            png_jpeg_threshold_bytes: 1,
            ..ImagePresentation::default()
        };
        let store = SessionFileStore::new(state.path(), None)
            .image_presentation(policy)
            .expect("policy");
        let pixels = image::DynamicImage::new_rgba8(32, 32);
        let bytes = encode_pixels(&pixels, ImageFormat::Png).expect("transparent PNG");
        let original_size = u64::try_from(bytes.len()).expect("size");
        let image = store
            .ingest_image(
                "session",
                "transparent.png".into(),
                bytes,
                ImageDetail::High,
            )
            .await
            .expect("image");
        assert_eq!(
            image.rendition.as_ref().expect("cached no-op").file,
            image.file
        );
        let reopened = SessionFileStore::new(state.path(), None)
            .image_presentation(policy)
            .expect("policy");
        let mut reference = image.clone();
        reference.rendition = None;
        assert_eq!(
            reopened
                .prepare_image("session", &reference)
                .await
                .expect("cache"),
            image.rendition
        );
        assert_eq!(
            reopened
                .list_files("session", &[SessionFileOrigin::Observation])
                .await
                .expect("files")
                .len(),
            1
        );
        assert_eq!(reopened.stored_bytes().await.expect("bytes"), original_size);
        let input =
            [serde_json::json!({"role":"user", "content":[{"type":"input_image", "image":image}]})];
        grant_context(Some(&store), "session", "child", &input)
            .await
            .expect("fork");
        assert_eq!(
            store
                .list_files("child", &[SessionFileOrigin::Observation])
                .await
                .expect("child files")
                .len(),
            1
        );
        store
            .delete_session("session")
            .await
            .expect("delete parent");
        assert_eq!(
            store.stored_bytes().await.expect("child bytes"),
            original_size
        );
        store.delete_session("child").await.expect("delete child");
        assert_eq!(store.stored_bytes().await.expect("collected"), 0);
    }

    #[tokio::test]
    async fn preparation_rejects_claimed_dimensions_before_reusing_cached_pixels() {
        let state = tempfile::tempdir().expect("state");
        let store = SessionFileStore::new(state.path(), None)
            .image_presentation(ImagePresentation {
                max_dimension: 64,
                max_patches: 2,
                ..ImagePresentation::default()
            })
            .expect("policy");
        let bytes =
            encode_pixels(&image::DynamicImage::new_rgb8(256, 128), ImageFormat::Png).expect("PNG");
        let mut image = store
            .ingest_image("session", "image.png".into(), bytes, ImageDetail::High)
            .await
            .expect("image");
        image.width = 1;
        assert!(store.prepare_image("session", &image).await.is_err());
        image.width = 256;
        image.rendition.as_mut().expect("rendition").width = 1;
        assert!(store.prepare_image("session", &image).await.is_err());
        image.rendition.as_mut().expect("rendition").width = 64;
        let old_id = image.rendition.as_ref().expect("rendition").file.id.clone();
        store
            .prepare_delete_sessions(
                &["session".into()],
                super::super::SessionFileSelection::Ids(vec![old_id.clone()]),
            )
            .await
            .expect("prepare purge")
            .delete()
            .await
            .expect("purge rendition");
        let regenerated = store
            .prepare_image("session", &image)
            .await
            .expect("rebuild purged rendition")
            .expect("rendition");
        assert_ne!(regenerated.file.id, old_id);
        assert_eq!(
            store
                .prepare_image("session", &image)
                .await
                .expect("reuse regenerated")
                .as_ref(),
            Some(&regenerated)
        );
        let input =
            [serde_json::json!({"role":"user", "content":[{"type":"input_image", "image":image}]})];
        grant_context(Some(&store), "session", "child", &input)
            .await
            .expect("fork repairs the historical rendition pointer");
        assert_eq!(
            store
                .prepare_image("child", &image)
                .await
                .expect("child retains the current rendition")
                .as_ref(),
            Some(&regenerated)
        );
        assert!(
            grant_context(Some(&store), "stranger", "other", &input)
                .await
                .is_err()
        );
        let (record, _) = store
            .resolve("session", &regenerated.file.id)
            .await
            .expect("record");
        std::fs::write(store.blob_path(&record.content_hash), b"tampered").expect("tamper blob");
        image.rendition = Some(regenerated);
        assert!(store.prepare_image("session", &image).await.is_err());
        let (original, _) = store
            .resolve("session", &image.file.id)
            .await
            .expect("original");
        std::fs::write(
            store.blob_path(&original.content_hash),
            b"tampered original",
        )
        .expect("tamper original");
        assert!(
            grant_context(Some(&store), "session", "corrupt", &input)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn exif_rotation_fits_visual_pixels_and_preserves_original_reference_geometry() {
        let state = tempfile::tempdir().expect("state");
        let store = SessionFileStore::new(state.path(), None)
            .image_presentation(ImagePresentation {
                max_dimension: 64,
                max_patches: 2,
                ..ImagePresentation::default()
            })
            .expect("policy");
        let pixels = image::RgbImage::from_fn(128, 64, |x, _| {
            if x < 64 {
                image::Rgb([255, 0, 0])
            } else {
                image::Rgb([0, 255, 0])
            }
        });
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 95);
        // Little-endian TIFF containing one SHORT orientation tag: rotate 90 degrees.
        encoder
            .set_exif_metadata(vec![
                0x49, 0x49, 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0,
                0, 0, 0,
            ])
            .expect("EXIF");
        encoder.encode_image(&pixels).expect("JPEG");
        // Existing references have raw encoded geometry and no cached preparation metadata.
        let file = store
            .publish_artifact("session", "rotated.jpg".into(), "image/jpeg".into(), &bytes)
            .await
            .expect("original");
        let image = ImageReference {
            file,
            width: 128,
            height: 64,
            detail: ImageDetail::High,
            rendition: None,
        };
        let rendition = store
            .prepare_image("session", &image)
            .await
            .expect("prepare existing reference")
            .expect("rendition");
        assert_eq!((image.width, image.height), (128, 64));
        assert_eq!((rendition.width, rendition.height), (32, 64));
        assert_eq!(
            store
                .read_file("session", &image.file)
                .await
                .expect("original"),
            bytes
        );
        let prepared = store
            .read_file("session", &rendition.file)
            .await
            .expect("rendition pixels");
        let (_, decoded, orientation) = decode_image(&prepared).expect("decode rendition");
        assert_eq!((decoded.width(), decoded.height()), (32, 64));
        assert_eq!(orientation.to_exif(), 1);
        let pixels = decoded.to_rgb8();
        let top = pixels.get_pixel(16, 8).0;
        let bottom = pixels.get_pixel(16, 56).0;
        assert!(top[0] > 200 && top[1] < 30, "red rotates to the top");
        assert!(
            bottom[1] > 200 && bottom[0] < 30,
            "green rotates to the bottom"
        );
        let record = store
            .resolve("session", &image.file.id)
            .await
            .expect("record")
            .0;
        assert_eq!(record.image_dimensions, Some([128, 64]));
        assert_eq!(record.image_orientation, Some(6));
        assert_eq!(
            store
                .prepare_image("session", &image)
                .await
                .expect("cache")
                .as_ref(),
            Some(&rendition)
        );
    }

    #[tokio::test]
    async fn missing_file_check_distinguishes_purge_from_corruption() {
        let state = tempfile::tempdir().expect("state");
        let store = SessionFileStore::new(state.path(), None);
        let file = store
            .publish_artifact("session", "file.txt".into(), "text/plain".into(), b"file")
            .await
            .expect("file");
        let directory = store.session_dir("session").join(&file.id);
        assert!(
            !store
                .file_is_missing("session", &file.id)
                .await
                .expect("present")
        );
        tokio::fs::remove_file(directory.join(super::super::METADATA_FILE))
            .await
            .expect("remove metadata");
        assert!(
            !store
                .file_is_missing("session", &file.id)
                .await
                .expect("corrupt")
        );
        tokio::fs::remove_dir(&directory)
            .await
            .expect("remove directory");
        assert!(
            store
                .file_is_missing("session", &file.id)
                .await
                .expect("purged")
        );
        tokio::fs::write(&directory, b"not a directory")
            .await
            .expect("non-directory");
        assert!(store.file_is_missing("session", &file.id).await.is_err());
        #[cfg(unix)]
        {
            tokio::fs::remove_file(&directory)
                .await
                .expect("remove file");
            std::os::unix::fs::symlink(state.path(), &directory).expect("symlink");
            assert!(store.file_is_missing("session", &file.id).await.is_err());
        }
    }

    #[tokio::test]
    async fn originals_renditions_cache_forks_and_capture_coordinates_share_storage_rules() {
        let state = tempfile::tempdir().expect("state");
        let policy = ImagePresentation {
            max_dimension: 64,
            max_patches: 2,
            ..ImagePresentation::default()
        };
        let store = SessionFileStore::new(state.path(), None)
            .image_presentation(policy)
            .expect("policy");
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(256, 128)
                .write_to(&mut bytes, format)
                .expect("image");
            let original = bytes.into_inner();
            let mut observation = store
                .ingest_image(
                    "parent",
                    "original".into(),
                    original.to_vec(),
                    ImageDetail::High,
                )
                .await
                .expect("ingest");
            assert_eq!(
                store
                    .read_file("parent", &observation.file)
                    .await
                    .expect("original"),
                original
            );
            let rendition = observation.rendition.take().expect("prepared");
            assert_eq!((rendition.width, rendition.height), (64, 32));
            let prepared = store
                .read_image("parent", &rendition.file)
                .await
                .expect("prepared bytes");
            assert_eq!(
                image::load_from_memory(&prepared).expect("decode").width(),
                64
            );
            assert_eq!(image::guess_format(&prepared).expect("format"), format);
            let reopened = SessionFileStore::new(state.path(), None)
                .image_presentation(policy)
                .expect("policy");
            assert_eq!(
                reopened
                    .prepare_image("parent", &observation)
                    .await
                    .expect("persistent cache")
                    .expect("rendition"),
                rendition
            );
            assert!(store.prepare_image("stranger", &observation).await.is_err());
            observation.rendition = Some(rendition);
            let stricter = SessionFileStore::new(state.path(), None)
                .image_presentation(ImagePresentation {
                    max_dimension: 32,
                    max_patches: 1,
                    ..ImagePresentation::default()
                })
                .expect("stricter policy");
            let different = stricter
                .prepare_image("parent", &observation)
                .await
                .expect("policy cache key")
                .expect("stricter rendition");
            assert_eq!((different.width, different.height), (32, 16));
            assert_ne!(
                different.file.id,
                observation
                    .rendition
                    .as_ref()
                    .expect("initial rendition")
                    .file
                    .id
            );

            let input = [
                serde_json::json!({"role":"user", "content":[{"type":"input_image", "image":observation}]}),
            ];
            grant_context(Some(&store), "parent", "child", &input)
                .await
                .expect("fork");
            assert!(
                store
                    .prepare_image("child", &observation)
                    .await
                    .expect("child authorization")
                    .is_some()
            );
            assert!(
                store
                    .ingest_screenshot("parent", "capture".into(), original, ImageDetail::High)
                    .await
                    .is_err()
            );
        }
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(16, 16)
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("small image");
        let original = bytes.into_inner();
        let image = store
            .ingest_screenshot(
                "parent",
                "small".into(),
                original.to_vec(),
                ImageDetail::High,
            )
            .await
            .expect("fitting capture");
        assert!(image.rendition.is_none());
        assert_eq!(
            store
                .read_file("parent", &image.file)
                .await
                .expect("fitting bytes"),
            original
        );
        store.delete_session("parent").await.expect("delete parent");
        assert!(store.stored_bytes().await.expect("fork charged") > 0);
        store.delete_session("child").await.expect("delete child");
        assert_eq!(store.stored_bytes().await.expect("collected"), 0);
    }
}
