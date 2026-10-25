use mobius::SessionId;
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

use crate::config::{ChatSpec, ConfigStore, CredentialStore, GatewayConfig, local_user_name};
use crate::extensions::{ExtensionStore, ResolvedExtensions};
use crate::middleware_manifest::{BuiltinMiddleware, MIDDLEWARE};
use crate::provider_catalog::{
    CatalogRoute, catalog_routes, configured_model_providers, configured_model_routes,
    credential_is_configured,
};
use crate::sandbox::GatewaySandbox;
use crate::wire::{
    AgentComposition, MiddlewareConfig, ProviderConfig, ProviderEndpointAuth,
    RoutineInteractionPolicy,
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
    pub(crate) bot: Arc<crate::wire::BotRecord>,
    pub(crate) epoch: u64,
    stale: std::sync::atomic::AtomicBool,
    pub(crate) providers: Vec<ProviderConfig>,
    models: Arc<ModelRouter>,
    context_window: i64,
    model_providers: BTreeMap<Arc<str>, String>,
    approval_policy: ApprovalPolicy,
    pub(crate) active_message_delivery: ActiveMessageDelivery,
    /// The configured voice route when voice is on; `Some(None)` uses the default voice.
    pub(crate) voice: Option<Option<Arc<str>>>,
    extensions: ResolvedExtensions,
    computer_runtime: Option<crate::computer_runtime::PreparedComputer>,
    computer_config: Arc<crate::computer_runtime::ComputerConfig>,
    subagent_limits: Option<mobius::middleware::subagents::SubagentLimits>,
}

impl PreparedBot {
    #[cfg(test)]
    pub(crate) fn test_models(&mut self, models: Arc<ModelRouter>) {
        self.models = models;
    }

    pub(crate) fn matches_runtime(&self, bot: &crate::wire::BotRecord) -> bool {
        !self.stale.load(std::sync::atomic::Ordering::Acquire)
            && same_runtime(&self.bot, &bot.description, &bot.config.config)
    }

    pub(crate) fn invalidate(&self) {
        self.stale.store(true, std::sync::atomic::Ordering::Release);
    }

    /// The agent's and voice's system prompt: the Bot's description as its preamble.
    pub(crate) fn instructions(&self) -> String {
        compose_instructions(&self.bot.description, &self.bot.config.config.system_prompt)
    }
}

fn compose_instructions(description: &str, system_prompt: &str) -> String {
    match description.trim() {
        "" => system_prompt.to_owned(),
        preamble => format!("{preamble}\n\n{system_prompt}"),
    }
}

/// Whether `bot` starts the same agent as one with `description` and `config`.
pub(crate) fn same_runtime(
    bot: &crate::wire::BotRecord,
    description: &str,
    config: &AgentComposition,
) -> bool {
    *bot.description == *description && bot.config.config == *config
}

