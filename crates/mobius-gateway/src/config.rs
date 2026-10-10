//! Validated gateway configuration and owner-only persistence.

mod runtime;
mod store;
mod validation;
mod workspace;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::Read as _;
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use mobius::agent::DEFAULT_MAX_MODEL_STEPS;
use mobius::backend::model::provider::{
    ModelPreset, ProviderAuth, ProviderDefinition, default_provider, provider,
};
use mobius::protocol::TokenUsage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest as _;

pub(crate) use crate::wire::ConfiguredModel;
#[cfg(test)]
use crate::wire::RoutineInteractionPolicy;
use crate::wire::{
    AgentComposition, DailyUsage, ProfileSnapshot, ProviderConfig, ProviderEndpointAuth,
    ProviderTint, VersionedAgentConfig, WorkspaceInfo,
};
use crate::{Error, Result};

pub use self::runtime::RuntimeConfig;
use self::store::*;
pub use self::store::{
    ConfigStore, CredentialStore, ResolvedCredential, load_secret_file, state_dir,
};
pub use self::validation::validate_agent_composition;
use self::validation::*;
pub(crate) use self::validation::{
    configured_approval_policy, effective_reasoning_effort, model_route_id,
    validate_bot_compatibility,
};
pub(crate) use self::workspace::{
    create_workspace_directory, local_user_name, validate_chat_workspace, workspace_id,
};
pub use crate::server::ConnectionPolicy;

const CONFIG_VERSION: u32 = 29;
pub(crate) const MAX_CAPACITY: usize = 4_096;

pub(crate) fn bounded<T>(name: &str, value: T, range: std::ops::RangeInclusive<T>) -> Result<()>
where
    T: Copy + PartialOrd + std::fmt::Display,
{
    if !range.contains(&value) {
        return Err(Error::Config(format!(
            "{name} must be between {} and {}",
            range.start(),
            range.end()
        )));
    }
    Ok(())
}
const CHAT_SPEC_VERSION: u32 = 15;
pub(crate) const CHAT_SPEC_METADATA_KEY: &str = "mobius_gateway.chat";
const CONFIG_FILE: &str = "gateway.toml";
const CLOUDFLARE_TOKEN_FILE: &str = "cloudflare-token";
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_CREDENTIAL_STATE_BYTES: usize = 256 * 1024;
const MAX_SYSTEM_PROMPT_BYTES: usize = 64 * 1024;
/// Maximum UTF-8 byte length accepted for a provider credential.
pub const MAX_PROVIDER_API_KEY_BYTES: usize = 16 * 1024;
const MAX_PROVIDER_CATALOG_ENTRIES: usize = 64;
const MAX_PROVIDER_CATALOG_ENTRY_BYTES: usize = 1024;
const MAX_PROVIDER_CATALOG_BYTES: usize = 16 * 1024;
const MAX_CUSTOM_MODEL_ROUTES: usize = 64;
/// Maximum UTF-8 byte length accepted for a Cloudflare tunnel token.
pub const MAX_CLOUDFLARE_TOKEN_BYTES: usize = 16 * 1024;
/// Maximum UTF-8 byte length accepted for a provider label.
pub const MAX_PROVIDER_LABEL_BYTES: usize = 128;
/// Maximum byte length accepted for a named Cloudflare tunnel hostname.
pub const MAX_HOSTNAME_BYTES: usize = 253;
const MAX_WORKSPACE_DIRECTORY_NAME_BYTES: usize = 255;
const MAX_ATTACHED_FOLDERS: usize = 8;
const SECONDS_PER_DAY: u64 = 86_400;
const USAGE_HISTORY_DAYS: u64 = 52 * 7;

/// Default loopback listener used by a local gateway.
pub const DEFAULT_LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 8741);

/// Default system prompt installed by `mobius-gateway init`.
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("config/default_prompt.md");

/// Context window used for custom models without an advertised preset.
pub const DEFAULT_CONTEXT_WINDOW: i64 = 272_000;

/// Certificate paths required by a TLS listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// The certificate.
    pub certificate: PathBuf,
    /// The private key.
    pub private_key: PathBuf,
}

