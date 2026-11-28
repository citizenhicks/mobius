//! Input accepted by provider dictation adapters.
use tokio::sync::mpsc;

use crate::{Error, Result};

/// Largest frame accepted by the dictation transport, one second at 24 kHz.
pub const MAX_CHUNK_BYTES: usize = 48_000;
/// Largest recording accepted by a provider dictation request.
pub const MAX_RECORDING_BYTES: usize = 16 * 1024 * 1024;

/// Language hint normalized from the microphone's locale.
#[derive(Debug)]
pub struct DictationLanguage(String);

impl DictationLanguage {
    /// Validates the locale and extracts the language used by the transcription APIs.
    ///
    /// # Errors
    /// Rejects malformed locale components and unbounded hints.
    pub fn new(mut locale: String) -> Result<Self> {
        let mut parts = locale.split(['-', '_']);
        let language = parts.next().unwrap_or_default();
        if locale.len() > 64
            || !(1..=8).contains(&language.len())
            || !language.bytes().all(|b| b.is_ascii_alphabetic())
            || parts.any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric()))
        {
            return Err(Error::Invalid("invalid dictation language".into()));
        }
        locale.truncate(language.len());
        locale.make_ascii_lowercase();
        Ok(Self(locale))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Bounded input for one mono little-endian PCM16 recording. Dropping it signals stop.
#[derive(Debug)]
pub struct DictationInput {
    sender: mpsc::Sender<Vec<u8>>,
    bytes: usize,
}

/// Audio accepted for a single transcription.
#[derive(Debug)]
pub struct DictationAudio(pub(super) mpsc::Receiver<Vec<u8>>);

impl DictationInput {
    /// Creates the recording and its provider input queue.
    #[must_use]
    pub fn channel() -> (Self, DictationAudio) {
        let (sender, receiver) = mpsc::channel(32);
        (Self { sender, bytes: 0 }, DictationAudio(receiver))
    }

    /// Adds a bounded, sample-aligned audio frame.
    ///
    /// # Errors
    /// Rejects empty or oversized frames, recordings over the limit, and a full or closed queue.
    pub fn try_send(&mut self, bytes: Vec<u8>) -> Result<()> {
        let count = bytes.len();
        if count == 0 || count > MAX_CHUNK_BYTES || !count.is_multiple_of(2) {
            return Err(Error::Invalid("invalid PCM16 dictation frame".into()));
        }
        if count > MAX_RECORDING_BYTES - self.bytes {
            return Err(Error::Invalid("dictation duration limit exceeded".into()));
        }
        self.sender.try_send(bytes).map_err(|_unavailable| {
            Error::Invalid("the dictation stream could not accept audio in time".into())
        })?;
        self.bytes += count;
        Ok(())
    }
}
