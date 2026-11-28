//! Runs the advertised settings against the provider, without saving the form or creating a chat.

use super::rejection::{invalid_config, provider_failure_reason};
use super::{GatewayHost, Rejection};
use crate::auth::ProviderAccess;
use crate::harness::mobius::assembly::{RouteValue, build_route, resolve_credential};
use crate::harness::mobius::provider_catalog::{catalog_routes, image_defaults, media_routes};
use crate::wire::{ProviderEndpointAuth, ProviderForm, ProviderModelKind};
use mobius::backend::model::decisions::{DecisionQuestion, DecisionRequest, decide};
use mobius::backend::model::dictation::{
    DictationInput, DictationLanguage, DictationMode, DictationProvider, MAX_CHUNK_BYTES,
};
use mobius::backend::model::provider::{ProviderCredential, provider};
use mobius::backend::model::{
    ImageDefaults, ImageGenerationRequest, ModelCredentialLifetime, ModelRequest, ModelRouter,
    RealtimeVoiceCommand, user_message,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

impl GatewayHost {
    pub(crate) async fn test_provider_group(
        &self,
        access: ProviderAccess,
        mut form: ProviderForm,
        group: ProviderModelKind,
        api_key: Option<String>,
    ) -> Result<(), Rejection> {
        use futures_util::TryStreamExt as _;

        form.groups.retain(|kind, _| *kind == group);
        let snapshot = self.prepare_provider_form(access, form).await?;
        let (instance, configured) = snapshot
            .configured_providers
            .first_key_value()
            .ok_or_else(|| invalid_config("Unknown provider instance"))?;
        let selection = &configured.selection;
        let definition = provider(&selection.provider).map_err(invalid_config)?;
        let mut routes = if group == ProviderModelKind::Language {
            catalog_routes(definition, configured, selection)
        } else {
            let media =
                media_routes(&snapshot.configured_providers, None).map_err(invalid_config)?;
            match group {
                ProviderModelKind::Image => media.images,
                ProviderModelKind::Voice => media.voices,
                ProviderModelKind::Decision => media.decisions,
                ProviderModelKind::Dictation => media.dictations,
                ProviderModelKind::Language => unreachable!(),
                _ => return Err(invalid_config("Unsupported model group")),
            }
        };
        if routes.is_empty() {
            return Err(invalid_config(
                "Add a model to this group before testing it",
            ));
        }
        let transport = &snapshot.model_transport;
        let base = definition.resolve_base_url(selection.base_url.as_ref());
        let (credential, lifetime) = if let Some(key) = api_key {
            if configured.operator_managed
                || selection.endpoint_auth == ProviderEndpointAuth::Credentialless
            {
                return Err(invalid_config(
                    "This provider setup does not accept a test API key",
                ));
            }
            crate::config::validate_credential(instance, &selection.provider, &key, None)
                .map_err(invalid_config)?;
            (
                ProviderCredential::ApiKey(key),
                ModelCredentialLifetime::default(),
            )
        } else {
            let state = self.state.lock().await;
            resolve_credential(
                selection,
                definition,
                &base,
                &state.store,
                &state.credentials,
                transport,
            )
            .map_err(|_credential| {
                invalid_config("A credential is required for this provider endpoint")
            })?
        };
        let http = transport.streaming_client().map_err(invalid_config)?;
        if group == ProviderModelKind::Image {
            // The model's default route can be an alias for an explicit quality variant.
            let mut tested = BTreeSet::new();
            routes.retain(|route| {
                let model = &route.choice.model;
                let quality = route
                    .choice
                    .reasoning_effort
                    .as_ref()
                    .map(Arc::clone)
                    .or_else(|| {
                        image_defaults(definition, configured, model)
                            .quality
                            .map(Arc::from)
                    });
                tested.insert((Arc::clone(model), quality))
            });
        }
        let tests = futures_util::stream::iter(routes.into_iter().map(Ok)).try_for_each_concurrent(
            2,
            async |route| {
                let model = Arc::clone(&route.choice.model);
                let effort = route.choice.reasoning_effort.as_deref();
                let label = effort.map_or_else(
                    || model.to_string(),
                    |variant| format!("{model} ({variant})"),
                );
                let defaults = (group == ProviderModelKind::Image)
                    .then(|| image_defaults(definition, configured, &model));
                let test = async {
                    if group == ProviderModelKind::Dictation {
                        let adapter = DictationProvider::new(
                            definition,
                            route.operation_base(definition, base.share())?,
                            credential.clone(), // rules-allow: each model owns an adapter with the submitted credential
                            transport,
                        )?;
                        test_dictation(&adapter, &lifetime, &model).await
                    } else {
                        let built = build_route(
                            route,
                            definition,
                            credential.clone(), // rules-allow: each model owns an adapter with the submitted credential
                            lifetime.clone(), // rules-allow: every independent provider call observes revocation
                            base.share(),
                            &http,
                            transport,
                        )?;
                        test_model(group, built, defaults).await
                    }
                };
                test.await.map_err(|error| {
                    let reason = match error {
                        crate::Error::Invalid(message) => message,
                        error => provider_failure_reason(&error),
                    };
                    invalid_config(format!("{group:?} test failed for {label}: {reason}"))
                })
            },
        );
        tokio::time::timeout(Duration::from_secs(120), tests)
            .await
            .map_err(|_elapsed| invalid_config(format!("{group:?} group test timed out")))?
    }
}

async fn test_model(
    group: ProviderModelKind,
    built: RouteValue,
    defaults: Option<ImageDefaults>,
) -> crate::Result<()> {
    let id = Arc::clone(&built.choice.route);
    let mut router = ModelRouter::new(Arc::clone(&id), Arc::clone(&built.model));
    match group {
        ProviderModelKind::Language => {
            router.set_credential_lifetime(&id, built.lifetime)?;
            let input = [user_message("Reply with the word ready.")];
            let output = router
                .respond(
                    &id,
                    ModelRequest {
                        session_id: None,
                        cancellation: None,
                        prompt_cache: None,
                        instructions: "",
                        input: input.as_slice().into(),
                        catalog_revision: "provider-test",
                        tools: &[],
                        deferred_tools: &[],
                        allow_hosted_tools: true,
                        allow_continuation: false,
                    },
                    Arc::new(|_| Box::pin(async { Ok(()) })),
                )
                .await?;
            require(!output.text().trim().is_empty())
        }
        ProviderModelKind::Image => {
            router.register_image(built.model, built.choice, built.lifetime)?;
            if let Some(defaults) = defaults {
                router.set_image_defaults(Arc::clone(&id), defaults);
            }
            let settings = router.image_model(Some(&id))?;
            let image = router
                .generate_image(
                    &id,
                    ImageGenerationRequest {
                        model: settings.model,
                        size: settings.size,
                        image_aspect: settings.aspect,
                        quality: settings.quality,
                        prompt: "Generate a solid blue image.",
                        references: &[],
                    },
                )
                .await?;
            tokio::task::spawn_blocking(move || image.validate())
                .await
                .map_err(|e| crate::Error::Internal(e.to_string()))??;
            Ok(())
        }
        ProviderModelKind::Voice => {
            router.register_voice(built.model, built.choice, built.lifetime)?;
            let session = mobius::SessionId::new(uuid::Uuid::new_v4().to_string())?;
            let offer = include_str!("provider_test/offer.sdp").replace('\n', "\r\n");
            let mut call = router
                .start_realtime_voice(
                    Some(&id),
                    session,
                    offer,
                    "This is a connection test.".into(),
                )
                .await?;
            call.commands
                .send(RealtimeVoiceCommand::Close)
                .await
                .map_err(|_closed| crate::Error::Invalid("Voice call ended early".into()))?;
            while let Some(event) = call.events.recv().await {
                event?;
            }
            Ok(())
        }
        ProviderModelKind::Decision => {
            let questions = BTreeMap::from([(
                "color",
                DecisionQuestion::new(
                    "What color is the square?",
                    BTreeMap::from([("blue", "Blue"), ("red", "Red")]),
                ),
            )]);
            let answers = decide(
                &*built.model,
                &built.lifetime,
                DecisionRequest {
                    state: "The square is blue.",
                    questions: &questions,
                },
            )
            .await?;
            require(answers.contains_key("color"))
        }
        ProviderModelKind::Dictation => unreachable!(),
        _ => Err(crate::Error::Invalid("Unsupported model group".into())),
    }
}

async fn test_dictation(
    adapter: &DictationProvider,
    lifetime: &ModelCredentialLifetime,
    model: &str,
) -> crate::Result<()> {
    // “The meeting starts at nine”, synthesized with Flite 2.2's cmu_us_slt voice, PCM16 mono 24 kHz.
    let speech = include_bytes!("provider_test/speech.pcm");
    let (mut source, audio) = DictationInput::channel();
    for chunk in speech.chunks(MAX_CHUNK_BYTES) {
        source.try_send(chunk.to_vec())?; // rules-allow: the asynchronous input queue owns each PCM frame
    }
    let (updates, mut partials) = mpsc::channel(2);
    let language = DictationLanguage::new("en".into())?;
    let result = adapter.transcribe_with_lifetime(lifetime, model, Some(&language), audio, updates);
    tokio::pin!(result);
    if adapter.mode(model) == DictationMode::Live {
        let first_partial = async {
            while let Some(text) = partials.recv().await {
                if !text.trim().is_empty() {
                    return Ok(());
                }
            }
            require(false)
        };
        tokio::select! {
            ended = &mut result => { ended?; return require(false); }
            partial = tokio::time::timeout(Duration::from_secs(15), first_partial) => {
                partial.map_err(|_elapsed| crate::Error::Invalid("live transcription returned no text before commit".into()))??;
            }
        }
    }
    drop(source);
    require(!result.await?.trim().is_empty())
}

fn require(valid: bool) -> crate::Result<()> {
    if valid {
        Ok(())
    } else {
        Err(crate::Error::Invalid(
            "Provider returned an incomplete result".into(),
        ))
    }
}
