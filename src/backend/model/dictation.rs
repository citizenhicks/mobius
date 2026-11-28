//! Transcription of a composer's microphone input through a configured provider.
mod input;
mod live;
mod provider;

pub use input::{
    DictationAudio, DictationInput, DictationLanguage, MAX_CHUNK_BYTES, MAX_RECORDING_BYTES,
};
pub(in crate::backend::model) use provider::DictationApi;
pub use provider::{DictationMode, DictationProvider};

#[cfg(test)]
mod tests;
