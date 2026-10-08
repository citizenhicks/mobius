//! Stable model route selection and route diagnostics.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::SystemTime;

use super::GeneratedImage;
use super::ImageGenerationRequest;
use super::Model;
use super::ModelCancellation;
use super::ModelCancellationReason;
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

type ToolDefinitions = Vec<Arc<ToolDefinition>>;

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
    context_group: Option<String>,
    provider: Arc<dyn Model>,
    credential: ModelCredentialLifetime,
}

/// An independently registered image or voice provider.
struct MediaRoute {
    choice: ModelChoice,
    provider: Arc<dyn Model>,
    credential: ModelCredentialLifetime,
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
                context_group: None,
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
            context_group: None,
            provider,
            credential: ModelCredentialLifetime::default(),
        });
        Ok(())
    }

    /// Registers an image model independently of the chat catalog.
    /// # Errors
    ///
    /// Returns an error if the provider cannot generate images or the choice repeats.
    pub fn register_image(
        &mut self,
        provider: Arc<dyn Model>,
        choice: ModelChoice,
        credential: ModelCredentialLifetime,
    ) -> Result<()> {
        if !provider.supports_image_generation() {
            return Err(Error::Config(format!(
                "provider cannot generate images for `{}`",
                choice.route
            )));
        }
        Self::push_media(&mut self.images, provider, choice, credential)
    }

    /// Registers a voice independently of the chat catalog.
    /// # Errors
    ///
    /// Returns an error if the provider lacks realtime voice, the voice is missing, or the choice repeats.
    pub fn register_voice(
        &mut self,
        provider: Arc<dyn Model>,
        choice: ModelChoice,
        credential: ModelCredentialLifetime,
    ) -> Result<()> {
        if !provider.supports_realtime_voice() || choice.reasoning_effort.is_none() {
            return Err(Error::Config(format!(
                "provider cannot serve voice `{}`",
                choice.route
            )));
        }
        Self::push_media(&mut self.voices, provider, choice, credential)
    }

    fn push_media(
        routes: &mut Vec<MediaRoute>,
        provider: Arc<dyn Model>,
        choice: ModelChoice,
        credential: ModelCredentialLifetime,
    ) -> Result<()> {
        if routes
            .iter()
            .any(|media| media.choice.route == choice.route)
        {
            return Err(Error::Duplicate(format!("media route `{}`", choice.route)));
        }
        routes.push(MediaRoute {
            choice,
            provider,
            credential,
        });
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
                self.same_context_model(&candidate.route, &choice.route)
                    && candidate.reasoning_effort.as_deref() == Some(reasoning_effort)
            })
            .ok_or_else(|| {
                Error::Unknown(format!(
                    "reasoning effort `{reasoning_effort}` for model route `{route}`"
                ))
            })
    }

    /// Whether two registered routes share a model's replay context.
    /// Reasoning variants within the same registered context group remain compatible;
    /// a removed route or a different group/model requires a context handoff.
    #[must_use]
    pub fn same_context_model(&self, source: &str, target: &str) -> bool {
        match (self.route(source), self.route(target)) {
            (Ok(source), Ok(target)) => {
                source.choice.route == target.choice.route
                    || source.context_group.as_ref().is_some_and(|group| {
                        target.context_group.as_ref() == Some(group)
                            && source.choice.model == target.choice.model
                    })
            }
            _ => false,
        }
    }

    /// Associates reasoning variants with one stable provider-instance context owner.
    /// Display labels are deliberately excluded from replay compatibility.
    /// # Errors
    /// Returns an error if the route is unknown or the group is empty.
    pub fn set_context_group(&mut self, route: &str, group: impl Into<String>) -> Result<()> {
        let group = group.into();
        if group.trim().is_empty() {
            return Err(Error::Config("model context group cannot be empty".into()));
        }
        let route = self
            .routes
            .iter_mut()
            .find(|entry| entry.choice.route == route)
            .ok_or_else(|| Error::Unknown(format!("model route `{route}`")))?;
        route.context_group = Some(group);
        Ok(())
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
        // Credential expiry belongs to this attempt, not a subsequent fallback in its parent.
        let parent = request.cancellation;
        let cancellation = ModelCancellation::default();
        let request = ModelRequest {
            cancellation: Some(&cancellation),
            ..request
        };
        let response = while_valid(&route.credential, Some(&cancellation), || {
            route.provider.respond_prepared(request, events, media)
        });
        tokio::pin!(response);
        // This guard must drop before the pinned response reads its local cancellation reason.
        let _inheritance = cancellation.inherit_on_drop(parent);
        response.await
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
        input: super::ModelInput<'_>,
    ) -> Result<()> {
        super::MediaPreparation {
            files: self.files.as_ref(),
            limits: self.image_limits,
        }
        .prepare(session_id, input, self.provider(provider)?, false)
        .await
        .map(|_| ())
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

    /// Resolves the selected voice, or the first configured one.
    /// # Errors
    ///
    /// Returns an error when no voice matches.
    pub fn voice_choice(&self, voice_route: Option<&str>) -> Result<&ModelChoice> {
        select_media(&self.voices, voice_route, "voice").map(|media| &media.choice)
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
        // A rejected image response does not prove generation was uncharged.
        while_valid(&media.credential, None, || {
            media.provider.generate_image(request)
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
        let settings = media.provider.transport_settings();
        // Voice cleanup uses an earlier deadline without changing the route's shared credential lifetime.
        let mut credential = media.credential.clone();
        credential.expires_at = credential.expires_at.map(|expires_at| {
            super::RealtimeVoiceCall::cleanup_deadline(
                expires_at,
                std::time::Duration::from_millis(settings.voice_io_timeout_ms),
            )
        });
        let mut call = while_valid(&credential, None, || {
            media.provider.start_realtime_voice(request)
        })
        .await?;
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
        mut direct: ToolDefinitions,
        deferred: ToolDefinitions,
        materialized: &BTreeSet<String>,
    ) -> Result<(ToolDefinitions, ToolDefinitions)> {
        match self.provider(provider)?.tool_discovery() {
            ToolDiscoveryMode::Native => Ok((direct, deferred)),
            ToolDiscoveryMode::Rebuild => {
                direct.extend(
                    deferred
                        .into_iter()
                        .filter(|tool| materialized.contains(&tool.name)),
                );
                Ok((direct, Vec::new()))
            }
        }
    }

    /// Applies transport-owned metadata to the first input of a new turn.
    pub(crate) fn prepare_turn_input(
        &self,
        context: super::ModelInput<'_>,
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
    cancellation: Option<&ModelCancellation>,
    operation: impl FnOnce() -> F,
) -> Result<T> {
    let expired = || {
        if let Some(cancellation) = cancellation {
            cancellation.record(ModelCancellationReason::CredentialExpired);
        }
        Err(expired_credential())
    };
    if !credential.is_valid() {
        return expired();
    }
    let operation = operation();
    // Keep the actual future alive until its drop guard can read the recorded reason.
    tokio::pin!(operation);
    tokio::select! {
        biased;
        // Each operation needs its own watch cursor to observe revocation independently.
        _ = credential.clone().ended() => expired(),
        result = operation.as_mut() => result,
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
