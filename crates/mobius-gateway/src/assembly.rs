use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;

use mobius::Error as MobiusError;
use mobius::agent::{Agent, AgentConfig, create_agent};
use mobius::backend::checkpoint::CheckpointStore;
use mobius::backend::model::provider::{
    HttpClient, ProviderAuth, ProviderBuildConfig, ProviderCredential, ProviderDefinition, provider,
};
use mobius::backend::model::{
    Model, ModelCredentialLifetime, ModelEventSink, ModelOutput, ModelRequest, ModelRouter,
};
use mobius::backend::sandbox::{ApprovalPolicy, Sandbox, SandboxBackend};
use mobius::backend::session_files::SessionFileStore;
use mobius::middleware::artifacts::Artifacts;
use mobius::middleware::attachments::Attachments;
use mobius::middleware::extensions::{Extensions, MANIFEST as EXTENSIONS_MANIFEST};
use mobius::middleware::image_generation::ImageGeneration;
use mobius::middleware::instructions::Instructions;
use mobius::middleware::messages::Messages;
use mobius::middleware::scratchpad::Scratchpad;
use mobius::middleware::sessions::{LiveChats, Sessions};
use mobius::middleware::subagents::Subagents;
use mobius::middleware::tasks::Tasks;
use mobius::middleware::tools::Tools;
use mobius::middleware::{Middleware, MiddlewareStack};
use mobius::protocol::{ActiveMessageDelivery, ModelChoice, ModelInfo, SessionContext, TokenUsage};

use crate::config::{
    ChatSpec, ConfigStore, CredentialStore, DEFAULT_CONTEXT_WINDOW, GatewayConfig,
    configured_approval_policy, effective_reasoning_effort, local_user_name, model_route_id,
};
use crate::extensions::{ExtensionStore, ResolvedExtensions};
use crate::middleware_manifest::{BuiltinMiddleware, MIDDLEWARE};
use crate::provider_catalog::{
    CatalogRoute, catalog_routes, configured_model_providers, configured_model_routes,
    credential_is_configured,
};
use crate::sandbox::GatewaySandbox;
use crate::wire::{
    AgentComposition, MiddlewareConfig, ProviderConfig, ProviderEndpointAuth,
    RoutineInteractionPolicy, validate_session_id,
};
use crate::{Error, Result};

pub(crate) async fn run_discovery<T, F>(
    discovery_gate: Arc<tokio::sync::Mutex<()>>,
    operation: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let guard = discovery_gate.lock_owned().await;
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        operation()
    })
    .await
    .map_err(|error| Error::Config(format!("agent discovery task failed: {error}")))?
}

pub(crate) struct PreparedBot {
    pub(crate) bot: crate::wire::BotRecord,
    pub(crate) epoch: u64,
    stale: std::sync::atomic::AtomicBool,
    pub(crate) providers: Vec<ProviderConfig>,
    models: Arc<ModelRouter>,
    context_window: i64,
    model_providers: BTreeMap<String, String>,
    approval_policy: ApprovalPolicy,
    pub(crate) active_message_delivery: ActiveMessageDelivery,
    extensions: ResolvedExtensions,
    computer_runtime: Option<crate::computer_runtime::PreparedComputer>,
    computer_config: Arc<crate::computer_runtime::ComputerConfig>,
    subagent_ceilings: mobius::middleware::subagents::SubagentCeilings,
}

impl PreparedBot {
    #[cfg(test)]
    pub(crate) fn test_models(&mut self, models: Arc<ModelRouter>) {
        self.models = models;
    }

    pub(crate) fn matches_runtime(&self, bot: &crate::wire::BotRecord) -> bool {
        !self.stale.load(std::sync::atomic::Ordering::Acquire)
            && self.bot.description == bot.description
            && self.bot.config.config == bot.config.config
    }

