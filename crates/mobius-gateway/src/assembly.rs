use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
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
use mobius::middleware::compaction::{Compaction, CompactionMode};
use mobius::middleware::context_offloading::ContextOffloading;
use mobius::middleware::extensions::{Extensions, MANIFEST as EXTENSIONS_MANIFEST};
use mobius::middleware::image_generation::ImageGeneration;
use mobius::middleware::instructions::Instructions;
use mobius::middleware::messages::Messages;
use mobius::middleware::scratchpad::{Scratchpad, ScratchpadStore};
use mobius::middleware::sessions::{LiveChats, Sessions};
use mobius::middleware::subagents::{SubagentLaunch, SubagentLauncher, Subagents};
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
    credential_is_configured, selected_base_url,
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
    pub(crate) compaction: Option<Arc<Compaction>>,
    extensions: ResolvedExtensions,
    computer_runtime: Option<std::path::PathBuf>,
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

pub(crate) async fn prepare_bot(
    gateway: &GatewayConfig,
    bot: crate::wire::BotRecord,
    store: &ConfigStore,
    credentials: &CredentialStore,
    session_files: SessionFileStore,
    epoch: u64,
    computer_config: Arc<crate::computer_runtime::ComputerConfig>,
) -> Result<PreparedBot> {
    #[cfg(test)]
    store
        .runtime_operations
        .preparations
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let config = &bot.config.config;
    let model_providers = configured_model_providers(gateway, store, credentials)?;
    let (models, context_window) =
        if credential_is_configured(&config.provider, store, credentials)? {
            build_models(gateway, &config.provider, store, credentials, session_files)?
        } else {
            unavailable_models(gateway, &config.provider, session_files)?
        };
    let choices = models.choices().cloned().collect::<Vec<_>>();
    crate::config::validate_bot_compatibility(gateway, config, &choices)?;
    let approval_policy = configured_approval_policy(&config.middleware)?;
    let active_message_delivery = configured_message_delivery(&config.middleware)?;
    let compaction = config
        .middleware
        .enabled(mobius::middleware::compaction::MANIFEST.id)
        .then(|| configured_compaction(&config.middleware).map(Arc::new))
        .transpose()?;
    let computer_runtime =
        crate::computer_runtime::prepare(store.state_dir(), &config.middleware, &computer_config)
            .await?;
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
        compaction,
        extensions,
        computer_runtime,
        computer_config,
        subagent_ceilings: gateway.execution.subagent_ceilings()?,
    })
}

pub(crate) struct BuiltAgent {
    pub(crate) agent: Agent,
    pub(crate) model_router: Arc<ModelRouter>,
    pub(crate) sandbox: Arc<Sandbox>,
    pub(crate) gateway_sandbox: Arc<GatewaySandbox>,
    pub(crate) subagents: Option<Arc<Subagents>>,
    pub(crate) subagent_template: Option<Arc<OnceLock<AgentConfig>>>,
}