/// Cloudflare Tunnel exposure selected for this gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum CloudflareConfig {
    /// Account-free tunnel with an address assigned at process startup.
    Quick,
    /// User-owned tunnel with a stable published hostname.
    Named {
        /// The published tunnel hostname.
        hostname: String,
    },
}

/// Durable machine-wide settings and defaults for one gateway process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    version: u32,
    /// Gateway lifecycle and private ingress.
    #[serde(default)]
    pub runtime: RuntimeConfig,
    /// Connection and active-chat resource limits.
    #[serde(default)]
    pub connections: ConnectionPolicy,
    /// Operator-controlled pairing policy.
    #[serde(default)]
    pub auth: crate::auth::AuthConfig,
    /// Computer runtime and desktop launch options.
    #[serde(default)]
    pub computer: crate::computer_runtime::ComputerConfig,
    /// Model network and retry policy.
    #[serde(default)]
    pub model_transport: mobius::backend::model::ModelTransportSettings,
    /// Host execution policy independent of Bot permissions.
    #[serde(default)]
    pub execution: crate::sandbox::ExecutionConfig,
    /// Explicit outbound collectors.
    #[serde(default)]
    pub telemetry: crate::telemetry::TelemetryConfig,
    /// The listen.
    pub listen: SocketAddr,
    /// The TLS.
    pub tls: Option<TlsConfig>,
    /// The cloudflare.
    pub cloudflare: Option<CloudflareConfig>,
    /// Own a persistent headed browser and, on Linux, a private virtual desktop.
    #[serde(default)]
    pub desktop_enabled: bool,
    /// The bot defaults.
    pub bot_defaults: Option<VersionedAgentConfig>,
    pub(crate) configured_providers: BTreeMap<String, ConfiguredProvider>,
    pub(crate) installed_extensions: BTreeMap<String, crate::extensions::InstalledExtension>,
    usage: UsageHistory,
}

/// One durable provider selection and its gateway model and reasoning catalogs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfiguredProvider {
    pub(crate) selection: ProviderConfig,
    pub(crate) label: String,
    pub(crate) tint: ProviderTint,
    #[serde(default)]
    pub(crate) models: Vec<ModelPreset>,
    /// Nonempty explicit IDs replace the provider's built-in image presets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) image_model_ids: Vec<String>,
    /// Omission uses the provider catalog; an empty list disables image generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) image_models: Option<Vec<mobius::backend::model::provider::MediaModelPreset>>,
    /// Omission uses the provider catalog; an empty list disables live voice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) voice_models: Option<Vec<mobius::backend::model::provider::MediaModelPreset>>,
}

pub(crate) struct ProviderRegistration {
    pub(crate) selection: ProviderConfig,
    pub(crate) label: Option<String>,
    pub(crate) tint: Option<ProviderTint>,
    pub(crate) models: Vec<ConfiguredModel>,
    pub(crate) image_model_ids: Option<Vec<String>>,
}

/// Chat ownership and workspace, independent of Bot configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ChatSpec {
    version: u32,
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) attached_folders: Vec<PathBuf>,
    pub(crate) bot_id: String,
    #[serde(skip)]
    pub(crate) catalog_visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredChatSpec {
    version: u32,
    workspace: Option<PathBuf>,
    attached_folders: Vec<PathBuf>,
    bot_id: String,
}

impl Default for AgentComposition {
    fn default() -> Self {
        Self::defaults_with_ceilings(mobius::middleware::subagents::SubagentCeilings::default())
    }
}