    pub(crate) fn invalidate(&self) {
        self.stale.store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn instructions(&self) -> String {
        format!(
            "{}\n\n{}",
            self.bot.description, self.bot.config.config.system_prompt
        )
    }
}

pub(crate) fn prepare_bot<'a>(
    gateway: &GatewayConfig,
    bot: crate::wire::BotRecord,
    store: &'a ConfigStore,
    credentials: &CredentialStore,
    session_files: SessionFileStore,
    epoch: u64,
    computer_config: Arc<crate::computer_runtime::ComputerConfig>,
) -> impl std::future::Future<Output = Result<PreparedBot>> + Send + use<'a> {
    // Resolve configuration while its owner can lend a lock guard; only runtime preparation awaits.
    let prepared = (|| {
        #[cfg(test)]
        store
            .runtime_operations
            .preparations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let config = &bot.config.config;
        let choices = crate::provider_catalog::configured_model_catalog(gateway)?;
        crate::config::validate_bot_compatibility(gateway, config, choices.catalogs())?;
        let model_providers = configured_model_providers(gateway, store, credentials)?;
        let endpoint = gateway
            .configured_providers
            .get(&config.provider.instance)
            .map_or(&config.provider, |configured| &configured.selection);
        let (models, context_window) = if credential_is_configured(endpoint, store, credentials)? {
            build_models(gateway, &config.provider, store, credentials, session_files)?
        } else {
            unavailable_models(gateway, &config.provider, store, credentials, session_files)?
        };
        let approval_policy = configured_approval_policy(&config.middleware)?;
        let active_message_delivery = configured_message_delivery(&config.middleware)?;
        let extensions = ExtensionStore::new(store).resolve(gateway, &config.extensions)?;
        let providers = std::iter::once(config.provider.clone())
            .chain(
                gateway
                    .configured_providers
                    .values()
                    .filter(|provider| provider.selection.instance != config.provider.instance)
                    .map(|provider| provider.selection.clone()),
            )
            .collect();
        let subagent_ceilings = gateway.execution.subagent_ceilings;
        Ok::<_, Error>(async move {
            let computer_runtime = crate::computer_runtime::prepare(
                store.state_dir(),
                &bot.config.config.middleware,
                &computer_config,
            )
            .await?;
            Ok(PreparedBot {
                stale: std::sync::atomic::AtomicBool::new(false),
                providers,
                bot,
                epoch,
                models,
                context_window,
                model_providers,
                approval_policy,
                active_message_delivery,
                extensions,
                computer_runtime,
                computer_config,
                subagent_ceilings,
            })
        })
    })();
    async move { prepared?.await }
}

