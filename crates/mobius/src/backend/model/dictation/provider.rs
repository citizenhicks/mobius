//! Transcription of a composer's microphone input through a configured provider.
use std::collections::BTreeMap;
use std::fmt::{self, Debug, Formatter};
use std::io::Cursor;
use std::time::Duration;

use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use tokio::sync::mpsc;

use super::{DictationAudio, DictationLanguage, MAX_RECORDING_BYTES};
use crate::backend::model::authorization::{self, ApiKeyAuthorization};
use crate::backend::model::endpoints::{self, EndpointApi};
use crate::backend::model::provider::{
    BaseUrl, ProviderAuth, ProviderCredential, ProviderDefinition,
};
use crate::backend::model::router::{ModelCredentialLifetime, while_valid};
use crate::backend::model::transport::{self, ModelTransportSettings};
use crate::{Error, Result};

pub(super) const MAX_TRANSCRIPT_BYTES: usize = 128 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::backend::model) struct DictationApi {
    pub(in crate::backend::model) recording: String,
    pub(in crate::backend::model) realtime: Option<String>,
    #[serde(default)]
    live_models: Vec<String>,
}

/// When the endpoint can start returning words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DictationMode {
    /// Incremental text while microphone audio is arriving.
    Live,
    /// A single transcript after the recording finishes.
    AfterStop,
}

impl ProviderDefinition {
    /// Whether this endpoint can be explicitly provisioned with transcription models.
    #[must_use]
    pub fn can_configure_dictation(&self, base_url: Option<&str>) -> bool {
        self.dictation.is_some()
            || (matches!(self.auth(), ProviderAuth::ApiKey(_))
                && self.supports_credentialless_endpoints()
                && !self.uses_default_endpoint(base_url))
    }

    /// Known IDs with incremental transcription. Other admitted IDs use recorded audio.
    #[must_use]
    pub fn live_dictation_models(&self) -> &[String] {
        self.dictation.map_or(&[], |api| api.live_models.as_slice())
    }

    /// The input mode of an explicitly selected transcription model.
    #[must_use]
    pub fn dictation_mode(&self, model: &str, base: &BaseUrl) -> Option<DictationMode> {
        self.can_configure_dictation(Some(base.as_str()))
            .then(|| mode(self.dictation, base, model))
    }
}

fn mode(api: Option<&DictationApi>, base: &BaseUrl, model: &str) -> DictationMode {
    if let Some(mode) = base.operation_dictation_mode() {
        return mode;
    }
    if api.is_some_and(|api| api.live_models.iter().any(|id| id == model)) {
        DictationMode::Live
    } else {
        DictationMode::AfterStop
    }
}

/// Provider-owned transcription using the same HTTP pool and socket connector as chat.
pub struct DictationProvider {
    pub(super) base_url: BaseUrl,
    pub(super) api: Option<&'static DictationApi>,
    pub(super) auth: Option<ApiKeyAuthorization>,
    pub(super) headers: &'static BTreeMap<String, String>,
    pub(super) settings: ModelTransportSettings,
}

impl Debug for DictationProvider {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("DictationProvider")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl DictationProvider {
    /// Binds the provider instance and its credential to the validated endpoint.
    ///
    /// # Errors
    /// Returns an error if the provider or authentication method cannot serve dictation.
    pub fn new(
        definition: &ProviderDefinition,
        base_url: BaseUrl,
        credential: ProviderCredential,
        transport: &ModelTransportSettings,
    ) -> Result<Self> {
        if !definition.can_configure_dictation(Some(base_url.as_str())) {
            return Err(Error::Invalid("provider does not support dictation".into()));
        }
        if matches!(credential, ProviderCredential::Credentialless) {
            definition.validate_credentialless_endpoint(Some(&base_url))?;
        }
        let key = credential.into_optional_api_key(definition.id())?;
        authorization::validate_api_key(key.as_deref())?;
        Ok(Self {
            base_url,
            api: definition.dictation,
            auth: key.map(ApiKeyAuthorization::new),
            headers: definition.headers,
            settings: *transport,
        })
    }

    /// Delivery mode negotiated for this model and endpoint.
    #[must_use]
    pub fn mode(&self, model: &str) -> DictationMode {
        mode(self.api, &self.base_url, model)
    }

