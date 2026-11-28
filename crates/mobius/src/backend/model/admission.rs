//! Model request admission: the complete provider-encoded body must fit its route's budget.

use super::ModelInput;
use super::media::MediaPreparation;
use crate::{Error, Result};

/// Serializes the admitted transport body, retaining the exact bytes that passed admission.
/// Providers own encoding; continuation transports pass only their selected logical suffix.
pub(super) async fn encode_request<M: super::Model + ?Sized>(
    model: &M,
    request: super::ModelRequest<'_>,
    media: Option<MediaPreparation<'_>>,
    replay: bool,
    mut encode: impl FnMut(ModelInput<'_>) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut prepared = match media {
        Some(media) => {
            media
                .prepare(request.session_id, request.input, model, replay)
                .await?
        }
        None => None,
    };
    let max_bytes = model.transport_settings().max_request_bytes;
    let (Some(input), Some(media)) = (prepared.as_mut(), media) else {
        let body = encode(request.input)?;
        check_request_bytes(body.len(), max_bytes)?;
        return Ok(body);
    };
    let mut body = Vec::new();
    super::media::bound_request(
        input,
        request.input,
        media.limits,
        max_bytes,
        replay,
        |input| {
            // A demoted replay cannot reuse the old bytes; release them before re-encoding.
            drop(std::mem::take(&mut body));
            body = encode(input.into())?;
            Ok(body.len())
        },
    )?;
    Ok(body)
}

pub(super) fn check_request_bytes(bytes: usize, max_bytes: usize) -> Result<()> {
    if bytes > max_bytes {
        return Err(over_budget());
    }
    Ok(())
}

pub(super) fn over_budget() -> Error {
    Error::Provider("the newest observation batch and retained text exceed the model request budget; use fewer or smaller images, or reduce the text before retrying".into())
}