impl AgentComposition {
    pub(crate) fn defaults_with_ceilings(
        ceilings: mobius::middleware::subagents::SubagentCeilings,
    ) -> Self {
        let provider = default_provider();
        let model = provider.default_model().and_then(|id| provider.model(id));
        let mut middleware = crate::middleware_manifest::default_config(ceilings);
        middleware.set_setting(
            "sandbox",
            "approval_policy",
            Some(mobius::protocol::FrontendSettingValue::String(
                "full_access".into(),
            )),
        );
        Self {
            provider: ProviderConfig {
                instance: provider.id().into(),
                provider: provider.id().into(),
                model: model.map_or_else(String::new, |model| model.id.as_str().into()),
                base_url: provider.default_base_url().map(str::to_string),
                endpoint_auth: ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: model
                    .and_then(|model| model.default_reasoning.as_deref())
                    .map(Into::into),
                service_tier: None,
                web_search: *provider
                    .web_search()
                    .first()
                    .expect("default provider web-search manifest"),
                tool_discovery: None,
            },
            middleware,
            extensions: BTreeSet::new(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            max_model_steps: DEFAULT_MAX_MODEL_STEPS as u64,
        }
    }
}

impl GatewayConfig {
    /// Builds validated machine-wide settings and Bot-creation defaults.
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or a required resource cannot be initialized.
    pub fn new(listen: SocketAddr, tls: Option<TlsConfig>) -> Result<Self> {
        let config = Self {
            version: CONFIG_VERSION,
            runtime: Default::default(),
            connections: Default::default(),
            auth: Default::default(),
            computer: Default::default(),
            model_transport: Default::default(),
            execution: Default::default(),
            telemetry: Default::default(),
            listen,
            tls,
            cloudflare: None,
            desktop_enabled: false,
            bot_defaults: None,
            configured_providers: BTreeMap::new(),
            installed_extensions: BTreeMap::new(),
            usage: UsageHistory::default(),
        };
        config.validate()?;
        Ok(config)
    }

    /// Builds a loopback gateway exposed through Cloudflare Tunnel.
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or a required resource cannot be initialized.
    pub fn new_cloudflare(listen: SocketAddr, cloudflare: CloudflareConfig) -> Result<Self> {
        let mut config = Self::new(listen, None)?;
        config.cloudflare = Some(cloudflare);
        config.validate()?;
        Ok(config)
    }

    /// Registers one configured provider and establishes the first Bot defaults.
    #[cfg(test)]
    pub(crate) fn registering_provider(
        &self,
        selection: ProviderConfig,
        label: String,
        tint: ProviderTint,
        models: Vec<ConfiguredModel>,
        image_model_ids: Vec<String>,
    ) -> Result<Self> {
        self.registering_configured(
            ProviderRegistration {
                selection,
                label: Some(label),
                tint: Some(tint),
                models,
                image_model_ids: Some(image_model_ids),
            },
            false,
        )
    }