pub(crate) struct BuiltAgent {
    pub(crate) agent: Agent,
    pub(crate) model_router: Arc<ModelRouter>,
    pub(crate) sandbox: Arc<Sandbox>,
    pub(crate) gateway_sandbox: Arc<GatewaySandbox>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "the headless composition root keeps its runtime dependencies explicit"
)]
pub(crate) async fn assemble(
    gateway: Arc<Mutex<GatewayConfig>>,
    chat: &ChatSpec,
    store: &ConfigStore,
    checkpoints: Arc<dyn CheckpointStore>,
    scratchpad: Arc<Scratchpad>,
    session_files: SessionFileStore,
    discovery_gate: Arc<tokio::sync::Mutex<()>>,
    desktop: Arc<crate::computer_runtime::desktop::DesktopControl>,
    remote_desktop: Arc<crate::computer_runtime::remote_desktop::RemoteDesktop>,
    session_id: Option<String>,
    origin_label: &str,
    prepared: Arc<PreparedBot>,
    live_chats: Option<Arc<dyn LiveChats>>,
    host_access: Option<crate::host::HostAccess>,
) -> Result<BuiltAgent> {
    #[cfg(test)]
    store
        .runtime_operations
        .assemblies
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some(session_id) = session_id.as_deref() {
        validate_session_id(session_id)?;
    }
    let (workspace_path, execution, telemetry, tls, profile_directory) = {
        let config = gateway
            .lock()
            .map_err(|_| Error::Config("gateway configuration lock is poisoned".into()))?;
        (
            chat.execution_root(store.state_dir(), config.tls.as_ref())?,
            config.execution.clone(),
            config.telemetry.clone(),
            config.tls.clone(),
            config.computer.browser.profile_directory.clone(),
        )
    };
    let models = Arc::clone(&prepared.models);
    let context_window = prepared.context_window;
    let approval_policy = prepared.approval_policy;
    let project = chat.workspace.is_some();
    let attached_folders = chat.attached_folders.clone();
    let state_dir = store.state_dir().to_path_buf();
    let token_estimate = mobius::middleware::TokenEstimate::new(execution.bytes_per_token)?;
    let resources = Arc::clone(&prepared);
    let gateway_for_middleware = Arc::clone(&gateway);
    // Hidden routine and channel chats may carry third-party input.
    let live_chats = live_chats.filter(|_| chat.catalog_visible);
    let (gateway_sandbox, sandbox, mut entries) = run_discovery(discovery_gate, move || {
        let settings = &resources.bot.config.config.middleware;
        let computer_runtime = &resources.computer_runtime;
        let tls_key = tls.as_ref().map(|tls| tls.private_key.as_path());
        if let Some(hook) = &telemetry.activity_hook {
            hook.validate_roots(
                [&state_dir, &workspace_path]
                    .into_iter()
                    .chain(attached_folders.iter())
                    .map(std::path::PathBuf::as_path),
            )?;
        }
        let resolved_extensions = &resources.extensions;
        let extensions = (EXTENSIONS_MANIFEST.required || settings.enabled(EXTENSIONS_MANIFEST.id))
            .then(|| {
                Extensions::installed(
                    &workspace_path,
                    resolved_extensions.skill_roots.iter().cloned(),
                    resolved_extensions
                        .plugins
                        .iter()
                        .map(|plugin| plugin.activation(Arc::clone(&gateway_for_middleware))),
                )
            })
            .transpose()?;
        let mut read_roots = extensions
            .as_ref()
            .map_or_else(Vec::new, Extensions::resource_roots);
        if let Some(computer) = computer_runtime {
            read_roots.extend(crate::computer_runtime::resource_roots(
                &computer.directory,
                &resources.computer_config,
                &state_dir,
                &workspace_path,
                &attached_folders,
            )?);
        }
        let output_bytes =
            crate::middleware_manifest::usize_setting(settings, "sandbox", "tool_output_bytes")?;
        let credential_environment = telemetry
            .sinks
            .iter()
            .filter_map(|sink| sink.bearer_env.as_deref())
            .collect::<Vec<_>>();
        let gateway_sandbox = Arc::new(
            GatewaySandbox::new_configured(
                &workspace_path,
                &state_dir,
                tls_key,
                &execution,
                output_bytes,
                &credential_environment,
            )?
            .with_desktop(desktop)
            .with_remote_desktop(remote_desktop)
            .deny_read_paths(profile_directory.into_iter().chain(
                telemetry.sinks.iter().filter_map(|sink| {
                    sink.bearer_file
                        .as_ref()
                        .map(|file| {
                            let path = std::path::PathBuf::from(file);
                            if path.is_absolute() {
                                path
                            } else {
                                state_dir.join(path)
                            }
                        })
                        .filter(|path| path.exists())
                }),
            ))?
            .allow_attached_folders(attached_folders.iter().cloned())?
            .allow_read_roots(read_roots)?,
        );
        let backend: Arc<dyn SandboxBackend> = gateway_sandbox.clone();
        let sandbox = Sandbox::new(Arc::clone(&backend), approval_policy)
            .tool_output_limit(output_bytes)?
            .background_command_limit(crate::middleware_manifest::usize_setting(
                settings,
                "sandbox",
                "background_commands",
            )?)?;
        let sandbox = if attached_folders.is_empty() {
            sandbox
        } else {
            sandbox.attached_folders(workspace_path.clone(), attached_folders)
        };
        let middleware = build_middleware(
            &resources,
            &workspace_path,
            &scratchpad,
            session_files,
            extensions.map(|extensions| (extensions, backend)),
            live_chats,
            project,
        )?;
        Ok((gateway_sandbox, Arc::new(sandbox), middleware))
    })
    .await?;
    let mut metadata = match session_id.as_deref() {
        Some(session_id) => checkpoints
            .load(session_id)
            .await?
            .map(|checkpoint| checkpoint.metadata)
            .unwrap_or_default(),
        None => Default::default(),
    };
    metadata.extend(chat.metadata()?);
    let workspace = chat.workspace_info();
    let workspace_label = workspace
        .as_ref()
        .map(|workspace| workspace.path.display().to_string());
    let usage_resources = Arc::clone(&prepared);
    let usage_store = Arc::new(store.clone());
    let max_model_steps =
        usize::try_from(prepared.bot.config.config.max_model_steps).map_err(|_| {
            Error::Config("maximum model steps exceed this platform's supported range".into())
        })?;
    let persistent = session_id.as_deref() == Some(prepared.bot.conversation_session_id.as_str());
    if persistent {
        if project || !chat.catalog_visible {
            return Err(Error::Config(
                "Persistent Chat must be visible and project-free".into(),
            ));
        }
        entries.push(Arc::new(crate::persistent_chat::PersistentChat::new(
            prepared.bot.id.clone(),
            host_access
                .ok_or_else(|| Error::Config("Persistent Chat requires the gateway".into()))?,
        )));
    }
    let system_prompt = prepared.instructions();
    let mut agent_config = AgentConfig::new(
        models,
        Arc::clone(&sandbox),
        checkpoints,
        MiddlewareStack::new(entries)?,
        system_prompt,
    )
    .context_window(context_window)
    .token_estimate(token_estimate)
    .catalog_visible(chat.catalog_visible)
    .initial_replay_batches(0)
    .override_saved_model_route()
    .max_model_steps(max_model_steps)
    .metadata(metadata)
    .usage_observer(move |route, usage| {
        let resources = Arc::clone(&usage_resources);
        let gateway = Arc::clone(&gateway);
        let store = Arc::clone(&usage_store);
        Box::pin(async move {
            let provider = resources.model_providers.get(route).ok_or_else(|| {
                MobiusError::Config(
                    "model route is not in the configured gateway usage catalog".into(),
                )
            })?;
            publish_usage(&gateway, &store, provider, usage).await
        })
    })
    .session_context(SessionContext {
        owner_id: chat.bot_id.clone(),
        user_name: local_user_name(),
        workspace_id: workspace.map(|workspace| workspace.id),
        workspace_label,
        origin_label: Some(origin_label.into()),
        ..SessionContext::default()
    });
    if let Some(session_id) = session_id {
        agent_config = agent_config.session_id(session_id);
    }
    let agent = create_agent(agent_config).await?;
    let model_router = agent.model_router();
    Ok(BuiltAgent {
        agent,
        model_router,
        sandbox,
        gateway_sandbox,
    })
}