struct BuiltMiddleware {
    entries: Vec<Arc<dyn Middleware>>,
    subagent_template: Option<Arc<OnceLock<AgentConfig>>>,
    subagents: Option<Arc<Subagents>>,
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
    scratchpad: ScratchpadStore,
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
    let gateway_config = gateway
        .lock()
        .map_err(|_| Error::Config("gateway configuration lock is poisoned".into()))?
        .clone();
    crate::config::validate_desktop_bot_policy(&gateway_config, &prepared.bot.config.config)?;
    let models = Arc::clone(&prepared.models);
    let context_window = prepared.context_window;
    let model_providers = prepared.model_providers.clone();
    let approval_policy = prepared.approval_policy;
    let settings = prepared.bot.config.config.middleware.clone();
    let workspace_path = chat.execution_root(store.state_dir(), gateway_config.tls.as_ref())?;
    let project = chat.workspace.is_some();
    let attached_folders = chat.attached_folders.clone();
    let state_dir = store.state_dir().to_path_buf();
    let computer_runtime = prepared.computer_runtime.clone();
    let tls_key = gateway_config
        .tls
        .as_ref()
        .map(|tls| tls.private_key.clone());
    let token_estimate =
        mobius::middleware::TokenEstimate::new(gateway_config.execution.bytes_per_token)?;
    let resources = Arc::clone(&prepared);
    let gateway_for_middleware = Arc::clone(&gateway);
    // Hidden routine and channel chats may carry third-party input.
    let live_chats = live_chats.filter(|_| chat.catalog_visible);
    let (
        gateway_sandbox,
        sandbox,
        BuiltMiddleware {
            mut entries,
            subagent_template: template,
            subagents,
        },
    ) = run_discovery(discovery_gate, move || {
        let resolved_extensions = &resources.extensions;
        let extensions = (EXTENSIONS_MANIFEST.required || settings.enabled(EXTENSIONS_MANIFEST.id))
            .then(|| {
                Extensions::discover_installed(
                    [
                        workspace_path.join(".agents/skills"),
                        workspace_path.join(".codex/skills"),
                    ]
                    .into_iter()
                    .chain(resolved_extensions.skill_roots.iter().cloned()),
                )
            })
            .transpose()?;
        let mut read_roots = extensions
            .as_ref()
            .map_or_else(Vec::new, Extensions::resource_roots);
        if extensions.is_some() {
            read_roots.extend(
                resolved_extensions
                    .plugins
                    .iter()
                    .map(|plugin| plugin.root.clone()),
            );
        }
        if let Some(runtime) = &computer_runtime {
            read_roots.extend(crate::computer_runtime::resource_roots(
                runtime,
                &resources.computer_config,
                &state_dir,
                &workspace_path,
                &attached_folders,
            )?);
        }
        let output_bytes =
            crate::middleware_manifest::usize_setting(&settings, "sandbox", "tool_output_bytes")?;
        let credential_environment = gateway_config
            .telemetry
            .sinks
            .iter()
            .filter_map(|sink| sink.bearer_env.as_deref())
            .collect::<Vec<_>>();
        let gateway_sandbox = Arc::new(
            GatewaySandbox::new_configured(
                &workspace_path,
                &state_dir,
                tls_key.as_deref(),
                &gateway_config.execution,
                output_bytes,
                &credential_environment,
            )?
            .with_desktop(desktop)
            .with_remote_desktop(remote_desktop)
            .deny_read_paths(
                gateway_config
                    .computer
                    .browser
                    .profile_directory
                    .iter()
                    .cloned()
                    .chain(gateway_config.telemetry.sinks.iter().filter_map(|sink| {
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
                    })),
            )?
            .allow_attached_folders(attached_folders.iter().cloned())?
            .allow_read_roots(read_roots)?,
        );
        let backend: Arc<dyn SandboxBackend> = gateway_sandbox.clone();
        let sandbox = Sandbox::new(Arc::clone(&backend), approval_policy)
            .tool_output_limit(output_bytes)?
            .background_command_limit(crate::middleware_manifest::usize_setting(
                &settings,
                "sandbox",
                "background_commands",
            )?)?;
        let sandbox = if attached_folders.is_empty() {
            sandbox
        } else {
            sandbox.attached_folders(workspace_path.clone(), attached_folders.clone())
        };
        let extensions = extensions
            .map(|extensions| {
                activate_extensions(
                    extensions,
                    resolved_extensions,
                    gateway_for_middleware,
                    &workspace_path,
                    backend,
                )
            })
            .transpose()?;
        let middleware = build_middleware(
            &resources,
            &workspace_path,
            scratchpad,
            session_files,
            extensions,
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
    let usage_store = store.clone();
    let max_model_steps =
        usize::try_from(prepared.bot.config.config.max_model_steps).map_err(|_| {
            Error::Config("maximum model steps exceed this platform's supported range".into())
        })?;
    let system_prompt = prepared.instructions();
    let mut agent_config = AgentConfig::new(
        models,
        Arc::clone(&sandbox),
        checkpoints,
        MiddlewareStack::new(entries.clone())?,
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
        let provider = model_providers.get(route).ok_or_else(|| {
            MobiusError::Config("model route is not in the configured gateway usage catalog".into())
        })?;
        persist_usage(&gateway, &usage_store, provider, usage)
    })
    .session_context(SessionContext {
        owner_id: chat.bot_id.clone(),
        user_name: local_user_name(),
        workspace_id: workspace.as_ref().map(|workspace| workspace.id.clone()),
        workspace_label: workspace
            .as_ref()
            .map(|workspace| workspace.path.display().to_string()),
        origin_label: Some(origin_label.into()),
        ..SessionContext::default()
    });
    let persistent = session_id.as_deref() == Some(prepared.bot.conversation_session_id.as_str());
    if persistent && (project || !chat.catalog_visible) {
        return Err(Error::Config(
            "Persistent Chat must be visible and project-free".into(),
        ));
    }
    if let Some(session_id) = session_id {
        agent_config = agent_config.session_id(session_id);
    }
    if let Some(template) = &template {
        template
            .set(agent_config.clone())
            .map_err(|_| Error::Config("subagent launcher was initialized twice".into()))?;
    }
    if persistent {
        entries.push(Arc::new(crate::persistent_chat::PersistentChat::new(
            prepared.bot.id.clone(),
            host_access
                .ok_or_else(|| Error::Config("Persistent Chat requires the gateway".into()))?,
        )));
        agent_config = agent_config.middleware(MiddlewareStack::new(entries)?);
    }
    let agent = create_agent(agent_config).await?;
    let model_router = agent.model_router();
    Ok(BuiltAgent {
        agent,
        model_router,
        sandbox,
        gateway_sandbox,
        subagents,
        subagent_template: template,
    })
}

pub(crate) fn persist_usage(
    gateway: &Mutex<GatewayConfig>,
    store: &ConfigStore,
    provider: &str,
    usage: &TokenUsage,
) -> mobius::Result<()> {
    let mut gateway = gateway
        .lock()
        .map_err(|_| MobiusError::Config("gateway configuration lock is poisoned".into()))?;
    let mut next = gateway.clone();
    if next
        .observe_usage(provider, usage)
        .map_err(|error| MobiusError::Config(error.to_string()))?
    {
        store
            .save(&next)
            .map_err(|error| MobiusError::Config(error.to_string()))?;
        *gateway = next;
    }
    Ok(())
}

fn subagent_launcher(template: &Arc<OnceLock<AgentConfig>>) -> SubagentLauncher {
    let template = Arc::downgrade(template);
    Arc::new(move |launch: SubagentLaunch| {
        let template = template.clone();
        Box::pin(async move {
            let config = template
                .upgrade()
                .ok_or_else(|| MobiusError::Stopped("subagent launcher stopped".into()))?
                .get()
                .ok_or_else(|| MobiusError::Config("subagent launcher is not ready".into()))?
                .clone()
                .isolated_execution()?
                .session_id(launch.session_id)
                .metadata(launch.metadata)
                .role(launch.role)
                .model_route(&launch.model, launch.reasoning_effort.as_deref())?;
            create_agent(config).await
        })
    })
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
        configured_model_routes(gateway, store, credentials)?
            .into_iter()
            .filter(|route| route.provider.instance != selection.instance),
    );
    catalog.sort_by_key(|route| route.choice.route != selected_route);
    if catalog.first().map(|route| route.choice.route.as_str()) != Some(selected_route.as_str()) {
        return Err(Error::Config(
            "active model route is not in the configured gateway catalog".into(),
        ));
    }
    let routes = instantiate_routes(catalog, store, credentials, &gateway.model_transport)?;
    let first = routes
        .first()
        .ok_or_else(|| Error::Config("provider has no model routes".into()))?;
    let context_window = first
        .choice
        .context_window
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let mut router =
        ModelRouter::new(&first.id, Arc::clone(&first.model)).session_files(session_files);
    for route in routes.iter().skip(1) {
        router.register(&route.id, Arc::clone(&route.model))?;
    }
    for route in routes {
        router.set_credential_lifetime(&route.id, route.lifetime)?;
        router.configure_choice(route.choice)?;
    }
    Ok((Arc::new(router), context_window))
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
    let mut routes = Vec::with_capacity(catalog.len());
    for route in catalog {
        let definition = provider(&route.provider.provider)?;
        let base_url = selected_base_url(definition, &route.provider).map(str::to_owned);
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
    let model = definition.build(ProviderBuildConfig {
        credential,
        model: route.provider.model,
        base_url,
        reasoning_effort: route.provider.reasoning_effort,
        service_tier: route.provider.service_tier,
        web_search: route.provider.web_search,
        http: http.clone(),
        transport: *transport,
    })?;
    let mut choice = route.choice;
    choice.supports_image_input = model.supports_image_input();
    choice.supports_image_generation = model.supports_image_generation();
    let id = choice.route.clone();
    Ok(RouteValue {
        choice,
        id,
        model,
        lifetime,
    })
}

struct RouteValue {
    lifetime: ModelCredentialLifetime,
    id: String,
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
    session_files: SessionFileStore,
) -> Result<(Arc<ModelRouter>, i64)> {
    let definition = provider(&selection.provider)?;
    let context_window = definition
        .model(&selection.model)
        .map_or(DEFAULT_CONTEXT_WINDOW, |preset| preset.context_window);
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
        context_window: Some(context_window),
        supports_image_input: definition.supports_image_input(),
        supports_image_generation: false,
        supports_realtime_voice: false,
        tool_discovery: definition.tool_discovery(&selection.model, selection.base_url.as_deref()),
    })?;
    Ok((Arc::new(router), context_window))
}