    /// Registers one provider setup and establishes the first Bot defaults.
    pub(crate) fn registering_configured(
        &self,
        mut configured: ProviderRegistration,
        preserve_selection: bool,
    ) -> Result<Self> {
        let definition = provider(&configured.selection.provider)?;
        if let Some(current) = self
            .configured_providers
            .get(&configured.selection.instance)
            && current.selection.provider != configured.selection.provider
        {
            return Err(Error::Config(format!(
                "provider instance `{}` already belongs to `{}`",
                configured.selection.instance, current.selection.provider
            )));
        }
        let mut next = self.clone();
        let mut previous = next
            .configured_providers
            .remove(&configured.selection.instance);
        if let Some(previous) = &mut previous {
            if preserve_selection {
                configured.selection.model = std::mem::take(&mut previous.selection.model);
                configured.selection.reasoning_effort = previous.selection.reasoning_effort.take();
                configured.selection.web_search = previous.selection.web_search;
                configured.label = None;
                configured.tint = None;
                configured.models.clear();
                configured.image_model_ids = None;
            }
            if configured.selection.tool_discovery.is_none() {
                configured.selection.tool_discovery = previous.selection.tool_discovery;
            }
        }
        let label = configured
            .label
            .or_else(|| {
                previous
                    .as_mut()
                    .map(|previous| std::mem::take(&mut previous.label))
            })
            .unwrap_or_else(|| definition.label().to_owned());
        let tint = configured
            .tint
            .or_else(|| previous.as_ref().map(|previous| previous.tint))
            .unwrap_or_default();
        let image_model_ids = configured
            .image_model_ids
            .or_else(|| {
                previous
                    .as_mut()
                    .map(|previous| std::mem::take(&mut previous.image_model_ids))
            })
            .unwrap_or_default();
        let image_models = previous
            .as_mut()
            .and_then(|previous| previous.image_models.take());
        let voice_models = previous
            .as_mut()
            .and_then(|previous| previous.voice_models.take());
        let selection = &configured.selection;
        if !selection.model.is_empty() {
            definition.build_config_is_valid(
                &selection.model,
                selection.base_url.as_deref(),
                selection.reasoning_effort.as_deref(),
                selection.web_search,
            )?;
        }
        let default_selection = (self.bot_defaults.is_none() && !selection.model.is_empty())
            .then(|| configured.selection.clone());
        let models = if configured.models.is_empty() {
            previous.map_or_else(
                || {
                    if definition.locked_models() || configured.selection.model.is_empty() {
                        Vec::new()
                    } else if !definition.models().is_empty() {
                        definition.models().to_vec()
                    } else {
                        crate::provider_catalog::prepare_configured_models(
                            definition,
                            vec![ConfiguredModel {
                                id: configured.selection.model.clone(),
                                ..Default::default()
                            }],
                            Vec::new(),
                        )
                    }
                },
                |previous| previous.models,
            )
        } else {
            crate::provider_catalog::prepare_configured_models(
                definition,
                configured.models,
                previous.map_or_else(Vec::new, |previous| previous.models),
            )
        };
        next.configured_providers.insert(
            configured.selection.instance.clone(),
            ConfiguredProvider {
                selection: configured.selection,
                label,
                tint,
                models,
                image_model_ids,
                image_models,
                voice_models,
            },
        );
        if let Some(selection) = default_selection {
            let config = AgentComposition {
                provider: selection,
                ..AgentComposition::defaults_with_ceilings(next.execution.subagent_ceilings)
            };
            next.bot_defaults = Some(VersionedAgentConfig {
                revision: 1,
                config,
            });
        }
        next.validate()?;
        Ok(next)
    }

    /// Removes one non-default provider and lets middleware inherit the primary model.
    pub(crate) fn removing_provider(&self, instance: &str) -> Result<Self> {
        if !self.configured_providers.contains_key(instance) {
            return Err(Error::Config(format!(
                "provider instance `{instance}` is not configured"
            )));
        }
        if self
            .bot_defaults
            .as_ref()
            .is_some_and(|default| default.config.provider.instance == instance)
        {
            return Err(Error::Config(
                "choose another provider for Bot defaults before removing this provider".into(),
            ));
        }
        let mut next = self.clone();
        next.configured_providers.remove(instance);
        if let Some(mut default) = next.bot_defaults.take() {
            if clear_missing_model_routes(&mut default.config, &next)? {
                default.revision = default
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| Error::Config("configuration revision overflow".into()))?;
            }
            next.bot_defaults = Some(default);
        }
        next.validate()?;
        Ok(next)
    }