    /// Transcribes one recording or live stream, stopping when the gateway revokes access.
    ///
    /// # Errors
    /// Returns validation, credential, connection, or provider errors.
    pub async fn transcribe_with_lifetime(
        &self,
        lifetime: &ModelCredentialLifetime,
        model: &str,
        language: Option<&DictationLanguage>,
        audio: DictationAudio,
        updates: mpsc::Sender<String>,
    ) -> Result<String> {
        while_valid(lifetime, None, || {
            self.transcribe(model, language, audio, updates)
        })
        .await
    }

    /// Transcribes one stream of mono signed little-endian PCM16 at 24 kHz.
    /// Closing the input finishes the utterance. Updates are cumulative, and the return value is final.
    ///
    /// # Errors
    /// Returns errors for invalid input, failed requests or an interrupted provider.
    pub async fn transcribe(
        &self,
        model: &str,
        language: Option<&DictationLanguage>,
        audio: DictationAudio,
        updates: mpsc::Sender<String>,
    ) -> Result<String> {
        let language = language.map(DictationLanguage::as_str);
        match self.mode(model) {
            DictationMode::Live => self.live(model, language, audio, updates).await,
            DictationMode::AfterStop => self.recording(model, language, audio).await,
        }
    }

    async fn recording(
        &self,
        model: &str,
        language: Option<&str>,
        mut audio: DictationAudio,
    ) -> Result<String> {
        use crate::backend::model::authorization::OpenAiAuthorization as _;
        use std::io::Write as _;
        const HEADER_BYTES: usize = 44;
        const MAX_WAV_BYTES: usize = MAX_RECORDING_BYTES + HEADER_BYTES;
        // Reserve the RIFF header in place so finalizing a long recording does not copy the audio.
        let mut wav = vec![0; HEADER_BYTES];
        while let Some(chunk) = audio.0.recv().await {
            let needed = wav.len() + chunk.len();
            if needed > wav.capacity() {
                let capacity = (wav.capacity() * 2).clamp(needed, MAX_WAV_BYTES);
                wav.try_reserve_exact(capacity - wav.len())
                    .map_err(|error| Error::Internal(error.to_string()))?;
            }
            wav.extend_from_slice(&chunk);
        }
        let total = wav.len() - HEADER_BYTES;
        if total == 0 {
            return Ok(String::new());
        }
        let size = u32::try_from(total)
            .map_err(|_overflow| Error::Invalid("recording is too long".into()))?;
        let mut header = Cursor::new(wav.as_mut_slice());
        for field in [
            b"RIFF".as_slice(),
            &(size + 36).to_le_bytes(),
            b"WAVEfmt ",
            &16_u32.to_le_bytes(),
            &1_u16.to_le_bytes(),
            &1_u16.to_le_bytes(),
            &24_000_u32.to_le_bytes(),
            &48_000_u32.to_le_bytes(),
            &2_u16.to_le_bytes(),
            &16_u16.to_le_bytes(),
            b"data",
            &size.to_le_bytes(),
        ] {
            header
                .write_all(field)
                .map_err(|e| Error::Internal(e.to_string()))?;
        }
        let part = Part::bytes(wav)
            .file_name("dictation.wav")
            .mime_str("audio/wav")
            .map_err(|error| Error::Internal(error.to_string()))?;
        let mut form = Form::new()
            .text("model", model.to_owned())
            .part("file", part);
        if let Some(language) = language {
            form = form.text("language", language.to_owned());
        }
        let path = match self.api {
            Some(api) => api.recording.as_str(),
            None => endpoints::default_path(EndpointApi::Transcriptions)?,
        };
        let client = self.settings.streaming_client()?;
        let request = client
            .post(self.base_url.operation_url(path))
            .multipart(form)
            .timeout(Duration::from_millis(self.settings.http_idle_timeout_ms));
        let authorization = match &self.auth {
            Some(auth) => Some(auth.authorize_http(false, None).await?),
            None => None,
        };
        let request = transport::provider_request(
            request,
            Some(self.headers),
            authorization.as_ref().map(|auth| auth.token.as_ref()),
        );
        let response = transport::send_checked(request, "dictation").await?;
        let bytes = transport::read_limited(response, MAX_TRANSCRIPT_BYTES, "dictation").await?;
        let transcript: Transcript = serde_json::from_slice(&bytes)
            .map_err(|_json| Error::Provider("invalid dictation transcript response".into()))?;
        Ok(transcript.text)
    }
}

#[derive(Deserialize)]
struct Transcript {
    text: String,
}
