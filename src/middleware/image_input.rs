use base64::Engine as _;
use serde_json::Value;

use crate::backend::model::image_input;
use crate::{Error, Result};

pub(crate) const MAX_IMAGE_INPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ImageInputStats {
    pub(crate) bytes: usize,
    pub(crate) count: usize,
}

pub(crate) fn model_input_image_bytes<'a>(
    items: impl IntoIterator<Item = &'a Value>,
) -> Result<usize> {
    Ok(model_input_image_stats(items)?.bytes)
}

pub(crate) fn model_input_image_stats<'a>(
    items: impl IntoIterator<Item = &'a Value>,
) -> Result<ImageInputStats> {
    let mut stats = ImageInputStats::default();
    for item in items {
        let Some(content) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in content {
            let Some((_, data)) = image_input(part, "model")? else {
                continue;
            };
            if base64::decoded_len_estimate(data.len()) > MAX_IMAGE_INPUT_BYTES + 2 {
                return image_input_limit();
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| Error::Provider("model image input contains invalid base64".into()))?;
            stats.bytes = checked_image_input_bytes(stats.bytes, bytes.len())?;
            stats.count = stats
                .count
                .checked_add(1)
                .ok_or_else(|| Error::Provider("model image input count overflow".into()))?;
        }
    }
    Ok(stats)
}

pub(crate) fn checked_image_input_bytes(used: usize, additional: usize) -> Result<usize> {
    used.checked_add(additional)
        .filter(|total| *total <= MAX_IMAGE_INPUT_BYTES)
        .map_or_else(image_input_limit, Ok)
}

fn image_input_limit<T>() -> Result<T> {
    Err(Error::Provider(
        "image input exceeds the 8 MiB model-input limit".into(),
    ))
}

pub(crate) fn raster_media_type(bytes: &[u8]) -> Result<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Ok("image/jpeg");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        if bytes
            .windows(4)
            .any(|window| matches!(window, b"ANIM" | b"ANMF"))
        {
            return Err(Error::Provider(
                "animated WebP images are not supported".into(),
            ));
        }
        return Ok("image/webp");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        if gif_image_count(bytes)? == 1 {
            return Ok("image/gif");
        }
        return Err(Error::Provider(
            "animated GIF images are not supported".into(),
        ));
    }
    Err(Error::Provider(
        "image is not a supported PNG, JPEG, WebP, or GIF".into(),
    ))
}

fn gif_image_count(bytes: &[u8]) -> Result<usize> {
    if bytes.len() < 13 {
        return Err(Error::Provider("GIF image is truncated".into()));
    }
    let packed = bytes[10];
    let global_table = if packed & 0x80 == 0 {
        0
    } else {
        3_usize << (usize::from(packed & 0x07) + 1)
    };
    let mut offset = 13_usize
        .checked_add(global_table)
        .ok_or_else(|| Error::Provider("GIF image size overflow".into()))?;
    let mut images = 0_usize;
    while offset < bytes.len() {
        match bytes[offset] {
            0x2c => {
                images += 1;
                if images > 1 {
                    return Ok(images);
                }
                let descriptor_end = offset
                    .checked_add(10)
                    .filter(|end| *end <= bytes.len())
                    .ok_or_else(|| Error::Provider("GIF image descriptor is truncated".into()))?;
                let packed = bytes[descriptor_end - 1];
                let local_table = if packed & 0x80 == 0 {
                    0
                } else {
                    3_usize << (usize::from(packed & 0x07) + 1)
                };
                offset = descriptor_end
                    .checked_add(local_table)
                    .and_then(|value| value.checked_add(1))
                    .filter(|value| *value <= bytes.len())
                    .ok_or_else(|| Error::Provider("GIF image data is truncated".into()))?;
                offset = skip_gif_sub_blocks(bytes, offset)?;
            }
            0x21 => {
                offset = offset
                    .checked_add(2)
                    .filter(|value| *value <= bytes.len())
                    .ok_or_else(|| Error::Provider("GIF extension is truncated".into()))?;
                offset = skip_gif_sub_blocks(bytes, offset)?;
            }
            0x3b => return Ok(images),
            _ => return Err(Error::Provider("GIF image is malformed".into())),
        }
    }
    Err(Error::Provider("GIF image has no trailer".into()))
}

fn skip_gif_sub_blocks(bytes: &[u8], mut offset: usize) -> Result<usize> {
    loop {
        let length = usize::from(
            *bytes
                .get(offset)
                .ok_or_else(|| Error::Provider("GIF data block is truncated".into()))?,
        );
        offset = offset
            .checked_add(1)
            .and_then(|value| value.checked_add(length))
            .filter(|value| *value <= bytes.len())
            .ok_or_else(|| Error::Provider("GIF data block is truncated".into()))?;
        if length == 0 {
            return Ok(offset);
        }
    }
}