    /// Replaces only the defaults copied into future chats.
    pub(crate) fn replacing_bot_defaults(
        &self,
        expected_revision: u64,
        composition: AgentComposition,
    ) -> Result<Self> {
        let current = self
            .bot_defaults
            .as_ref()
            .ok_or_else(|| Error::Config("configure a provider before saving defaults".into()))?;
        if current.revision != expected_revision {
            return Err(Error::Config(format!(
                "configuration revision changed from {expected_revision} to {}",
                current.revision
            )));
        }
        let mut next = self.clone();
        next.bot_defaults = Some(VersionedAgentConfig {
            revision: current
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Config("configuration revision overflow".into()))?,
            config: composition,
        });
        next.validate()?;
        Ok(next)
    }

    pub(crate) fn validate_provider_selection(&self, selection: &ProviderConfig) -> Result<()> {
        validate_provider_config(selection)?;
        let configured = self
            .configured_providers
            .get(&selection.instance)
            .ok_or_else(|| {
                Error::Config("provider selection must use a configured provider entry".into())
            })?;
        validate_configured_provider_selection(configured, selection)
    }

    /// Records one live token-usage increment and reports whether daily usage changed.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn observe_usage(&mut self, provider: &str, usage: &TokenUsage) -> Result<bool> {
        self.usage.observe(provider, usage, SystemTime::now())
    }

    /// Returns frontend-safe local identity and daily aggregate usage.
    #[must_use]
    pub fn profile(&self) -> ProfileSnapshot {
        ProfileSnapshot {
            user_name: local_user_name(),
            daily_usage: self
                .usage
                .days
                .iter()
                .flat_map(|(unix_day, providers)| {
                    providers.iter().map(|(provider, usage)| DailyUsage {
                        unix_day: *unix_day,
                        provider: provider.clone(),
                        usage: usage.clone(),
                    })
                })
                .collect(),
            provider_usage: Vec::new(),
            run_stats: crate::wire::RunStats::default(),
            recent_run_groups: Vec::new(),
        }
    }

    /// Validates every persisted trust-boundary field.
    /// # Errors
    ///
    /// Returns an error if the supplied value is invalid.
    pub fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION {
            return Err(Error::Config(format!(
                "unsupported gateway config version {}",
                self.version
            )));
        }
        validate_telemetry(&self.telemetry)?;
        self.runtime.validate()?;
        self.connections.validate()?;
        self.auth.validate()?;
        self.computer.validate()?;
        self.model_transport.validate()?;
        self.execution.validate()?;
        if let Some(ingress) = self.runtime.ingress
            && (ingress == self.listen
                || ingress.port() == 0
                || self.tls.is_some()
                || self.cloudflare.is_some())
        {
            return Err(Error::Config(
                "ingress must differ from listen and cannot use TLS or Cloudflare".into(),
            ));
        }
        if self.listen.port() == 0 {
            return Err(Error::Config(
                "gateway listen port must be greater than zero".into(),
            ));
        }
        match (&self.tls, self.listen.ip().is_loopback()) {
            (None, false) => {
                return Err(Error::Config(
                    "non-loopback gateway listeners require a TLS certificate and private key"
                        .into(),
                ));
            }
            (Some(tls), _) => tls.validate()?,
            (None, true) => {}
        }
        if self.cloudflare.is_some() && (!self.listen.ip().is_loopback() || self.tls.is_some()) {
            return Err(Error::Config(
                "Cloudflare gateways require a plaintext loopback listener".into(),
            ));
        }
        if let Some(cloudflare) = &self.cloudflare {
            cloudflare.validate()?;
        }
        let has_chat = self
            .configured_providers
            .values()
            .any(|configured| !configured.selection.model.is_empty());
        if self.bot_defaults.is_some() != has_chat {
            return Err(Error::Config(
                "Bot defaults must exist exactly when a chat provider is configured".into(),
            ));
        }
        for (instance, configured) in &self.configured_providers {
            if instance != &configured.selection.instance {
                return Err(Error::Config(format!(
                    "configured provider key `{instance}` does not match `{}`",
                    configured.selection.instance
                )));
            }
            validate_configured_provider(configured)?;
        }
        crate::extensions::validate_installed(&self.installed_extensions)?;
        validate_custom_model_route_count(&self.configured_providers)?;
        if let Some(default) = &self.bot_defaults {
            if default.revision == 0 {
                return Err(Error::Config(
                    "configuration revision must be positive".into(),
                ));
            }
            validate_agent_composition_with_ceilings(
                &default.config,
                self.execution.subagent_ceilings,
            )?;
            self.validate_provider_selection(&default.config.provider)?;
            for (middleware, setting, route) in
                crate::middleware_manifest::configured_model_routes(&default.config.middleware)
            {
                if !crate::provider_catalog::configured_route_exists(self, route)? {
                    return Err(Error::Config(format!(
                        "Bot default middleware setting `{middleware}.{setting}` is not a configured model route"
                    )));
                }
            }
        }
        for providers in self.usage.days.values() {
            for (provider, usage) in providers {
                validate_usage_provider(provider)?;
                validate_usage(usage)?;
            }
        }
        Ok(())
    }
}

