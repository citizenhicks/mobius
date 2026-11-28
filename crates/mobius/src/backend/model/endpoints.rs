//! Request formats and endpoint paths for a provider’s model groups.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use super::dictation::DictationMode;
use super::provider::{BaseUrl, ProviderDefinition};
use crate::protocol::{FrontendSymbol, ModelCapability, ToolDiscoveryMode};
use crate::{Error, Result};

/// Model groups exposed in provider configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderModelKind {
    /// Chat inference.
    Language,
    /// Speech transcription.
    Dictation,
    /// Image generation.
    Image,
    /// Voice calls.
    Voice,
    /// Fixed-choice decision inference.
    Decision,
}

/// A compatible request format with its default operation URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelEndpointApi {
    /// Stable wire identifier for the request format.
    pub id: String,
    /// Provider-facing display name.
    pub label: String,
    /// Operation suffix under the provider base URL.
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum EndpointApi {
    Responses,
    ChatCompletions,
    Messages,
    Transcriptions,
    RealtimeTranscription,
    Images,
    VoiceCalls,
    Decisions,
}

impl EndpointApi {
    const fn id(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
            Self::Messages => "messages",
            Self::Transcriptions => "transcriptions",
            Self::RealtimeTranscription => "realtime_transcription",
            Self::Images => "images",
            Self::VoiceCalls => "voice_calls",
            Self::Decisions => "decisions",
        }
    }
}

impl DictationMode {
    /// The audio streaming behavior of an explicitly selected transcription adapter.
    #[must_use]
    pub fn for_endpoint_api(api: &str) -> Option<Self> {
        if api == EndpointApi::Transcriptions.id() {
            Some(Self::AfterStop)
        } else if api == EndpointApi::RealtimeTranscription.id() {
            Some(Self::Live)
        } else {
            None
        }
    }
}

/// Presentation metadata shared by every provider.
#[derive(Debug, Deserialize)]
#[non_exhaustive]
pub struct ModelGroupDefaults {
    /// Semantic icon used by settings clients.
    pub symbol: FrontendSymbol,
    /// Model category.
    pub kind: ProviderModelKind,
    /// Section title.
    pub label: String,
}

#[derive(Deserialize)]
struct Api {
    label: String,
    path: Option<String>,
}
#[derive(Deserialize)]
struct Defaults {
    groups: Vec<ModelGroupDefaults>,
    apis: BTreeMap<EndpointApi, Api>,
}
static DEFAULTS: LazyLock<Defaults> =
    LazyLock::new(|| crate::config::embedded(include_str!("endpoints.toml")));

/// Returns the order and field schema for provider model groups.
#[must_use]
pub fn model_group_defaults() -> &'static [ModelGroupDefaults] {
    &DEFAULTS.groups
}

pub(super) fn default_path(id: EndpointApi) -> Result<&'static str> {
    DEFAULTS
        .apis
        .get(&id)
        .and_then(|api| api.path.as_deref())
        .ok_or_else(|| Error::Internal(format!("missing bundled path for {id:?}")))
}

struct AdapterEndpoint {
    api: EndpointApi,
    label: &'static str,
    path: &'static str,
    edit_path: Option<&'static str>,
}