pub(crate) fn configured_compaction(settings: &MiddlewareConfig) -> Result<Compaction> {
    Ok(Compaction::new(crate::middleware_manifest::integer_setting(
        settings,
        "compaction",
        "at_tokens",
    )?)?
    .mode(
        crate::middleware_manifest::string_setting(settings, "compaction", "mode")?
            .ok_or_else(|| Error::Config("unsupported compaction mode".into()))?
            .parse::<CompactionMode>()?,
    )
    .keep_recent_tokens(crate::middleware_manifest::usize_setting(
        settings,
        "compaction",
        "keep_recent_tokens",
    )?)?
    .native_retained_tokens(crate::middleware_manifest::usize_setting(
        settings,
        "compaction",
        "native_retained_tokens",
    )?)?
    .reserve_tokens(crate::middleware_manifest::integer_setting(
        settings,
        "compaction",
        "reserve_tokens",
    )?)?
    .handoff_policy(
        crate::middleware_manifest::integer_setting(
            settings,
            "compaction",
            "handoff_reserve_divisor",
        )?,
        crate::middleware_manifest::integer_setting(
            settings,
            "compaction",
            "handoff_warning_reserves",
        )?,
        crate::middleware_manifest::integer_setting(
            settings,
            "compaction",
            "handoff_urgent_reserves",
        )?,
    )?)
}

