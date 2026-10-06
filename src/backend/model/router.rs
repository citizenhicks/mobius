//! Stable model route selection and route diagnostics.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::SystemTime;

use super::CompactOutput;
use super::CompactRequest;
use super::GeneratedImage;
use super::ImageGenerationRequest;
use super::Model;
use super::ModelEventSink;
use super::ModelOutput;
use super::ModelRequest;
use super::PromptCacheMode;
use super::ToolDefinition;
use super::has_prompt_cache_breakpoint;
use super::mark_prompt_cache_breakpoint;
use crate::Error;
use crate::Result;
use crate::protocol::ModelChoice;
use crate::protocol::ModelStepDiagnostics;
use crate::protocol::PromptCacheDiagnostics;
use crate::protocol::TokenUsage;
use crate::protocol::ToolDiscoveryMode;

/// Selects a model Adapter by a stable provider ID.
pub struct ModelRouter {
    default: String,
    routes: Vec<ModelRoute>,
    images: Vec<MediaRoute>,
    voices: Vec<MediaRoute>,
    files: Option<crate::backend::session_files::SessionFileStore>,
    image_limits: super::ImageInputLimits,
}

struct ModelRoute {
    choice: ModelChoice,
    provider: Arc<dyn Model>,
    credential: ModelCredentialLifetime,
}

/// An image or voice model served through a registered model route's transport.
struct MediaRoute {
    choice: ModelChoice,
    transport: String,
}

/// One configured image route with the provider model and quality it calls.
#[derive(Debug, Clone, Copy)]
pub struct ImageModel<'a> {
    /// The image route.
    pub route: &'a str,
    /// The provider image model.
    pub model: &'a str,
    /// The provider quality level, or `None` for its default.
    pub quality: Option<&'a str>,
}

fn select_media<'a>(
    routes: &'a [MediaRoute],
    route: Option<&str>,
    kind: &str,
) -> Result<&'a MediaRoute> {
    match route {
        Some(route) => routes
            .iter()
            .find(|media| media.choice.route == route)
            .ok_or_else(|| Error::Unknown(format!("{kind} model `{route}`"))),
        None => routes
            .first()
            .ok_or_else(|| Error::Config(format!("no {kind} model is configured"))),
    }
}

impl ModelRouter {
    /// Creates a router with its first provider.
    pub fn new(id: impl Into<String>, provider: Arc<dyn Model>) -> Self {
        let id = id.into();
        let choice = inferred_choice(&id, provider.as_ref());
        Self {
            default: id,
            files: None,
            image_limits: super::ImageInputLimits::default(),
            images: Vec::new(),
            voices: Vec::new(),
            routes: vec![ModelRoute {
                choice,
                provider,
                credential: ModelCredentialLifetime::default(),
            }],
        }
    }

    /// Injects the same durable file store used by image-producing capabilities.
    #[must_use]
    pub fn session_files(mut self, files: crate::backend::session_files::SessionFileStore) -> Self {
        self.files = Some(files);
        self
    }

