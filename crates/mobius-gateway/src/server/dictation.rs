//! One ephemeral recording per paired connection. Dropping the connection cancels paid work.

use crate::host::{GatewayHost, Rejection};
use crate::wire::ServerMessage;
use base64::engine::general_purpose::STANDARD;
use mobius::backend::model::dictation::{DictationInput, DictationLanguage, MAX_CHUNK_BYTES};
use std::future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub(super) struct Recording {
    id: Arc<str>,
    audio: Option<DictationInput>,
    updates: mpsc::Receiver<String>,
    task: JoinHandle<Result<String, Rejection>>,
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) async fn write_error(
    writer: &mut (impl super::AsyncWrite + Unpin),
    request_id: Arc<str>,
    result: Result<(), Rejection>,
) -> super::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(rejection) => super::write_rejection(writer, request_id, rejection).await,
    }
}

fn rejected(message: &str) -> Rejection {
    Rejection::new("invalid_dictation", message)
}

fn decode(input: &str) -> Result<Vec<u8>, Rejection> {
    use base64::Engine as _;
    // Bound the allocation before decoding. DictationInput owns all decoded audio limits.
    if input.len() > MAX_CHUNK_BYTES.div_ceil(3) * 4 {
        return Err(rejected("Dictation frame exceeds its size limit"));
    }
    STANDARD
        .decode(input)
        .map_err(|_invalid| rejected("Invalid base64 dictation audio"))
}

pub(super) fn start(
    slot: &mut Option<Recording>,
    id: &Arc<str>,
    host: &Arc<GatewayHost>,
    route: String,
    language: Option<String>,
) -> Result<(), Rejection> {
    if slot.is_some() {
        return Err(rejected("A dictation is already active on this connection"));
    }
    if !crate::single_line_field(id, 256) || !crate::single_line_field(&route, 1024) {
        return Err(rejected("Invalid dictation request or route"));
    }
    let language = language
        .map(DictationLanguage::new)
        .transpose()
        .map_err(|error| rejected(&error.to_string()))?;
    let (audio, input) = DictationInput::channel();
    let (send, updates) = mpsc::channel(8);
    let host = Arc::clone(host);
    let task = tokio::spawn(async move {
        tokio::time::timeout(
            Duration::from_secs(8 * 60),
            host.transcribe(&route, language.as_ref(), input, send),
        )
        .await
        .map_err(|_elapsed| rejected("Dictation timed out"))?
    });
    *slot = Some(Recording {
        id: Arc::clone(id),
        audio: Some(audio),
        updates,
        task,
    });
    Ok(())
}

pub(super) fn audio(
    slot: &mut Option<Recording>,
    id: &str,
    encoded: &str,
) -> Result<(), Rejection> {
    let recording = active(slot, id)?;
    let result = (|| {
        let bytes = decode(encoded)?;
        recording
            .audio
            .as_mut()
            .ok_or_else(|| rejected("This dictation is not accepting audio"))?
            .try_send(bytes)
            .map_err(|error| rejected(&error.to_string()))
    })();
    if result.is_err() {
        *slot = None;
    }
    result
}

fn active<'a>(slot: &'a mut Option<Recording>, id: &str) -> Result<&'a mut Recording, Rejection> {
    slot.as_mut()
        .filter(|r| &*r.id == id)
        .ok_or_else(|| rejected("Unknown dictation request"))
}

pub(super) fn finish(slot: &mut Option<Recording>, id: &str) -> Result<(), Rejection> {
    let r = active(slot, id)?;
    r.audio
        .take()
        .ok_or_else(|| rejected("This dictation is not accepting audio"))?;
    Ok(())
}

pub(super) fn cancel(slot: &mut Option<Recording>, id: &str) -> Result<(), Rejection> {
    active(slot, id)?;
    *slot = None;
    Ok(())
}

pub(super) async fn next(slot: &mut Option<Recording>) -> ServerMessage {
    let Some(recording) = slot.as_mut() else {
        return future::pending().await;
    };
    let result = tokio::select! {
        biased;
        Some(text) = recording.updates.recv() => return ServerMessage::DictationPartial {
            request_id: Arc::clone(&recording.id),
            text,
        },
        result = &mut recording.task => result,
    };
    let id = Arc::clone(&recording.id);
    *slot = None;
    match result {
        Ok(Ok(text)) => ServerMessage::DictationTranscript {
            request_id: id,
            text,
        },
        Ok(Err(reason)) => reason.for_request(id),
        Err(error) => {
            tracing::warn!(%error, "dictation worker ended");
            rejected("Dictation stopped unexpectedly").for_request(id)
        }
    }
}