// Usage publication is serialized before entering the blocking pool, including across chats.
static USAGE_PUBLICATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn publish_usage(
    gateway: &Arc<Mutex<GatewayConfig>>,
    store: &ConfigStore,
    provider: &str,
    usage: &TokenUsage,
) -> mobius::Result<()> {
    let guard = USAGE_PUBLICATION.lock().await;
    let gateway = Arc::clone(gateway);
    let store = store.clone();
    let provider = provider.to_owned();
    let usage = usage.clone();
    tokio::task::spawn_blocking(move || {
        // Keep admission occupied until atomic publication finishes, even after caller cancellation.
        let _guard = guard;
        persist_usage(&gateway, &store, &provider, &usage)
    })
    .await
    .map_err(|error| MobiusError::Config(format!("usage publication task failed: {error}")))?
}

fn persist_usage(
    gateway: &Mutex<GatewayConfig>,
    store: &ConfigStore,
    provider: &str,
    usage: &TokenUsage,
) -> mobius::Result<()> {
    let mut gateway = gateway
        .lock()
        .map_err(|_| MobiusError::Config("gateway configuration lock is poisoned".into()))?;
    store
        .record_usage(&mut gateway, provider, usage)
        .map_err(|error| MobiusError::Config(error.to_string()))
}