    /// Sets request image limits independently from file storage limits.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn image_input_limits(mut self, limits: super::ImageInputLimits) -> Result<Self> {
        if limits.max_images == 0 || limits.max_encoded_bytes == 0 {
            return Err(Error::Config(
                "image request limits must be positive".into(),
            ));
        }
        self.image_limits = limits;
        Ok(self)
    }

    /// Returns operational policy for the selected route.
    /// # Errors
    /// Returns an error if the route is unknown.
    pub fn transport_settings_for(&self, route: &str) -> Result<super::ModelTransportSettings> {
        Ok(self.provider(route)?.transport_settings())
    }

    /// Reports whether the route accepts images associated with a tool call.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn supports_tool_image_input(&self, provider: &str) -> Result<bool> {
        Ok(self.provider(provider)?.supports_tool_image_input())
    }

    /// Registers another provider.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn register(&mut self, id: impl Into<String>, provider: Arc<dyn Model>) -> Result<()> {
        let id = id.into();
        if self.routes.iter().any(|route| route.choice.route == id) {
            return Err(Error::Duplicate(format!("model provider `{id}`")));
        }
        self.routes.push(ModelRoute {
            choice: inferred_choice(&id, provider.as_ref()),
            provider,
            credential: ModelCredentialLifetime::default(),
        });
        Ok(())
    }

    /// Registers an image model served by the transport of model route `transport`.
    /// # Errors
    ///
    /// Returns an error if the transport is unknown, cannot generate images, or the choice repeats.
    pub fn register_image(
        &mut self,
        transport: impl Into<String>,
        choice: ModelChoice,
    ) -> Result<()> {
        let transport = transport.into();
        if !self.provider(&transport)?.supports_image_generation() {
            return Err(Error::Config(format!(
                "model route `{transport}` cannot generate images"
            )));
        }
        Self::push_media(&mut self.images, transport, choice)
    }

    /// Registers a voice served by the transport of model route `transport`.
    /// # Errors
    ///
    /// Returns an error if the transport is unknown, lacks realtime voice, or the choice repeats.
    pub fn register_voice(
        &mut self,
        transport: impl Into<String>,
        choice: ModelChoice,
    ) -> Result<()> {
        let transport = transport.into();
        if !self.provider(&transport)?.supports_realtime_voice()
            || choice.reasoning_effort.is_none()
        {
            return Err(Error::Config(format!(
                "model route `{transport}` cannot serve voice `{}`",
                choice.route
            )));
        }
        Self::push_media(&mut self.voices, transport, choice)
    }

    fn push_media(
        routes: &mut Vec<MediaRoute>,
        transport: String,
        choice: ModelChoice,
    ) -> Result<()> {
        if routes
            .iter()
            .any(|media| media.choice.route == choice.route)
        {
            return Err(Error::Duplicate(format!("media route `{}`", choice.route)));
        }
        routes.push(MediaRoute { choice, transport });
        Ok(())
    }

    /// Returns the configured image models in display order.
    pub fn image_choices(&self) -> impl ExactSizeIterator<Item = &ModelChoice> {
        self.images.iter().map(|media| &media.choice)
    }

    /// Returns the configured voices in display order.
    pub fn voice_choices(&self) -> impl ExactSizeIterator<Item = &ModelChoice> {
        self.voices.iter().map(|media| &media.choice)
    }

    /// Cancels paid operations when their credential expires or is revoked.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn set_credential_lifetime(
        &mut self,
        id: &str,
        credential: ModelCredentialLifetime,
    ) -> Result<()> {
        let route = self
            .routes
            .iter_mut()
            .find(|route| route.choice.route == id)
            .ok_or_else(|| Error::Unknown(format!("model provider `{id}`")))?;
        route.credential = credential;
        Ok(())
    }

    /// Returns the selectable routes in frontend display order.
    pub fn choices(
        &self,
    ) -> impl DoubleEndedIterator<Item = &ModelChoice> + ExactSizeIterator + Clone {
        self.routes.iter().map(|route| &route.choice)
    }

    /// Resolves one route and optional reasoning effort through the model catalog.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn resolve_choice(
        &self,
        route: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<&ModelChoice> {
        let choice = self
            .choices()
            .find(|choice| choice.route == route)
            .ok_or_else(|| Error::Unknown(format!("model route `{route}`")))?;
        let Some(reasoning_effort) = reasoning_effort else {
            return Ok(choice);
        };
        self.choices()
            .find(|candidate| {
                candidate.group == choice.group
                    && candidate.reasoning_effort.as_deref() == Some(reasoning_effort)
            })
            .ok_or_else(|| {
                Error::Unknown(format!(
                    "reasoning effort `{reasoning_effort}` for model route `{route}`"
                ))
            })
    }

    /// Replaces display metadata for one registered route.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn configure_choice(&mut self, mut choice: ModelChoice) -> Result<()> {
        if choice.group.trim().is_empty() || choice.model.trim().is_empty() {
            return Err(Error::Config(
                "model choice group and model cannot be empty".into(),
            ));
        }
        if choice.context_window.is_some_and(|window| window <= 0) {
            return Err(Error::Config(
                "model choice context window must be positive".into(),
            ));
        }
        let current = self
            .routes
            .iter_mut()
            .find(|current| current.choice.route == choice.route)
            .ok_or_else(|| Error::Unknown(format!("model route `{}`", choice.route)))?;
        choice.supports_image_input = current.provider.supports_image_input();
        choice.supports_image_generation = current.provider.supports_image_generation();
        choice.supports_realtime_voice = current.provider.supports_realtime_voice();
        choice.tool_discovery = current.provider.tool_discovery();
        current.choice = choice;
        Ok(())
    }

    /// Returns the default provider ID.
    #[must_use]
    pub fn default_provider(&self) -> &str {
        &self.default
    }

    /// Streams one response through the selected provider.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn respond(
        &self,
        provider: &str,
        request: ModelRequest<'_>,
        events: ModelEventSink,
    ) -> Result<ModelOutput> {
        let route = self.route(provider)?;
        let media = super::MediaPreparation {
            files: self.files.as_ref(),
            limits: self.image_limits,
        };
        while_valid(&route.credential, || {
            route.provider.respond_prepared(request, events, media)
        })
        .await
    }

    /// Activates a session's fallback transport after safe retries are exhausted.
    /// # Errors
    ///
    /// Returns an error if the route is unknown or the provider cannot switch transports.
    pub async fn fallback_transport(&self, provider: &str, session_id: &str) -> Result<bool> {
        self.provider(provider)?
            .fallback_transport(session_id)
            .await
    }

    /// Validates media before an active-context rewrite is committed.
    /// # Errors
    ///
    /// Returns an error if the supplied value is invalid.
    pub async fn validate_media(
        &self,
        provider: &str,
        session_id: &str,
        input: &[serde_json::Value],
    ) -> Result<()> {
        super::media::hydrate(
            self.files.as_ref(),
            session_id,
            input,
            self.provider(provider)?,
            0,
        )
        .await
        .map(|_| ())
    }

    /// Reports whether one route has a native compaction endpoint.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn compaction_endpoint(&self, provider: &str) -> Result<bool> {
        Ok(self.provider(provider)?.compaction_endpoint())
    }

    /// Reports whether one route accepts native image input.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn supports_image_input(&self, provider: &str) -> Result<bool> {
        Ok(self.provider(provider)?.supports_image_input())
    }

    /// Resolves the selected image route, or the first configured one, with its provider model
    /// and quality.
    /// # Errors
    ///
    /// Returns an error when no image model matches.
    pub fn image_model(&self, image_route: Option<&str>) -> Result<ImageModel<'_>> {
        select_media(&self.images, image_route, "image").map(|media| ImageModel {
            route: &media.choice.route,
            model: &media.choice.model,
            quality: media.choice.reasoning_effort.as_deref(),
        })
    }

    /// Resolves the selected voice, or the first configured one, with its transport route.
    /// # Errors
    ///
    /// Returns an error when no voice matches.
    pub fn voice_choice(&self, voice_route: Option<&str>) -> Result<(&ModelChoice, &str)> {
        select_media(&self.voices, voice_route, "voice")
            .map(|media| (&media.choice, media.transport.as_str()))
    }

    /// Generates or edits an image with `request.model` through image route `image_route`.
    /// # Errors
    ///
    /// Returns an error when the route is unknown, or input, authorization, or the provider
    /// response is invalid.
    pub async fn generate_image(
        &self,
        image_route: &str,
        request: ImageGenerationRequest<'_>,
    ) -> Result<GeneratedImage> {
        let media = select_media(&self.images, Some(image_route), "image")?;
        if media.choice.model != request.model
            || media.choice.reasoning_effort.as_deref() != request.quality
        {
            return Err(Error::Config(format!(
                "image route `{image_route}` does not serve model `{}`",
                request.model
            )));
        }
        request.validate(self.image_limits)?;
        let route = self.route(&media.transport)?;
        let transport = route.provider.transport_settings();
        let request_id = uuid::Uuid::new_v4().to_string();
        while_valid(&route.credential, || async {
            let mut retries = 0;
            loop {
                match route.provider.generate_image(request).await {
                    Err(Error::Provider(error))
                        if error.is_retryable() && retries < transport.stream_retry_limit =>
                    {
                        let delay = super::transport::retry_delay(
                            &error,
                            retries as usize,
                            &request_id,
                            &transport,
                        );
                        retries += 1;
                        tokio::time::sleep(delay).await;
                    }
                    result => return result,
                }
            }
        })
        .await
    }

    /// Starts a provider-owned voice call with the selected voice, or the first one.
    /// # Errors
    ///
    /// Returns an error if no voice matches, or validation or an operation required by this
    /// function fails.
    pub async fn start_realtime_voice(
        &self,
        voice_route: Option<&str>,
        mut request: super::RealtimeVoiceRequest,
    ) -> Result<super::RealtimeVoiceCall> {
        let media = select_media(&self.voices, voice_route, "voice")?;
        // ponytail: owned copies, the request outlives the router borrow inside the provider.
        request.model = Some(media.choice.model.as_str().into());
        request.voice = media.choice.reasoning_effort.as_deref().map(Into::into);
        let route = self.route(&media.transport)?;
        let settings = self.transport_settings_for(&media.transport)?;
        let mut credential = route.credential.clone();
        credential.expires_at = credential.expires_at.map(|expires_at| {
            super::RealtimeVoiceCall::cleanup_deadline(
                expires_at,
                std::time::Duration::from_millis(settings.voice_io_timeout_ms),
            )
        });
        let mut call =
            while_valid(&credential, || route.provider.start_realtime_voice(request)).await?;
        call.limit_credential(credential);
        Ok(call)
    }

    /// Reports deferred-tool cache behavior for one route.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn tool_discovery(&self, provider: &str) -> Result<ToolDiscoveryMode> {
        Ok(self.provider(provider)?.tool_discovery())
    }

    /// Prepares the provider-owned direct/deferred tool envelope for one request.
    pub(crate) fn prepare_tool_definitions(
        &self,
        provider: &str,
        mut direct: Vec<ToolDefinition>,
        deferred: Vec<ToolDefinition>,
        materialized: &BTreeSet<String>,
    ) -> Result<(Vec<ToolDefinition>, Vec<ToolDefinition>)> {
        match self.provider(provider)?.tool_discovery() {
            ToolDiscoveryMode::Native => Ok((direct, deferred)),
            ToolDiscoveryMode::Rebuild => {
                direct.extend(
                    deferred
                        .iter()
                        .filter(|tool| materialized.contains(&tool.name))
                        .cloned(),
                );
                Ok((direct, Vec::new()))
            }
        }
    }

    /// Applies transport-owned metadata to the first input of a new turn.
    pub(crate) fn prepare_turn_input(
        &self,
        context: &[serde_json::Value],
        input: &mut serde_json::Value,
    ) {
        if !has_prompt_cache_breakpoint(context) {
            let _ = mark_prompt_cache_breakpoint(input);
        }
    }

    /// Reports prompt-cache support for one route.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn prompt_cache_capability(&self, provider: &str) -> Result<PromptCacheMode> {
        Ok(self.provider(provider)?.prompt_cache_capability())
    }

    pub(crate) fn model_step_diagnostics(
        &self,
        provider: &str,
        context_epoch: u64,
        rewrite_reasons: Vec<String>,
        usage: &TokenUsage,
    ) -> Result<ModelStepDiagnostics> {
        let model = self.provider(provider)?;
        let capability = model.prompt_cache_capability();
        Ok(ModelStepDiagnostics {
            provider: provider.into(),
            prompt_cache: PromptCacheDiagnostics {
                capability,
                context_epoch,
                outcome: capability.outcome(usage, !rewrite_reasons.is_empty()),
                rewrite_reasons,
            },
        })
    }

    /// Compacts context through the selected provider.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub async fn compact(
        &self,
        provider: &str,
        request: CompactRequest<'_>,
    ) -> Result<CompactOutput> {
        let route = self.route(provider)?;
        let media = super::MediaPreparation {
            files: self.files.as_ref(),
            limits: self.image_limits,
        };
        while_valid(&route.credential, || {
            route.provider.compact_prepared(request, media)
        })
        .await
    }

    fn provider(&self, id: &str) -> Result<&dyn Model> {
        Ok(self.route(id)?.provider.as_ref())
    }

    fn route(&self, id: &str) -> Result<&ModelRoute> {
        self.routes
            .iter()
            .find(|route| route.choice.route == id)
            .ok_or_else(|| Error::Unknown(format!("model provider `{id}`")))
    }
}