fn build_middleware(
    prepared: &PreparedBot,
    workspace: &std::path::Path,
    scratchpad: ScratchpadStore,
    session_files: SessionFileStore,
    mut extensions: Option<Extensions>,
    mut live_chats: Option<Arc<dyn LiveChats>>,
    project: bool,
) -> Result<BuiltMiddleware> {
    let settings = &prepared.bot.config.config.middleware;
    let mut entries: Vec<Arc<dyn Middleware>> = Vec::new();
    let mut subagent_template = None;
    let mut subagents = None;
    for feature in MIDDLEWARE.iter().filter(|feature| {
        feature.manifest.required
            || settings.enabled(feature.manifest.id)
            || matches!(feature.kind, BuiltinMiddleware::Scratchpad)
    }) {
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
            BuiltinMiddleware::ImageGeneration => Arc::new(ImageGeneration::new(
                Arc::clone(&prepared.models),
                session_files.clone(),
            )),
            BuiltinMiddleware::Tools => Arc::new(if project {
                Tools::coding(session_files.clone())
            } else {
                Tools::reading(session_files.clone())
            }),
            BuiltinMiddleware::Instructions if !project => continue,
            BuiltinMiddleware::Instructions => Arc::new(Instructions::discover(workspace)?),
            BuiltinMiddleware::Scratchpad => Arc::new(
                Scratchpad::new(scratchpad.clone()).agent_enabled(settings.enabled("scratchpad")),
            ),
            BuiltinMiddleware::Extensions => Arc::new(
                extensions
                    .take()
                    .ok_or_else(|| Error::Config("extensions were not discovered".into()))?,
            ),
            BuiltinMiddleware::Tasks => Arc::new(Tasks),
            BuiltinMiddleware::Subagents => {
                let template = Arc::new(OnceLock::<AgentConfig>::new());
                let (max_depth, max_concurrency, max_agents) =
                    crate::middleware_manifest::subagent_limits(
                        settings,
                        prepared.subagent_ceilings,
                    )?;
                let middleware = Subagents::new_with_ceilings(
                    prepared.subagent_ceilings,
                    max_depth,
                    max_concurrency,
                    max_agents,
                    subagent_launcher(&template),
                )?
                .session_files(session_files.clone());
                let middleware = match crate::middleware_manifest::string_setting(
                    settings,
                    "subagents",
                    "model_route",
                )? {
                    Some(route) => middleware.default_model(route),
                    None => middleware,
                };
                let middleware = Arc::new(middleware);
                subagents = Some(Arc::clone(&middleware));
                subagent_template = Some(template);
                middleware
            }
            BuiltinMiddleware::Messages => Arc::new(Messages::new(
                crate::middleware_manifest::usize_setting(settings, "messages", "max_pending")?,
                prepared.active_message_delivery,
            )?),
            BuiltinMiddleware::ContextOffloading => Arc::new(ContextOffloading::new(
                crate::middleware_manifest::integer_setting(
                    settings,
                    "context_offloading",
                    "stale_after_tokens",
                )?,
            )?),
            BuiltinMiddleware::ComputerControl => {
                let runtime = prepared
                    .computer_runtime
                    .as_deref()
                    .ok_or_else(|| Error::Config("computer runtime was not prepared".into()))?;
                Arc::new(mobius::middleware::computer_control::ComputerControl::new(
                    session_files.clone(),
                    crate::computer_runtime::worker_command(runtime, &prepared.computer_config)?,
                    runtime.join("computer-control.md"),
                )?)
            }
            BuiltinMiddleware::Compaction => prepared
                .compaction
                .as_ref()
                .map(Arc::clone)
                .ok_or_else(|| Error::Config("compaction policy was not prepared".into()))?,
            BuiltinMiddleware::Sessions => {
                let sessions = Sessions::new(crate::middleware_manifest::usize_setting(
                    settings,
                    "sessions",
                    "page_size",
                )?)?
                .session_files(session_files.clone());
                Arc::new(match live_chats.take() {
                    Some(chats) => sessions.live_chats(chats),
                    None => sessions,
                })
            }
        };
        entries.push(middleware);
    }
    Ok(BuiltMiddleware {
        entries,
        subagent_template,
        subagents,
    })
}

fn activate_extensions(
    extensions: Extensions,
    resolved: &ResolvedExtensions,
    gateway: Arc<Mutex<GatewayConfig>>,
    workspace: &std::path::Path,
    backend: Arc<dyn SandboxBackend>,
) -> Result<Extensions> {
    extensions
        .activate_plugins(
            resolved
                .plugins
                .iter()
                .map(|plugin| plugin.activation(Arc::clone(&gateway))),
            workspace,
            backend,
        )
        .map_err(Error::from)
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