impl ChatSpec {
    pub(crate) fn for_bot(
        workspace: &Path,
        bot: &crate::wire::BotRecord,
        state_dir: &Path,
        tls: Option<&TlsConfig>,
    ) -> Result<Self> {
        let spec = Self {
            version: CHAT_SPEC_VERSION,
            workspace: Some(validate_chat_workspace(workspace, state_dir, tls)?),
            attached_folders: Vec::new(),
            bot_id: bot.id.clone(),
            catalog_visible: true,
        };
        spec.validate(state_dir, tls)?;
        Ok(spec)
    }

    pub(crate) fn from_metadata(
        metadata: &BTreeMap<String, Value>,
        bots: &crate::bots::BotStore,
        state_dir: &Path,
        tls: Option<&TlsConfig>,
    ) -> Result<Self> {
        Self::from_metadata_if_present(metadata, bots, state_dir, tls)?.ok_or_else(|| {
            Error::Config("chat checkpoint has no gateway runtime configuration".into())
        })
    }

    pub(crate) fn from_metadata_if_present(
        metadata: &BTreeMap<String, Value>,
        bots: &crate::bots::BotStore,
        state_dir: &Path,
        tls: Option<&TlsConfig>,
    ) -> Result<Option<Self>> {
        let Some(value) = metadata.get(CHAT_SPEC_METADATA_KEY) else {
            return Ok(None);
        };
        let stored = StoredChatSpec::deserialize(value)?;
        let bot = bots.bot(&stored.bot_id)?;
        let spec = Self {
            version: stored.version,
            workspace: stored.workspace,
            attached_folders: stored
                .attached_folders
                .into_iter()
                .filter(|folder| folder.is_dir())
                .collect(),
            bot_id: bot.id,
            catalog_visible: true,
        };
        spec.validate(state_dir, tls)?;
        Ok(Some(spec))
    }

    pub(crate) fn metadata(&self) -> Result<BTreeMap<String, Value>> {
        Ok(BTreeMap::from([(
            CHAT_SPEC_METADATA_KEY.into(),
            serde_json::to_value(self)?,
        )]))
    }

    pub(crate) fn persistent(bot: &crate::wire::BotRecord) -> Self {
        Self {
            version: CHAT_SPEC_VERSION,
            workspace: None,
            attached_folders: Vec::new(),
            bot_id: bot.id.clone(),
            catalog_visible: true,
        }
    }

    /// Private execution cwd, independent of the logical project shown to clients.
    pub(crate) fn execution_root(
        &self,
        state_dir: &Path,
        tls: Option<&TlsConfig>,
    ) -> Result<PathBuf> {
        if let Some(workspace) = &self.workspace {
            return Ok(workspace.clone());
        }
        let root = state_dir.with_extension("workspaces").join(&self.bot_id);
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        fs::set_permissions(&root, mobius::owner_only::dir())?;
        validate_chat_workspace(&root, state_dir, tls)
    }

    #[must_use]
    pub(crate) fn workspace_info(&self) -> Option<WorkspaceInfo> {
        self.workspace.as_ref().map(|path| WorkspaceInfo {
            id: workspace_id(path),
            path: path.clone(),
        })
    }

    pub(crate) fn with_attached_folder(
        &self,
        folder: &Path,
        state_dir: &Path,
        tls: Option<&TlsConfig>,
    ) -> Result<Option<Self>> {
        let workspace = self.workspace.as_ref().ok_or_else(|| {
            Error::Config(
                "Persistent Chat has no project; open a project chat to attach folders".into(),
            )
        })?;
        let folder = validate_chat_workspace(folder, state_dir, tls)?;
        if &folder == workspace || self.attached_folders.contains(&folder) {
            return Ok(None);
        }
        if self.attached_folders.len() == MAX_ATTACHED_FOLDERS {
            return Err(Error::Config(format!(
                "a chat cannot attach more than {MAX_ATTACHED_FOLDERS} folders"
            )));
        }
        let mut next = self.clone();
        next.attached_folders.push(folder);
        next.validate(state_dir, tls)?;
        Ok(Some(next))
    }

