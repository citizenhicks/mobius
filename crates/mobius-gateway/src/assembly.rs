use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use mobius::Error as MobiusError;
use mobius::agent::{Agent, AgentConfig, create_agent};
use mobius::backend::checkpoint::CheckpointStore;
use mobius::backend::model::provider::{
    HttpClient, ProviderAuth, ProviderBuildConfig, ProviderCredential, ProviderDefinition,
    provider, streaming_client,
};
use mobius::backend::model::{Model, ModelEventSink, ModelOutput, ModelRequest, ModelRouter};
use mobius::backend::sandbox::{ApprovalPolicy, Sandbox, SandboxBackend};
use mobius::middleware::artifacts::Artifacts;
use mobius::middleware::attachments::Attachments;
use mobius::middleware::bots::{Bots, BotsBackend};
use mobius::middleware::compaction::Compaction;
use mobius::middleware::context_offloading::ContextOffloading;
use mobius::middleware::extensions::{Extensions, MANIFEST as EXTENSIONS_MANIFEST};
use mobius::middleware::instructions::Instructions;
use mobius::middleware::messages::Messages;
use mobius::middleware::scratchpad::{Scratchpad, ScratchpadStore};
use mobius::middleware::session_files::SessionFileStore;
use mobius::middleware::sessions::Sessions;
use mobius::middleware::subagents::{SubagentLaunch, SubagentLauncher, Subagents};
use mobius::middleware::tasks::Tasks;
use mobius::middleware::tools::Tools;
use mobius::middleware::{Middleware, MiddlewareStack};
use mobius::protocol::{ActiveMessageDelivery, ModelChoice, ModelInfo, SessionContext, TokenUsage};