fn build_models(
    gateway: &GatewayConfig,
    selection: &ProviderConfig,
    store: &ConfigStore,
    credentials: &CredentialStore,
    session_files: SessionFileStore,
) -> Result<(Arc<ModelRouter>, i64)> {
    gateway.validate_provider_selection(selection)?;
    let definition = provider(&selection.provider)?;
    let configured = gateway
        .configured_providers
        .get(&selection.instance)
        .ok_or_else(|| Error::Config("active provider is not in the configured catalog".into()))?;
    let effort = effective_reasoning_effort(definition, configured, selection);
    let selected_route = model_route_id(&selection.instance, &selection.model, effort);
    let mut catalog = catalog_routes(definition, configured, selection);
    catalog.extend(
        configured_model_routes(
            &gateway.configured_providers,
            gateway
                .bot_defaults
                .as_ref()
                .map(|defaults| defaults.config.provider.instance.as_str()),
            store,
            credentials,
        )?
        .into_iter()
        .filter(|route| route.provider.instance != selection.instance),
    );
    catalog.sort_by_key(|route| route.choice.route != selected_route);
    if catalog.first().map(|route| route.choice.route.as_str()) != Some(selected_route.as_str()) {
        return Err(Error::Config(
            "active model route is not in the configured gateway catalog".into(),
        ));
    }
    let media = crate::provider_catalog::media_routes(
        &gateway.configured_providers,
        Some((store, credentials)),
    )?;
    catalog.extend(media.images.into_iter().chain(media.voices));
    let routes = instantiate_routes(catalog, store, credentials, &gateway.model_transport)?;
    let mut routes = routes.into_iter();
    let first = routes
        .next()
        .ok_or_else(|| Error::Config("provider has no model routes".into()))?;
    let context_window = first
        .choice
        .context_window
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let mut router =
        ModelRouter::new(&first.choice.route, first.model).session_files(session_files);
    router.set_credential_lifetime(&first.choice.route, first.lifetime)?;
    router.set_context_group(&first.choice.route, first.instance)?;
    router.configure_choice(first.choice)?;
    for route in routes {
        register_route(&mut router, route)?;
    }
    Ok((Arc::new(router), context_window))
}

fn register_route(router: &mut ModelRouter, route: RouteValue) -> Result<()> {
    match route.capability {
        Some(mobius::protocol::ModelCapability::RealtimeVoice) => {
            router.register_voice(route.model, route.choice, route.lifetime)?;
        }
        Some(mobius::protocol::ModelCapability::ImageGeneration) => {
            router.register_image(route.model, route.choice, route.lifetime)?;
        }
        _ => {
            router.register(&route.choice.route, route.model)?;
            router.set_credential_lifetime(&route.choice.route, route.lifetime)?;
            router.set_context_group(&route.choice.route, route.instance)?;
            router.configure_choice(route.choice)?;
        }
    }
    Ok(())
}