    fn validate(&self, state_dir: &Path, tls: Option<&TlsConfig>) -> Result<()> {
        if self.version != CHAT_SPEC_VERSION {
            return Err(Error::Config(format!(
                "unsupported chat configuration version {}",
                self.version
            )));
        }
        if self.bot_id.is_empty() {
            return Err(Error::Config("chat Bot ownership is invalid".into()));
        }
        if let Some(path) = &self.workspace {
            let workspace = validate_chat_workspace(path, state_dir, tls)?;
            if workspace != *path {
                return Err(Error::Config(
                    "chat workspace must use its canonical path".into(),
                ));
            }
        } else if !self.attached_folders.is_empty() {
            return Err(Error::Config(
                "project-free chats cannot attach folders".into(),
            ));
        }
        if self.attached_folders.len() > MAX_ATTACHED_FOLDERS {
            return Err(Error::Config(format!(
                "a chat cannot attach more than {MAX_ATTACHED_FOLDERS} folders"
            )));
        }
        let mut folders = self.workspace.iter().collect::<BTreeSet<_>>();
        for attached in &self.attached_folders {
            let workspace = validate_chat_workspace(attached, state_dir, tls)?;
            if workspace != *attached {
                return Err(Error::Config(
                    "attached folders must use canonical paths".into(),
                ));
            }
            if !folders.insert(attached) {
                return Err(Error::Config("chat folders must be unique".into()));
            }
        }
        Ok(())
    }
}

fn clear_missing_model_routes(
    composition: &mut AgentComposition,
    gateway: &GatewayConfig,
) -> Result<bool> {
    let mut removed = Vec::new();
    for (middleware, setting, route) in
        crate::middleware_manifest::configured_model_routes(&composition.middleware)
    {
        if !crate::provider_catalog::configured_route_exists(gateway, route)? {
            removed.push((middleware.to_owned(), setting.to_owned()));
        }
    }
    let changed = !removed.is_empty();
    for (middleware, setting) in removed {
        composition
            .middleware
            .set_setting(middleware, setting, None);
    }
    Ok(changed)
}

impl TlsConfig {
    fn validate(&self) -> Result<()> {
        for (name, path) in [
            ("TLS certificate", &self.certificate),
            ("TLS private key", &self.private_key),
        ] {
            if !path.is_absolute() || !path.is_file() {
                return Err(Error::Config(format!(
                    "{name} must be an existing absolute file"
                )));
            }
        }
        Ok(())
    }
}

impl CloudflareConfig {
    /// Validates and normalizes a stable public hostname.
    /// # Errors
    ///
    /// Returns an error if validation or an operation required by this function fails.
    pub fn named(hostname: &str) -> Result<Self> {
        let hostname = hostname.trim().to_ascii_lowercase();
        let config = Self::Named { hostname };
        config.validate()?;
        Ok(config)
    }

    /// Returns the stable endpoint when one exists before startup.
    #[must_use]
    pub fn endpoint(&self) -> Option<String> {
        self.hostname().map(|hostname| format!("wss://{hostname}"))
    }

    /// Returns the stable hostname when this is a named tunnel.
    #[must_use]
    pub fn hostname(&self) -> Option<&str> {
        match self {
            Self::Quick => None,
            Self::Named { hostname } => Some(hostname),
        }
    }

    /// Validates one tunnel-scoped connector token without retaining it.
    /// # Errors
    ///
    /// Returns an error if the supplied value is invalid.
    pub fn validate_token(token: &str) -> Result<()> {
        validate_cloudflare_token(token).map(|_| ())
    }

    fn validate(&self) -> Result<()> {
        if let Self::Named { hostname } = self
            && (hostname.len() > MAX_HOSTNAME_BYTES
                || !hostname.is_ascii()
                || hostname != &hostname.to_ascii_lowercase()
                || !hostname.contains('.')
                || !hostname.split('.').all(crate::hostnames::valid_label))
        {
            return Err(invalid_cloudflare_hostname());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