use crate::config::{
    ChatSpec, ConfigStore, CredentialStore, DEFAULT_CONTEXT_WINDOW, GatewayConfig,
    effective_reasoning_effort, local_user_name, model_route_id,
};
use crate::extensions::{ExtensionStore, ResolvedExtensions};
use crate::middleware_manifest::{BuiltinMiddleware, MIDDLEWARE};
use crate::provider_catalog::{
    CatalogRoute, catalog_routes, configured_model_providers, configured_model_routes,
    credential_is_configured, selected_base_url,
};
use crate::sandbox::GatewaySandbox;
use crate::wire::{MiddlewareConfig, ProviderConfig, ProviderEndpointAuth, validate_session_id};
use crate::{Error, Result};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) struct BuiltAgent {
    pub(crate) agent: Agent,
    pub(crate) model_router: Arc<ModelRouter>,
    pub(crate) gateway_sandbox: Arc<GatewaySandbox>,
    pub(crate) subagent_template: Option<Arc<OnceLock<AgentConfig>>>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "the headless composition root keeps its runtime dependencies explicit"
)]
pub(crate) async fn assemble(
    gateway: Arc<Mutex<GatewayConfig>>,
    chat: &ChatSpec,
    store: &ConfigStore,
    credentials: Arc<CredentialStore>,
    checkpoints: Arc<dyn CheckpointStore>,
    scratchpad: ScratchpadStore,
    session_files: SessionFileStore,
    swarm: Arc<dyn BotsBackend>,
    session_id: Option<String>,
    origin_label: &str,
    override_saved_model_route: bool,
    reusable_model_router: Option<Arc<ModelRouter>>,
) -> Result<BuiltAgent> {
    if let Some(session_id) = session_id.as_deref() {
        validate_session_id(session_id)?;
    }
    let gateway_config = gateway
        .lock()
        .map_err(|_| Error::Config("gateway configuration lock is poisoned".into()))?
        .clone();
    let model_providers = configured_model_providers(&gateway_config, store, &credentials)?;
    let (models, context_window) = if let Some(models) = reusable_model_router {
        let context_window = models
            .choices()
            .find(|choice| choice.route == models.default_provider())
            .and_then(|choice| choice.context_window)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        (models, context_window)
    } else if credential_is_configured(&chat.agent.config.provider, store, &credentials)? {
        build_models(
            &gateway_config,
            &chat.agent.config.provider,
            store,
            &credentials,
        )?
    } else {
        unavailable_models(&gateway_config, &chat.agent.config.provider)?
    };
    let resolved_extensions =
        ExtensionStore::new(store).resolve(&gateway_config, &chat.agent.config.extensions)?;
    let extensions = (EXTENSIONS_MANIFEST.required
        || chat.agent.config.middleware.enabled(EXTENSIONS_MANIFEST.id))
    .then(|| {
        Extensions::discover_installed(
            [
                chat.workspace.join(".agents/skills"),
                chat.workspace.join(".codex/skills"),
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
    let gateway_sandbox = Arc::new(
        GatewaySandbox::new(
            &chat.workspace,
            store.state_dir(),
            gateway_config
                .tls
                .as_ref()
                .map(|tls| tls.private_key.as_path()),
            COMMAND_TIMEOUT,
        )?
        .allow_read_roots(read_roots)?,
    );
    let backend: Arc<dyn SandboxBackend> = gateway_sandbox.clone();
    let model_choices = models.choices().cloned().collect::<Vec<_>>();
    crate::middleware_manifest::validate_choices(&chat.agent.config.middleware, &model_choices)?;
    let approval_policy = crate::middleware_manifest::string_setting(
        &chat.agent.config.middleware,
        "sandbox",
        "approval_policy",
    )?
    .ok_or_else(|| Error::Config("missing middleware setting `sandbox.approval_policy`".into()))?
    .parse::<ApprovalPolicy>()?;
    let sandbox = Arc::new(Sandbox::new(Arc::clone(&backend), approval_policy));
    let (middleware, template) = build_middleware(
        &chat.agent.config.middleware,
        &chat.workspace,
        &chat.bot_id,
        chat.catalog_visible,
        Arc::clone(&gateway),
        scratchpad,
        session_files,
        swarm,
        backend,
        &resolved_extensions,
        extensions,
    )?;
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
    let max_model_steps = usize::try_from(chat.agent.config.max_model_steps).map_err(|_| {
        Error::Config("maximum model steps exceed this platform's supported range".into())
    })?;
    let system_prompt = format!(
        "{}\n\n{}",
        chat.bot_description, chat.agent.config.system_prompt
    );
    let mut agent_config =
        AgentConfig::new(models, sandbox, checkpoints, middleware, system_prompt)
            .context_window(context_window)
            .catalog_visible(chat.catalog_visible)
            .initial_replay_batches(0)
            .max_model_steps(max_model_steps)
            .metadata(metadata)
            .usage_observer(move |route, usage| {
                persist_usage(&gateway, &usage_store, &model_providers, route, usage)
            })
            .session_context(SessionContext {
                bot_id: chat.bot_id.clone(),
                user_name: local_user_name(),
                workspace_id: Some(workspace.id),
                workspace_label: Some(workspace.path.display().to_string()),
                origin_label: Some(origin_label.into()),
                ..SessionContext::default()
            });
    if let Some(session_id) = session_id {
        agent_config = agent_config.session_id(session_id);
    }
    if override_saved_model_route {
        agent_config = agent_config.override_saved_model_route();
    }
    if let Some(template) = &template {
        template
            .set(agent_config.clone())
            .map_err(|_| Error::Config("subagent launcher was initialized twice".into()))?;
    }
    let agent = create_agent(agent_config).await?;
    let model_router = agent.model_router();
    Ok(BuiltAgent {
        agent,
        model_router,
        gateway_sandbox,
        subagent_template: template,
    })
}

fn persist_usage(
    gateway: &Mutex<GatewayConfig>,
    store: &ConfigStore,
    model_providers: &BTreeMap<String, String>,
    route: &str,
    usage: &TokenUsage,
) -> mobius::Result<()> {
    let provider = model_providers.get(route).ok_or_else(|| {
        MobiusError::Config("model route is not in the configured gateway usage catalog".into())
    })?;
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
    let routes = instantiate_routes(catalog, store, credentials)?;
    let first = routes
        .first()
        .ok_or_else(|| Error::Config("provider has no model routes".into()))?;
    let context_window = first
        .choice
        .context_window
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let mut router = ModelRouter::new(&first.id, Arc::clone(&first.model));
    for route in routes.iter().skip(1) {
        router.register(&route.id, Arc::clone(&route.model))?;
    }
    for route in routes {
        router.configure_choice(route.choice)?;
    }
    Ok((Arc::new(router), context_window))
}

fn instantiate_routes(
    catalog: Vec<CatalogRoute>,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<Vec<RouteValue>> {
    let http = streaming_client()?;
    let mut provider_credentials = BTreeMap::<String, ProviderCredential>::new();
    let mut routes = Vec::with_capacity(catalog.len());
    for route in catalog {
        let definition = provider(&route.provider.provider)?;
        let base_url = selected_base_url(definition, &route.provider).map(str::to_owned);
        let credential = if route.provider.endpoint_auth == ProviderEndpointAuth::Credentialless {
            ProviderCredential::Credentialless
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
                    )?;
                    provider_credentials
                        .insert(route.provider.instance.clone(), credential.clone());
                    credential
                }
            }
        };
        routes.push(build_route(route, definition, credential, base_url, &http)?);
    }
    Ok(routes)
}

fn resolve_credential(
    instance: &str,
    definition: &ProviderDefinition,
    base_url: Option<&str>,
    store: &ConfigStore,
    credentials: &CredentialStore,
) -> Result<ProviderCredential> {
    match definition.auth() {
        ProviderAuth::ApiKey(default_env) => {
            if let Some(value) = credentials.get(instance, definition.id(), base_url)? {
                return Ok(ProviderCredential::ApiKey(value));
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
            Ok(ProviderCredential::ApiKey(value))
        }
        ProviderAuth::Browser(auth) => auth.load(&store.provider_auth_path()).map_err(Error::from),
    }
}

fn build_route(
    route: CatalogRoute,
    definition: &'static ProviderDefinition,
    credential: ProviderCredential,
    base_url: Option<String>,
    http: &HttpClient,
) -> Result<RouteValue> {
    let model = definition.build(ProviderBuildConfig {
        credential,
        model: route.provider.model,
        base_url,
        reasoning_effort: route.provider.reasoning_effort,
        web_search: route.provider.web_search,
        http: http.clone(),
    })?;
    let mut choice = route.choice;
    choice.supports_image_input = model.supports_image_input();
    let id = choice.route.clone();
    Ok(RouteValue { choice, id, model })
}

struct RouteValue {
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
                .and_then(|preset| preset.default_reasoning.map(str::to_string))
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
    let mut router = ModelRouter::new(&route, model);
    router.configure_choice(ModelChoice {
        route,
        group: selection.model.clone(),
        model: selection.model.clone(),
        reasoning_effort: effort,
        context_window: Some(context_window),
        supports_image_input: definition.supports_image_input(),
        tool_discovery: definition.tool_discovery(&selection.model, selection.base_url.as_deref()),
    })?;
    Ok((Arc::new(router), context_window))
}

#[expect(
    clippy::too_many_arguments,
    reason = "the headless composition root keeps middleware dependencies explicit"
)]
fn build_middleware(
    settings: &MiddlewareConfig,
    workspace: &std::path::Path,
    bot_id: &str,
    catalog_visible: bool,
    gateway: Arc<Mutex<GatewayConfig>>,
    scratchpad: ScratchpadStore,
    session_files: SessionFileStore,
    swarm: Arc<dyn BotsBackend>,
    backend: Arc<dyn SandboxBackend>,
    resolved_extensions: &ResolvedExtensions,
    mut extensions: Option<Extensions>,
) -> Result<(MiddlewareStack, Option<Arc<OnceLock<AgentConfig>>>)> {
    let mut entries: Vec<Arc<dyn Middleware>> = Vec::new();
    let mut subagent_template = None;
    for feature in MIDDLEWARE.iter().filter(|feature| {
        feature.manifest.required
            || settings.enabled(feature.manifest.id)
            || matches!(feature.kind, BuiltinMiddleware::Scratchpad)
    }) {
        let middleware: Arc<dyn Middleware> = match feature.kind {
            BuiltinMiddleware::Sandbox => continue,
            BuiltinMiddleware::Attachments => {
                Arc::new(Attachments::new(session_files.clone()).with_workspace(workspace)?)
            }
            BuiltinMiddleware::Artifacts => Arc::new(Artifacts::new(session_files.clone())),
            BuiltinMiddleware::Tools => Arc::new(Tools::coding()),
            BuiltinMiddleware::Instructions => Arc::new(Instructions::discover(workspace)?),
            BuiltinMiddleware::Scratchpad => Arc::new(
                Scratchpad::new(scratchpad.clone(), Arc::clone(&swarm), bot_id.to_owned())
                    .agent_enabled(settings.enabled("scratchpad")),
            ),
            BuiltinMiddleware::Extensions => Arc::new(
                extensions
                    .take()
                    .ok_or_else(|| Error::Config("extensions were not discovered".into()))?
                    .activate_plugins(
                        resolved_extensions
                            .plugins
                            .iter()
                            .map(|plugin| plugin.activation(Arc::clone(&gateway))),
                        workspace,
                        Arc::clone(&backend),
                    )?,
            ),
            BuiltinMiddleware::Tasks => Arc::new(Tasks),
            BuiltinMiddleware::Subagents => {
                let template = Arc::new(OnceLock::<AgentConfig>::new());
                let max_depth = u8::try_from(crate::middleware_manifest::integer_setting(
                    settings,
                    "subagents",
                    "max_depth",
                )?)
                .map_err(|_| {
                    Error::Config("subagent max depth must fit an unsigned byte".into())
                })?;
                let middleware = Subagents::new(
                    max_depth,
                    crate::middleware_manifest::usize_setting(
                        settings,
                        "subagents",
                        "max_concurrency",
                    )?,
                    crate::middleware_manifest::usize_setting(settings, "subagents", "max_agents")?,
                    subagent_launcher(&template),
                )?;
                let middleware = match crate::middleware_manifest::string_setting(
                    settings,
                    "subagents",
                    "model_route",
                )? {
                    Some(route) => middleware.default_model(route),
                    None => middleware,
                };
                subagent_template = Some(template);
                Arc::new(middleware)
            }
            BuiltinMiddleware::Messages => {
                let delivery = match crate::middleware_manifest::string_setting(
                    settings, "messages", "delivery",
                )? {
                    Some("steer") => ActiveMessageDelivery::Steer,
                    Some("queue") => ActiveMessageDelivery::Queue,
                    Some(value) => {
                        return Err(Error::Config(format!(
                            "unsupported messages delivery `{value}`"
                        )));
                    }
                    None => {
                        return Err(Error::Config(
                            "missing middleware setting `messages.delivery`".into(),
                        ));
                    }
                };
                Arc::new(Messages::new(
                    crate::middleware_manifest::usize_setting(settings, "messages", "max_pending")?,
                    delivery,
                )?)
            }
            BuiltinMiddleware::ContextOffloading => Arc::new(ContextOffloading::new(
                crate::middleware_manifest::integer_setting(
                    settings,
                    "context_offloading",
                    "stale_after_tokens",
                )?,
            )?),
            BuiltinMiddleware::Compaction => Arc::new(Compaction::new(
                crate::middleware_manifest::integer_setting(settings, "compaction", "at_tokens")?,
            )?),
            BuiltinMiddleware::Sessions => Arc::new(Sessions::new(
                crate::middleware_manifest::usize_setting(settings, "sessions", "page_size")?,
            )?),
            BuiltinMiddleware::Bots => {
                let bots = Bots::new(Arc::clone(&swarm), bot_id.to_owned());
                Arc::new(if catalog_visible {
                    bots.with_routine_creation(workspace)
                } else {
                    bots
                })
            }
        };
        entries.push(middleware);
    }
    Ok((MiddlewareStack::new(entries)?, subagent_template))
}

#[cfg(test)]
mod tests;