fn instantiate_routes(
    catalog: Vec<CatalogRoute>,
    store: &ConfigStore,
    credentials: &CredentialStore,
    transport: &mobius::backend::model::ModelTransportSettings,
) -> Result<Vec<RouteValue>> {
    let http = transport.streaming_client()?;
    let mut provider_credentials =
        BTreeMap::<String, (ProviderCredential, ModelCredentialLifetime)>::new();
    let mut routes: Vec<RouteValue> = Vec::with_capacity(catalog.len());
    for mut route in catalog {
        // Media requests carry their selected model/variant; share the adapter,
        // keeping a separate revocation cursor for each route.
        if route.capability.is_some()
            && let Some(previous) = routes.iter().find(|previous| {
                previous.instance == route.provider.instance
                    && previous.capability == route.capability
            })
        {
            routes.push(RouteValue {
                instance: route.provider.instance,
                capability: route.capability,
                choice: route.choice,
                model: Arc::clone(&previous.model),
                lifetime: previous.lifetime.clone(),
            });
            continue;
        }
        let definition = provider(&route.provider.provider)?;
        let base_url = if definition.configurable_base_url() {
            route
                .provider
                .base_url
                .take()
                .or_else(|| definition.default_base_url().map(str::to_owned))
        } else {
            None
        };
        let (credential, lifetime) =
            if route.provider.endpoint_auth == ProviderEndpointAuth::Credentialless {
                (ProviderCredential::Credentialless, Default::default())
            } else {
                match provider_credentials.get(route.provider.instance.as_str()) {
                    Some(credential) => credential.clone(),
                    None => {
                        let credential = resolve_credential(
                            &route.provider.instance,
                            definition,
                            base_url.as_deref(),
                            store,
                            credentials,
                            transport,
                        )?;
                        provider_credentials
                            .insert(route.provider.instance.clone(), credential.clone());
                        credential
                    }
                }
            };
        routes.push(build_route(
            route, definition, credential, lifetime, base_url, &http, transport,
        )?);
    }
    Ok(routes)
}

fn resolve_credential(
    instance: &str,
    definition: &ProviderDefinition,
    base_url: Option<&str>,
    store: &ConfigStore,
    credentials: &CredentialStore,
    transport: &mobius::backend::model::ModelTransportSettings,
) -> Result<(ProviderCredential, ModelCredentialLifetime)> {
    match definition.auth() {
        ProviderAuth::ApiKey(default_env) => {
            if let Some(value) = credentials.get(instance, definition.id(), base_url)? {
                return Ok((ProviderCredential::ApiKey(value.api_key), value.lifetime));
            }
            if !definition.uses_default_endpoint(base_url) {
                return Err(Error::Config(format!(
                    "set a credential for `{}`",
                    definition.id()
                )));
            }
            let value = std::env::var(default_env).map_err(|_| {
                Error::Config(format!("set a credential for `{}`", definition.id()))
            })?;
            if value.trim().is_empty() {
                return Err(Error::Config(format!(
                    "credential environment variable {default_env} is empty"
                )));
            }
            Ok((ProviderCredential::ApiKey(value), Default::default()))
        }
        ProviderAuth::Browser(auth) => auth
            .load_with_transport(&store.provider_auth_path(), *transport)
            .map(|credential| (credential, Default::default()))
            .map_err(Error::from),
    }
}

fn build_route(
    route: CatalogRoute,
    definition: &'static ProviderDefinition,
    credential: ProviderCredential,
    lifetime: ModelCredentialLifetime,
    base_url: Option<String>,
    http: &HttpClient,
    transport: &mobius::backend::model::ModelTransportSettings,
) -> Result<RouteValue> {
    let config = ProviderBuildConfig {
        capability: route.capability,
        credential,
        model: route.provider.model,
        base_url,
        reasoning_effort: route.provider.reasoning_effort,
        service_tier: route.provider.service_tier,
        web_search: route.provider.web_search,
        tool_discovery: Some(route.choice.tool_discovery),
        http: http.clone(),
        transport: *transport,
    };
    let model = definition.build(config)?;
    let mut choice = route.choice;
    if route.capability.is_none() {
        choice.supports_image_input = model.supports_image_input();
        choice.supports_image_generation = model.supports_image_generation();
    }
    Ok(RouteValue {
        instance: route.provider.instance,
        capability: route.capability,
        choice,
        model,
        lifetime,
    })
}

struct RouteValue {
    instance: String,
    capability: Option<mobius::protocol::ModelCapability>,
    lifetime: ModelCredentialLifetime,
    choice: ModelChoice,
    model: Arc<dyn Model>,
}

struct UnavailableModel {
    info: ModelInfo,
    supports_image_input: bool,
}

impl Model for UnavailableModel {
    fn info(&self) -> ModelInfo {
        self.info.clone()
    }

    fn supports_image_input(&self) -> bool {
        self.supports_image_input
    }