pub(crate) fn prepare_bot<'a>(
    gateway: &GatewayConfig,
    crate::bots::RuntimeBot {
        record: bot,
        approval_policy,
    }: crate::bots::RuntimeBot,
    store: &'a ConfigStore,
    credentials: &CredentialStore,
    session_files: Arc<SessionFileStore>,
    epoch: u64,
    computer_config: Arc<crate::computer_runtime::ComputerConfig>,
) -> impl std::future::Future<Output = Result<PreparedBot>> + Send + use<'a> {
    // Resolve configuration while its owner can lend a lock guard; only runtime preparation awaits.
    let prepared = (|| {
        let config = &bot.config.config;
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
        let active_message_delivery = configured_message_delivery(&config.middleware)?;
        let voice_id = mobius::middleware::voice::MANIFEST.id;
        let voice = config
            .middleware
            .enabled(voice_id)
            .then(|| {
                crate::middleware_manifest::string_setting(&config.middleware, voice_id, "model")
                    .map(|route| route.map(Arc::from))
            })
            .transpose()?;
        let extensions = ExtensionStore::new(store).resolve(gateway, &config.extensions)?;
        let providers = std::iter::once(config.provider.clone()) // rules-allow: the prepared catalog owns its provider selections
            .chain(
                gateway
                    .configured_providers
                    .values()
                    .filter(|provider| provider.selection.instance != config.provider.instance)
                    .map(|provider| provider.selection.clone()), // rules-allow: the prepared catalog owns its provider selections
            )
            .collect();
        let subagent_limits = (mobius::middleware::subagents::MANIFEST.required
            || config
                .middleware
                .enabled(mobius::middleware::subagents::MANIFEST.id))
        .then(|| {
            crate::middleware_manifest::subagent_limits(
                &config.middleware,
                gateway.execution.subagent_ceilings,
            )
        })
        .transpose()?;
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
                voice,
                extensions,
                computer_runtime,
                computer_config,
                subagent_limits,
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
    session_files: Arc<SessionFileStore>,
    discovery_gate: Arc<tokio::sync::Mutex<()>>,
    desktop: Arc<crate::computer_runtime::desktop::DesktopControl>,
    remote_desktop: Arc<crate::computer_runtime::remote_desktop::RemoteDesktop>,
    session_id: Option<SessionId>,
    origin_label: &str,
    prepared: Arc<PreparedBot>,
    live_chats: Option<Arc<dyn LiveChats>>,
    host_access: Option<crate::host::HostAccess>,
) -> Result<BuiltAgent> {
    let (workspace_path, execution, telemetry, tls, profile_directory) = {
        let config = crate::config::lock(&gateway)?;
        // The snapshot outlives the configuration lock.
        (
            chat.execution_root(store.state_dir(), config.tls.as_ref())?,
            config.execution.clone(), // rules-allow: snapshot taken under the configuration lock
            config.telemetry.clone(), // rules-allow: snapshot taken under the configuration lock
            config.tls.clone(),       // rules-allow: snapshot taken under the configuration lock
            config.computer.browser.profile_directory.clone(), // rules-allow: snapshot taken under the configuration lock
        )
    };
    let models = Arc::clone(&prepared.models);
    let context_window = prepared.context_window;
    let approval_policy = prepared.approval_policy;
    let project = chat.workspace.is_some();
    let attached_folders = chat.attached_folders.clone(); // rules-allow: the discovery task owns the chat's folder list
    let state_dir = store.state_dir().to_path_buf();
    let token_estimate = execution.token_estimate;
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
                    resolved_extensions.skill_roots.iter().cloned(), // rules-allow: the prepared Bot keeps its roots for later chats
                    resolved_extensions
                        .plugins
                        .iter()
                        .map(|plugin| plugin.activation(&gateway_for_middleware)),
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
        let output_bytes = mobius::backend::sandbox::OutputLimit::new(
            crate::middleware_manifest::count_setting(
                settings,
                mobius::backend::sandbox::MANIFEST.id,
                "tool_output_bytes",
            )?
            .get(),
        )?;
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
            .allow_attached_folders(attached_folders.iter().cloned())? // rules-allow: the backend and the sandbox each own the folder list
            .allow_read_roots(read_roots)?,
        );
        let backend: Arc<dyn SandboxBackend> = Arc::<GatewaySandbox>::clone(&gateway_sandbox); // rules-allow: the sandbox and the returned agent share the backend
        let sandbox = Sandbox::new(Arc::clone(&backend), approval_policy)
            .tool_output_limit(output_bytes)
            .background_command_limit(crate::middleware_manifest::count_setting(
                settings,
                mobius::backend::sandbox::MANIFEST.id,
                "background_commands",
            )?);
        let middleware = build_middleware(
            &resources,
            &workspace_path,
            &scratchpad,
            &session_files,
            extensions.map(|extensions| (extensions, backend)),
            live_chats,
            project,
        )?;
        let sandbox = if attached_folders.is_empty() {
            sandbox
        } else {
            sandbox.attached_folders(workspace_path, attached_folders)
        };
        Ok((gateway_sandbox, Arc::new(sandbox), middleware))
    })
    .await?;
    let mut metadata = match session_id.as_ref() {
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
    let usage_store = Arc::new(store.clone()); // rules-allow: the usage observer outlives this call
    let max_model_steps =
        usize::try_from(prepared.bot.config.config.max_model_steps).map_err(|error| {
            Error::Config(format!(
                "maximum model steps exceed this platform's supported range: {error}"
            ))
        })?;
    let persistent = session_id.as_ref() == Some(&prepared.bot.conversation_session_id);
    if persistent {
        if project || !chat.catalog_visible {
            return Err(Error::Config(
                "Persistent Chat must be visible and project-free".into(),
            ));
        }
        entries.push(Arc::new(crate::persistent_chat::PersistentChat::new(
            Arc::clone(&prepared.bot.id),
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
        owner_id: Arc::clone(&chat.bot_id),
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
    // The blocking task must own everything it publishes.
    let gateway = Arc::clone(gateway);
    let store = store.clone(); // rules-allow: the blocking task owns its handle
    let provider = provider.to_owned();
    let usage = *usage;
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
    let mut gateway =
        crate::config::lock(gateway).map_err(|error| MobiusError::Config(error.to_string()))?;
    store
        .record_usage(&mut gateway, provider, usage)
        .map_err(|error| MobiusError::Config(error.to_string()))
}

fn build_models(
    gateway: &GatewayConfig,
    selection: &ProviderConfig,
    store: &ConfigStore,
    credentials: &CredentialStore,
    session_files: Arc<SessionFileStore>,
) -> Result<(Arc<ModelRouter>, i64)> {
    let definition = provider(&selection.provider)?;
    let configured = gateway
        .configured_providers
        .get(&selection.instance)
        .ok_or_else(|| Error::Config("active provider is not in the configured catalog".into()))?;
    let selected = crate::provider_catalog::selected_route(definition, Some(configured), selection);
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
    // Validation keeps the selected route in the catalog; it leads the router.
    catalog.sort_by_key(|route| *route.choice.route != *selected.route);
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
    let mut router =
        ModelRouter::new(Arc::clone(&first.choice.route), first.model).session_files(session_files);
    router.set_credential_lifetime(&first.choice.route, first.lifetime)?;
    router.set_context_group(&first.choice.route, first.instance)?;
    router.configure_choice(first.choice)?;
    for route in routes {
        register_route(&mut router, route)?;
    }
    Ok((Arc::new(router), selected.context_window))
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
            router.register(Arc::clone(&route.choice.route), route.model)?;
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
                lifetime: previous.lifetime.clone(), // rules-allow: each route keeps its own revocation cursor
            });
            continue;
        }
        let definition = provider(&route.provider.provider)?;
        let base_url = if definition.configurable_base_url() {
            route.provider.base_url.take().or_else(|| {
                definition
                    .default_base_url()
                    .map(mobius::backend::model::provider::BaseUrl::share)
            })
        } else {
            None
        };
        let (credential, lifetime) = if route.provider.endpoint_auth
            == ProviderEndpointAuth::Credentialless
        {
            (ProviderCredential::Credentialless, Default::default())
        } else if let Some(credential) = provider_credentials.get(route.provider.instance.as_str())
        {
            credential.clone() // rules-allow: routes of one instance share its credential
        } else {
            let credential = resolve_credential(
                &route.provider.instance,
                definition,
                base_url.as_ref(),
                store,
                credentials,
                transport,
            )?;
            provider_credentials.insert(route.provider.instance.clone(), credential.clone()); // rules-allow: the cache keeps the credential for later routes
            credential
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
    base_url: Option<&mobius::backend::model::provider::BaseUrl>,
    store: &ConfigStore,
    credentials: &CredentialStore,
    transport: &mobius::backend::model::ModelTransportSettings,
) -> Result<(ProviderCredential, ModelCredentialLifetime)> {
    match definition.auth() {
        ProviderAuth::ApiKey(default_env) => {
            if let Some(value) = credentials.get(instance, definition.id(), base_url)? {
                return Ok((ProviderCredential::ApiKey(value.api_key), value.lifetime));
            }
            if !definition.uses_default_endpoint(
                base_url.map(mobius::backend::model::provider::BaseUrl::as_str),
            ) {
                return Err(Error::Config(format!(
                    "set a credential for `{}`",
                    definition.id()
                )));
            }
            let value = std::env::var(default_env).map_err(|error| {
                Error::Config(format!(
                    "set a credential for `{}` ({default_env}: {error})",
                    definition.id()
                ))
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
    base_url: Option<mobius::backend::model::provider::BaseUrl>,
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
        http: http.clone(), // rules-allow: each provider adapter owns a handle to the shared HTTP client
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
        ModelInfo {
            model: Arc::clone(&self.info.model),
            reasoning_effort: self.info.reasoning_effort.as_ref().map(Arc::clone),
        }
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
    session_files: Arc<SessionFileStore>,
) -> Result<(Arc<ModelRouter>, i64)> {
    let definition = provider(&selection.provider)?;
    let configured = gateway.configured_providers.get(&selection.instance);
    let crate::provider_catalog::SelectedRoute {
        route,
        effort,
        preset,
        context_window,
    } = crate::provider_catalog::selected_route(definition, configured, selection);
    let effort: Option<Arc<str>> = effort.map(Arc::from);
    let route: Arc<str> = route.into();
    let model_id: Arc<str> = selection.model.as_str().into();
    let model: Arc<dyn Model> = Arc::new(UnavailableModel {
        info: ModelInfo {
            model: Arc::clone(&model_id),
            reasoning_effort: effort.as_ref().map(Arc::clone),
        },
        supports_image_input: definition.supports_image_input(),
    });
    let mut router = ModelRouter::new(Arc::clone(&route), model).session_files(session_files);
    router.configure_choice(ModelChoice {
        route,
        group: Arc::clone(&model_id),
        model: model_id,
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
    session_files: &Arc<SessionFileStore>,
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
                let attachments = Attachments::new(Arc::clone(session_files));
                Arc::new(if project {
                    attachments.with_workspace(workspace)?
                } else {
                    attachments
                })
            }
            BuiltinMiddleware::Artifacts => Arc::new(Artifacts::new(Arc::clone(session_files))),
            BuiltinMiddleware::IntelligentUi => {
                Arc::new(mobius::middleware::intelligent_ui::IntelligentUi)
            }
            BuiltinMiddleware::ImageGeneration => Arc::new(ImageGeneration::from_settings(
                Arc::clone(&prepared.models),
                Arc::clone(session_files),
                |id| settings.setting(mobius::middleware::image_generation::MANIFEST.id, id),
            )),
            BuiltinMiddleware::Tools => Arc::new(if project {
                Tools::coding(Arc::clone(session_files))
            } else {
                Tools::reading(Arc::clone(session_files))
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
                let middleware =
                    Subagents::new(prepared.subagent_limits.ok_or_else(|| {
                        Error::Config("subagent limits were not prepared".into())
                    })?)
                    .session_files(Arc::clone(session_files));
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
                crate::middleware_manifest::count_setting(
                    settings,
                    feature.manifest.id,
                    "max_pending",
                )?,
                prepared.active_message_delivery,
            )),
            BuiltinMiddleware::Voice => Arc::new(mobius::middleware::voice::Voice),
            BuiltinMiddleware::ComputerControl => {
                let prepared = prepared
                    .computer_runtime
                    .as_ref()
                    .ok_or_else(|| Error::Config("computer runtime was not prepared".into()))?;
                Arc::new(mobius::middleware::computer_control::ComputerControl::new(
                    Arc::clone(session_files),
                    Arc::clone(&prepared.runtime),
                ))
            }
            BuiltinMiddleware::Compaction => {
                Arc::new(mobius::middleware::compaction::Compaction::from_settings(
                    |id| settings.setting(feature.manifest.id, id),
                )?)
            }
            BuiltinMiddleware::Sessions => Arc::new(Sessions::new(
                crate::middleware_manifest::count_setting(
                    settings,
                    feature.manifest.id,
                    "page_size",
                )?,
                Some(Arc::clone(session_files)),
                live_chats.take(),
            )),
        };
        entries.push(middleware);
    }
    Ok(entries)
}

fn configured_message_delivery(settings: &MiddlewareConfig) -> Result<ActiveMessageDelivery> {
    crate::middleware_manifest::string_setting(
        settings,
        mobius::middleware::messages::MANIFEST.id,
        "delivery",
    )?
    .ok_or_else(|| Error::Config("missing middleware setting `messages.delivery`".into()))?
    .parse::<ActiveMessageDelivery>()
    .map_err(Error::from)
}

/// What chats composed from `config` can do, choosing the voice by the router's rule.
pub(crate) fn bot_capabilities(
    config: &AgentComposition,
    voices: &crate::provider_catalog::VoiceRoutes,
) -> Result<crate::wire::BotCapabilities> {
    let middleware = &config.middleware;
    let voice_id = mobius::middleware::voice::MANIFEST.id;
    let voice = if middleware.enabled(voice_id) {
        match voices.select(crate::middleware_manifest::string_setting(
            middleware, voice_id, "model",
        )?) {
            Ok((route, waits_for_session_started)) => crate::wire::VoiceCapability::Ready {
                route: Arc::clone(route),
                waits_for_session_started,
            },
            Err(reason) => crate::wire::VoiceCapability::Unavailable {
                reason: reason.into(),
            },
        }
    } else {
        crate::wire::VoiceCapability::Off
    };
    Ok(crate::wire::BotCapabilities {
        attachments: middleware.enabled(mobius::middleware::attachments::MANIFEST.id),
        computer: middleware.enabled(mobius::middleware::computer_control::MANIFEST.id),
        voice,
    })
}

/// Whether unattended routine work with `policy` can stop for a human decision.
pub(crate) const fn routine_interaction_policy(policy: ApprovalPolicy) -> RoutineInteractionPolicy {
    match policy {
        ApprovalPolicy::Ask => RoutineInteractionPolicy::MayPauseForApproval,
        ApprovalPolicy::Allow | ApprovalPolicy::AllowNetwork | ApprovalPolicy::FullAccess => {
            RoutineInteractionPolicy::Unattended
        }
    }
}

#[cfg(test)]
mod tests;
