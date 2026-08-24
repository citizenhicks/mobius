//! Validated gateway configuration and owner-only persistence.

mod store;
mod validation;
mod workspace;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use mobius::agent::DEFAULT_MAX_MODEL_STEPS;
use mobius::backend::model::provider::{
    ProviderAuth, ProviderDefinition, default_provider, provider,
};
use mobius::protocol::TokenUsage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest as _;

use crate::wire::{
    AgentComposition, DailyUsage, ProfileSnapshot, ProviderConfig, ProviderEndpointAuth,
    ProviderTint, VersionedAgentConfig, WorkspaceInfo,
};
use crate::{Error, Result};

use self::store::*;
pub use self::store::{ConfigStore, CredentialStore, load_cloudflare_token, state_dir};
pub use self::validation::validate_agent_composition;
use self::validation::*;
pub(crate) use self::validation::{effective_reasoning_effort, model_route_id};
use self::workspace::*;
pub(crate) use self::workspace::{create_workspace_directory, local_user_name};

const CONFIG_VERSION: u32 = 20;
const CHAT_SPEC_VERSION: u32 = 9;
pub(crate) const CHAT_SPEC_METADATA_KEY: &str = "mobius_gateway.chat";
const CONFIG_FILE: &str = "gateway.toml";
const CLOUDFLARE_TOKEN_FILE: &str = "cloudflare-token";
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_CREDENTIAL_STATE_BYTES: usize = 256 * 1024;
const MAX_SYSTEM_PROMPT_BYTES: usize = 64 * 1024;
const MAX_API_KEY_BYTES: usize = 16 * 1024;
const MAX_PROVIDER_CATALOG_ENTRIES: usize = 64;
const MAX_PROVIDER_CATALOG_ENTRY_BYTES: usize = 1024;
const MAX_PROVIDER_CATALOG_BYTES: usize = 16 * 1024;
const MAX_CUSTOM_MODEL_ROUTES: usize = 64;
const MAX_CLOUDFLARE_TOKEN_BYTES: usize = 16 * 1024;
const MAX_WORKSPACE_DIRECTORY_NAME_BYTES: usize = 255;
const SECONDS_PER_DAY: u64 = 86_400;
const USAGE_HISTORY_DAYS: u64 = 52 * 7;

mod defaults {
    include!(concat!(env!("OUT_DIR"), "/defaults.rs"));
}

/// Default loopback listener used by a local gateway.
pub const DEFAULT_LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 8741);

/// Default system prompt installed by `mobius-gateway init`.
pub const DEFAULT_SYSTEM_PROMPT: &str = defaults::DEFAULT_SYSTEM_PROMPT;

/// Context window used for custom models without an advertised preset.
pub const DEFAULT_CONTEXT_WINDOW: i64 = defaults::DEFAULT_CONTEXT_WINDOW;

/// Certificate paths required by a TLS listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
}

/// Cloudflare Tunnel exposure selected for this gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum CloudflareConfig {
    /// Account-free tunnel with an address assigned at process startup.
    Quick,
    /// User-owned tunnel with a stable published hostname.
    Named { hostname: String },
}

/// Durable machine-wide settings and defaults for one gateway process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    version: u32,
    pub listen: SocketAddr,
    pub tls: Option<TlsConfig>,
    pub cloudflare: Option<CloudflareConfig>,
    pub default_agent: Option<VersionedAgentConfig>,
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
    pub(crate) model_ids: Vec<String>,
    pub(crate) reasoning_efforts: Vec<String>,
}

/// Durable runtime recipe copied into one chat checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChatSpec {
    version: u32,
    pub(crate) workspace: PathBuf,
    pub(crate) agent: VersionedAgentConfig,
}

impl Default for AgentComposition {
    fn default() -> Self {
        let provider = default_provider();
        let model = provider
            .default_model()
            .and_then(|id| provider.model(id))
            .expect("default model manifest");
        Self {
            provider: ProviderConfig {
                instance: provider.id().into(),
                provider: provider.id().into(),
                model: model.id.into(),
                base_url: provider.default_base_url().map(str::to_string),
                endpoint_auth: ProviderEndpointAuth::ProviderDefault,
                reasoning_effort: model.default_reasoning.map(str::to_string),
                web_search: *provider
                    .web_search()
                    .first()
                    .expect("default provider web-search manifest"),
            },
            middleware: crate::middleware_manifest::default_config(),
            extensions: BTreeSet::new(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            max_model_steps: DEFAULT_MAX_MODEL_STEPS as u64,
        }
    }
}

impl GatewayConfig {
    /// Builds validated machine-wide settings and new-chat defaults.
    pub fn new(listen: SocketAddr, tls: Option<TlsConfig>) -> Result<Self> {
        let config = Self {
            version: CONFIG_VERSION,
            listen,
            tls,
            cloudflare: None,
            default_agent: None,
            configured_providers: BTreeMap::new(),
            installed_extensions: BTreeMap::new(),
            usage: UsageHistory::default(),
        };
        config.validate()?;
        Ok(config)
    }