    fn respond<'a>(
        &'a self,
        _request: ModelRequest<'a>,
        _events: ModelEventSink,
    ) -> mobius::BoxFuture<'a, mobius::Result<ModelOutput>> {
        Box::pin(async {
            Err(MobiusError::Auth(
                "the selected provider is not configured on this gateway".into(),
            ))
        })
    }
}

fn unavailable_models(
    gateway: &GatewayConfig,
    selection: &ProviderConfig,
    store: &ConfigStore,
    credentials: &CredentialStore,
    session_files: SessionFileStore,
) -> Result<(Arc<ModelRouter>, i64)> {
    let definition = provider(&selection.provider)?;
    let configured = gateway.configured_providers.get(&selection.instance);
    let preset = configured
        .and_then(|configured| {
            configured
                .models
                .iter()
                .find(|model| model.id == selection.model)
        })
        .or_else(|| definition.model(&selection.model));
    let context_window = preset.map_or_else(
        || crate::provider_catalog::model_context_window(definition, &selection.model),
        |model| model.context_window,
    );
    let effort = match gateway.configured_providers.get(&selection.instance) {
        Some(configured) => {
            gateway.validate_provider_selection(selection)?;
            effective_reasoning_effort(definition, configured, selection).map(str::to_string)
        }
        None => selection.reasoning_effort.clone().or_else(|| {
            definition
                .model(&selection.model)
                .and_then(|preset| preset.default_reasoning.clone())
        }),
    };
    let route = model_route_id(&selection.instance, &selection.model, effort.as_deref());
    let model: Arc<dyn Model> = Arc::new(UnavailableModel {
        info: ModelInfo {
            model: selection.model.clone(),
            reasoning_effort: effort.clone(),
        },
        supports_image_input: definition.supports_image_input(),
    });
    let mut router = ModelRouter::new(&route, model).session_files(session_files);
    router.configure_choice(ModelChoice {
        route,
        group: selection.model.clone(),
        model: selection.model.clone(),
        reasoning_effort: effort,
        variant_label: None,
        context_window: Some(context_window),
        supports_image_input: definition.supports_image_input(),
        supports_image_generation: false,
        supports_realtime_voice: false,
        tool_discovery: configured.zip(preset).map_or_else(
            || definition.tool_discovery(&selection.model, selection.base_url.as_deref()),
            |(configured, model)| {
                crate::provider_catalog::effective_tool_discovery(
                    definition,
                    &configured.selection,
                    model,
                )
            },
        ),
    })?;
    let media = crate::provider_catalog::media_routes(
        &gateway.configured_providers,
        Some((store, credentials)),
    )?;
    let catalog = media.images.into_iter().chain(media.voices).collect();
    for route in instantiate_routes(catalog, store, credentials, &gateway.model_transport)? {
        register_route(&mut router, route)?;
    }
    Ok((Arc::new(router), context_window))
}