fn operation(id: EndpointApi, path: Option<&'static str>) -> Option<AdapterEndpoint> {
    let entry = DEFAULTS.apis.get(&id)?;
    Some(AdapterEndpoint {
        api: id,
        label: &entry.label,
        path: path.or(entry.path.as_deref())?,
        edit_path: None,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Operation {
    pub(super) provider: &'static str,
    pub(super) dictation_mode: Option<DictationMode>,
    pub(super) default_path: &'static str,
    pub(super) edit_path: Option<&'static str>,
    pub(super) url: String,
    pub(super) compatible_chat: bool,
}

impl ProviderDefinition {
    fn operations(&self, kind: ProviderModelKind, base: &BaseUrl) -> Vec<AdapterEndpoint> {
        let at = Some(base.as_str());
        let op = match kind {
            ProviderModelKind::Language => {
                return self
                    .language_apis
                    .iter()
                    .filter_map(|a| operation(*a, None))
                    .collect();
            }
            ProviderModelKind::Image
                if self.can_configure_at(ModelCapability::ImageGeneration, at) =>
            {
                self.image_endpoint
                    .and_then(|id| super::image_generation::image_api(id).ok())
                    .and_then(|a| {
                        Some(AdapterEndpoint {
                            edit_path: Some(&a.edit_path),
                            ..operation(EndpointApi::Images, Some(&a.generation_path))?
                        })
                    })
            }
            ProviderModelKind::Voice
                if self.can_configure_at(ModelCapability::RealtimeVoice, at) =>
            {
                self.voice_endpoint
                    .and_then(|a| operation(EndpointApi::VoiceCalls, Some(a.calls_path())))
            }
            ProviderModelKind::Decision if self.supports_at(ModelCapability::Decisions, at) => {
                operation(EndpointApi::Decisions, self.decisions_path)
            }
            ProviderModelKind::Dictation if self.can_configure_dictation(at) => {
                return [
                    operation(
                        EndpointApi::Transcriptions,
                        self.dictation.map(|d| d.recording.as_str()),
                    ),
                    self.dictation
                        .and_then(|d| d.realtime.as_deref())
                        .and_then(|path| operation(EndpointApi::RealtimeTranscription, Some(path))),
                ]
                .into_iter()
                .flatten()
                .collect();
            }
            _ => None,
        };
        op.into_iter().collect()
    }

    /// Lists the API choices for a group, with suffixes under the single provider base URL.
    #[must_use]
    pub fn group_endpoint_options(
        &self,
        kind: ProviderModelKind,
        base: &BaseUrl,
    ) -> Vec<ModelEndpointApi> {
        self.operations(kind, base)
            .into_iter()
            .map(|op| ModelEndpointApi {
                id: op.api.id().into(),
                label: op.label.into(),
                path: op.path.split('?').next().unwrap_or(op.path).into(),
            })
            .collect()
    }

    /// Returns a discovery mode required by a group's API adapter.
    #[must_use]
    pub fn group_api_tool_discovery(
        &self,
        kind: ProviderModelKind,
        api: &str,
    ) -> Option<ToolDiscoveryMode> {
        (kind == ProviderModelKind::Language
            && api == EndpointApi::ChatCompletions.id()
            && self.language_apis.first() == Some(&EndpointApi::Responses)
            && self.language_apis.contains(&EndpointApi::ChatCompletions))
        .then_some(ToolDiscoveryMode::Rebuild)
    }

    /// Binds an API path suffix to the group's provider base, preserving the credential scope.
    /// # Errors
    /// Rejects full URLs, parent directories, unsupported APIs, and invalid path characters.
    pub fn with_group_endpoint(
        &self,
        kind: ProviderModelKind,
        base: &BaseUrl,
        api: &str,
        path: &str,
    ) -> Result<BaseUrl> {
        let op = self
            .operations(kind, base)
            .into_iter()
            .find(|op| op.api.id() == api)
            .ok_or_else(|| Error::Invalid("incompatible model group API".into()))?;
        let suffix = path.strip_prefix('/').unwrap_or(path);
        if suffix.len() > 2048
            || suffix.split('/').any(|s| {
                s.is_empty()
                    || matches!(s, "." | "..")
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._~:@".contains(&b))
            })
        {
            return Err(Error::Invalid(
                "enter an endpoint path under the provider base URL".into(),
            ));
        }
        let mut endpoint = format!("{}/{suffix}", base.root());
        if let Some((_, params)) = op.path.split_once('?') {
            endpoint.push('?');
            endpoint.push_str(params);
        }
        let compatible_chat = self.group_api_tool_discovery(kind, api).is_some();
        Ok(base.with_operation(Arc::new(Operation {
            provider: self.id(),
            // The native recording path leaves each model's manifest mode in effect.
            dictation_mode: DictationMode::for_endpoint_api(api)
                .filter(|mode| *mode != DictationMode::AfterStop || suffix != op.path),
            default_path: op.path,
            edit_path: op.edit_path,
            url: endpoint,
            compatible_chat,
        })))
    }
}