    /// Builds a loopback gateway exposed through Cloudflare Tunnel.
    pub fn new_cloudflare(listen: SocketAddr, cloudflare: CloudflareConfig) -> Result<Self> {
        let mut config = Self::new(listen, None)?;
        config.cloudflare = Some(cloudflare);
        config.validate()?;
        Ok(config)
    }

    /// Registers one configured provider and establishes the first as the new-chat default.
    pub(crate) fn registering_provider(
        &self,
        selection: ProviderConfig,
        label: String,
        tint: ProviderTint,
        model_ids: Vec<String>,
        reasoning_efforts: Vec<String>,
    ) -> Result<Self> {
        if let Some(configured) = self.configured_providers.get(&selection.instance)
            && configured.selection.provider != selection.provider
        {
            return Err(Error::Config(format!(
                "provider instance `{}` already belongs to `{}`",
                selection.instance, configured.selection.provider
            )));
        }
        let configured = ConfiguredProvider {
            selection: selection.clone(),
            label,
            tint,
            model_ids,
            reasoning_efforts,
        };
        let mut next = self.clone();
        next.configured_providers
            .insert(selection.instance.clone(), configured);
        if self.default_agent.is_none() {
            let config = AgentComposition {
                provider: selection,
                ..AgentComposition::default()
            };
            next.default_agent = Some(VersionedAgentConfig {
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
            .default_agent
            .as_ref()
            .is_some_and(|default| default.config.provider.instance == instance)
        {
            return Err(Error::Config(
                "choose another gateway default before removing this provider".into(),
            ));
        }
        let mut next = self.clone();
        next.configured_providers.remove(instance);
        let mut default_config = next
            .default_agent
            .as_ref()
            .expect("a removable provider cannot be the only configured provider")
            .config
            .clone();
        if clear_missing_model_routes(&mut default_config, &next)? {
            let default = next
                .default_agent
                .as_mut()
                .expect("a removable provider cannot be the only configured provider");
            default.config = default_config;
            default.revision = default
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Config("configuration revision overflow".into()))?;
        }
        next.validate()?;
        Ok(next)
    }

    /// Replaces a same-provider new-chat default during an explicit fleet cutover.
    pub(crate) fn replacing_provider_default(&self, selection: &ProviderConfig) -> Result<Self> {
        let Some(current) = self.default_agent.as_ref() else {
            return Err(Error::Config(
                "register a provider before replacing its default".into(),
            ));
        };
        if current.config.provider.instance != selection.instance
            || current.config.provider == *selection
        {
            return Ok(self.clone());
        }
        let mut next = self.clone();
        let mut config = current.config.clone();
        config.provider = selection.clone();
        next.default_agent = Some(VersionedAgentConfig {
            revision: current
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Config("configuration revision overflow".into()))?,
            config,
        });
        next.validate()?;
        Ok(next)
    }

    /// Replaces only the defaults copied into future chats.
    pub(crate) fn replacing_default_agent(
        &self,
        expected_revision: u64,
        composition: AgentComposition,
    ) -> Result<Self> {
        let current = self
            .default_agent
            .as_ref()
            .ok_or_else(|| Error::Config("configure a provider before saving defaults".into()))?;
        if current.revision != expected_revision {
            return Err(Error::Config(format!(
                "configuration revision changed from {expected_revision} to {}",
                current.revision
            )));
        }
        let mut next = self.clone();
        next.default_agent = Some(VersionedAgentConfig {
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
            run_stats: crate::wire::RunStats::default(),
            recent_run_groups: Vec::new(),
        }
    }

    /// Validates every persisted trust-boundary field.
    pub fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION {
            return Err(Error::Config(format!(
                "unsupported gateway config version {}",
                self.version
            )));
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
        if self.configured_providers.is_empty() != self.default_agent.is_none() {
            return Err(Error::Config(
                "the gateway default must exist exactly when a provider is configured".into(),
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
        if let Some(default) = &self.default_agent {
            if default.revision == 0 {
                return Err(Error::Config(
                    "configuration revision must be positive".into(),
                ));
            }
            validate_agent_composition(&default.config)?;
            self.validate_provider_selection(&default.config.provider)?;
            for (middleware, setting, route) in
                crate::middleware_manifest::configured_model_routes(&default.config.middleware)
            {
                if !crate::provider_catalog::configured_route_exists(self, route)? {
                    return Err(Error::Config(format!(
                        "gateway default middleware setting `{middleware}.{setting}` is not a configured model route"
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
    pub(crate) fn new(
        workspace: &Path,
        agent: VersionedAgentConfig,
        state_dir: &Path,
        tls: Option<&TlsConfig>,
    ) -> Result<Self> {
        let spec = Self {
            version: CHAT_SPEC_VERSION,
            workspace: validate_chat_workspace(workspace, state_dir, tls)?,
            agent,
        };
        spec.validate(state_dir, tls)?;
        Ok(spec)
    }

    pub(crate) fn from_metadata(
        metadata: &BTreeMap<String, Value>,
        state_dir: &Path,
        tls: Option<&TlsConfig>,
    ) -> Result<Self> {
        Self::from_metadata_if_present(metadata, state_dir, tls)?.ok_or_else(|| {
            Error::Config("chat checkpoint has no gateway runtime configuration".into())
        })
    }

    pub(crate) fn from_metadata_if_present(
        metadata: &BTreeMap<String, Value>,
        state_dir: &Path,
        tls: Option<&TlsConfig>,
    ) -> Result<Option<Self>> {
        let Some(value) = metadata.get(CHAT_SPEC_METADATA_KEY) else {
            return Ok(None);
        };
        let spec: Self = serde_json::from_value(value.clone())?;
        spec.validate(state_dir, tls)?;
        Ok(Some(spec))
    }

    pub(crate) fn metadata(&self) -> Result<BTreeMap<String, Value>> {
        Ok(BTreeMap::from([(
            CHAT_SPEC_METADATA_KEY.into(),
            serde_json::to_value(self)?,
        )]))
    }

    #[must_use]
    pub(crate) fn workspace_info(&self) -> WorkspaceInfo {
        WorkspaceInfo {
            id: workspace_id(&self.workspace),
            path: self.workspace.clone(),
        }
    }

    pub(crate) fn replacing_agent(
        &self,
        expected_revision: u64,
        composition: AgentComposition,
        gateway: &GatewayConfig,
        state_dir: &Path,
        tls: Option<&TlsConfig>,
    ) -> Result<Self> {
        if expected_revision != self.agent.revision {
            return Err(Error::Config(format!(
                "configuration revision changed from {expected_revision} to {}",
                self.agent.revision
            )));
        }
        let mut next = self.clone();
        next.agent = VersionedAgentConfig {
            revision: self
                .agent
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Config("configuration revision overflow".into()))?,
            config: composition,
        };
        next.validate(state_dir, tls)?;
        gateway.validate_provider_selection(&next.agent.config.provider)?;
        Ok(next)
    }

    pub(crate) fn replacing_provider_selection(
        &self,
        selection: &ProviderConfig,
        gateway: &GatewayConfig,
        state_dir: &Path,
        tls: Option<&TlsConfig>,
    ) -> Result<Option<Self>> {
        if self.agent.config.provider.instance != selection.instance
            || self.agent.config.provider == *selection
        {
            return Ok(None);
        }
        let mut composition = self.agent.config.clone();
        composition.provider = selection.clone();
        self.replacing_agent(self.agent.revision, composition, gateway, state_dir, tls)
            .map(Some)
    }

    /// Falls back from provider entries removed since this chat last ran.
    pub(crate) fn normalizing_provider_catalog(
        &self,
        gateway: &GatewayConfig,
        state_dir: &Path,
        tls: Option<&TlsConfig>,
    ) -> Result<Self> {
        if gateway.configured_providers.is_empty() {
            return Ok(self.clone());
        }
        let mut composition = self.agent.config.clone();
        let mut changed = false;
        if !gateway
            .configured_providers
            .contains_key(&composition.provider.instance)
        {
            composition.provider = gateway
                .default_agent
                .as_ref()
                .ok_or_else(|| Error::Config("gateway has no default provider".into()))?
                .config
                .provider
                .clone();
            changed = true;
        }
        changed |= clear_missing_model_routes(&mut composition, gateway)?;
        if !changed {
            return Ok(self.clone());
        }
        self.replacing_agent(self.agent.revision, composition, gateway, state_dir, tls)
    }

    fn validate(&self, state_dir: &Path, tls: Option<&TlsConfig>) -> Result<()> {
        if self.version != CHAT_SPEC_VERSION {
            return Err(Error::Config(format!(
                "unsupported chat configuration version {}",
                self.version
            )));
        }
        if self.agent.revision == 0 {
            return Err(Error::Config(
                "chat configuration revision must be positive".into(),
            ));
        }
        let workspace = validate_chat_workspace(&self.workspace, state_dir, tls)?;
        if workspace != self.workspace {
            return Err(Error::Config(
                "chat workspace must use its canonical path".into(),
            ));
        }
        validate_agent_composition(&self.agent.config)
    }
}

fn clear_missing_model_routes(
    composition: &mut AgentComposition,
    gateway: &GatewayConfig,
) -> Result<bool> {
    let routes = crate::middleware_manifest::configured_model_routes(&composition.middleware)
        .into_iter()
        .map(|(middleware, setting, route)| {
            (middleware.to_owned(), setting.to_owned(), route.to_owned())
        })
        .collect::<Vec<_>>();
    let mut changed = false;
    for (middleware, setting, route) in routes {
        if !crate::provider_catalog::configured_route_exists(gateway, &route)? {
            composition
                .middleware
                .set_setting(middleware, setting, None);
            changed = true;
        }
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
    pub fn validate_token(token: &str) -> Result<()> {
        validate_cloudflare_token(token).map(|_| ())
    }

    fn validate(&self) -> Result<()> {
        if let Self::Named { hostname } = self
            && (hostname.len() > 253
                || !hostname.is_ascii()
                || hostname != &hostname.to_ascii_lowercase()
                || !hostname.contains('.')
                || !hostname.split('.').all(valid_hostname_label))
        {
            return Err(invalid_cloudflare_hostname());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