fn build_middleware(
    prepared: &PreparedBot,
    workspace: &std::path::Path,
    scratchpad: &Scratchpad,
    session_files: SessionFileStore,
    mut extensions: Option<(Extensions, Arc<dyn SandboxBackend>)>,
    mut live_chats: Option<Arc<dyn LiveChats>>,
    project: bool,
) -> Result<Vec<Arc<dyn Middleware>>> {
    let settings = &prepared.bot.config.config.middleware;
    let mut entries: Vec<Arc<dyn Middleware>> = Vec::new();
    for feature in MIDDLEWARE
        .iter()
        .filter(|feature| feature.manifest.required || settings.enabled(feature.manifest.id))
    {
        let middleware: Arc<dyn Middleware> = match feature.kind {
            BuiltinMiddleware::Sandbox | BuiltinMiddleware::PersistentChat => continue,
            BuiltinMiddleware::Attachments => {
                let attachments = Attachments::new(session_files.clone());
                Arc::new(if project {
                    attachments.with_workspace(workspace)?
                } else {
                    attachments
                })
            }
            BuiltinMiddleware::Artifacts => Arc::new(Artifacts::new(session_files.clone())),
            BuiltinMiddleware::ImageGeneration => Arc::new(ImageGeneration::from_settings(
                Arc::clone(&prepared.models),
                session_files.clone(),
                |id| settings.setting(mobius::middleware::image_generation::MANIFEST.id, id),
            )?),
            BuiltinMiddleware::Tools => Arc::new(if project {
                Tools::coding(session_files.clone())
            } else {
                Tools::reading(session_files.clone())
            }),
            BuiltinMiddleware::Instructions => {
                Arc::new(Instructions::discover(project.then_some(workspace))?)
            }
            BuiltinMiddleware::Scratchpad => Arc::new(scratchpad.for_chat()),
            BuiltinMiddleware::Extensions => {
                let (extensions, backend) = extensions
                    .take()
                    .ok_or_else(|| Error::Config("extensions were not discovered".into()))?;
                Arc::new(extensions.start_hooks(backend)?)
            }
            BuiltinMiddleware::Questions => {
                Arc::new(mobius::middleware::questions::Questions::default())
            }
            BuiltinMiddleware::Tasks => Arc::new(Tasks),
            BuiltinMiddleware::Subagents => {
                let middleware = Subagents::new(crate::middleware_manifest::subagent_limits(
                    settings,
                    prepared.subagent_ceilings,
                )?)
                .session_files(session_files.clone());
                let middleware = match crate::middleware_manifest::string_setting(
                    settings,
                    mobius::middleware::subagents::MANIFEST.id,
                    "model_route",
                )? {
                    Some(route) => middleware.default_model(route),
                    None => middleware,
                };
                Arc::new(middleware)
            }
            BuiltinMiddleware::Messages => Arc::new(Messages::new(
                crate::middleware_manifest::usize_setting(settings, "messages", "max_pending")?,
                prepared.active_message_delivery,
            )?),
            BuiltinMiddleware::Voice => Arc::new(mobius::middleware::voice::Voice),
            BuiltinMiddleware::ComputerControl => {
                let prepared = prepared
                    .computer_runtime
                    .as_ref()
                    .ok_or_else(|| Error::Config("computer runtime was not prepared".into()))?;
                Arc::new(mobius::middleware::computer_control::ComputerControl::new(
                    session_files.clone(),
                    Arc::clone(&prepared.runtime),
                ))
            }
            BuiltinMiddleware::Compaction => {
                Arc::new(mobius::middleware::compaction::Compaction::from_settings(
                    |id| settings.setting(feature.manifest.id, id),
                )?)
            }
            BuiltinMiddleware::Sessions => Arc::new(Sessions::new(
                crate::middleware_manifest::usize_setting(settings, "sessions", "page_size")?,
                Some(session_files.clone()),
                live_chats.take(),
            )?),
        };
        entries.push(middleware);
    }
    Ok(entries)
}

fn configured_message_delivery(settings: &MiddlewareConfig) -> Result<ActiveMessageDelivery> {
    crate::middleware_manifest::string_setting(settings, "messages", "delivery")?
        .ok_or_else(|| Error::Config("missing middleware setting `messages.delivery`".into()))?
        .parse::<ActiveMessageDelivery>()
        .map_err(Error::from)
}

pub(crate) fn bot_semantics(config: &AgentComposition) -> Result<(bool, RoutineInteractionPolicy)> {
    let accepts_file_attachments = config
        .middleware
        .enabled(mobius::middleware::attachments::MANIFEST.id);
    let routine_interaction_policy = match configured_approval_policy(&config.middleware)? {
        ApprovalPolicy::Ask => RoutineInteractionPolicy::MayPauseForApproval,
        ApprovalPolicy::Allow | ApprovalPolicy::AllowNetwork | ApprovalPolicy::FullAccess => {
            RoutineInteractionPolicy::Unattended
        }
    };
    Ok((accepts_file_attachments, routine_interaction_policy))
}

#[cfg(test)]
mod tests;