/// The absolute deadline and revocation signal for a model credential.
/// Dropping the signal's sender revokes every route and call holding a receiver.
#[derive(Clone, Default)]
pub struct ModelCredentialLifetime {
    /// Last instant at which paid operations are allowed.
    pub expires_at: Option<SystemTime>,
    /// A credential owner closes or changes this channel on revocation.
    pub revoked: Option<tokio::sync::watch::Receiver<()>>,
}

impl ModelCredentialLifetime {
    pub(super) async fn ended(mut self) {
        let expiry = async {
            match self.expires_at {
                Some(deadline) => {
                    tokio::time::sleep(
                        deadline
                            .duration_since(SystemTime::now())
                            .unwrap_or_default(),
                    )
                    .await
                }
                None => std::future::pending().await,
            }
        };
        let revoked = async {
            match self.revoked.as_mut() {
                Some(revoked) => {
                    let _ = revoked.changed().await;
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! { _ = expiry => {}, _ = revoked => {} }
    }

    fn is_valid(&self) -> bool {
        self.expires_at
            .is_none_or(|deadline| deadline > SystemTime::now())
            && self
                .revoked
                .as_ref()
                .is_none_or(|revoked| matches!(revoked.has_changed(), Ok(false)))
    }
}

async fn while_valid<T, F: Future<Output = Result<T>>>(
    credential: &ModelCredentialLifetime,
    operation: impl FnOnce() -> F,
) -> Result<T> {
    if !credential.is_valid() {
        return Err(expired_credential());
    }
    tokio::select! {
        biased;
        _ = credential.clone().ended() => Err(expired_credential()),
        result = async { operation().await } => result,
    }
}

fn expired_credential() -> Error {
    Error::Provider(crate::ProviderError::http(
        "model credential has expired or been revoked",
        401,
        None,
    ))
}

fn inferred_choice(route: &str, provider: &dyn Model) -> ModelChoice {
    let mut info = provider.info();
    if info.model.is_empty() {
        info.model = route.to_string();
    }
    ModelChoice {
        route: route.to_string(),
        group: route.to_string(),
        model: info.model,
        reasoning_effort: info.reasoning_effort,
        variant_label: None,
        context_window: None,
        supports_image_input: provider.supports_image_input(),
        supports_image_generation: provider.supports_image_generation(),
        supports_realtime_voice: provider.supports_realtime_voice(),
        tool_discovery: provider.tool_discovery(),
    }
}
