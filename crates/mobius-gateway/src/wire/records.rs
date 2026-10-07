use std::borrow::Cow;
use std::sync::Arc;

use mobius::backend::model::provider::MediaModelPreset;

use super::*;

/// Optional gateway broadcasts a client may suppress on its connection.
/// Approvals, session events, errors, and request responses cannot be suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayNotification {
    /// Unsolicited conversation catalog and activity updates.
    Sessions,
    /// Unsolicited Bot catalog updates.
    Bots,
}

/// The computer view this connection can open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerView {
    /// No interactive computer view is available.
    Unavailable,
    /// The local app renders the gateway's assigned browser page.
    EmbeddedBrowser,
    /// The gateway serves its desktop through an authenticated stream.
    RemoteDesktop,
}

/// Gateway-wide frontend-safe state sent after authentication.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadyPayload {
    /// Release version of the connected gateway process.
    pub gateway_version: String,
    /// The machine name.
    pub machine_name: String,
    /// The interactive computer view available to this connection.
    pub computer_view: ComputerView,
    /// The bots.
    pub bots: Vec<BotRecord>,
    /// The sessions.
    pub sessions: Vec<SessionRecord>,
    /// The background approvals.
    pub background_approvals: Vec<BackgroundApproval>,
    /// The providers.
    pub providers: Vec<ProviderStatus>,
    /// The provider instances.
    pub provider_instances: Vec<ProviderInstance>,
    /// The bot defaults.
    pub bot_defaults: Option<VersionedAgentConfig>,
    /// The models.
    pub models: Vec<ModelChoice>,
    /// The model providers.
    pub model_providers: BTreeMap<String, String>,
    /// Selectable image model routes, keyed into `model_providers` like `models`.
    #[serde(default)]
    pub image_models: Vec<ModelChoice>,
    /// Selectable voice routes; each route's variant is its voice.
    #[serde(default)]
    pub voice_models: Vec<ModelChoice>,
    /// The middleware features.
    pub middleware_features: Vec<MiddlewareFeature>,
    /// The extensions.
    pub extensions: Vec<ExtensionRecord>,
    /// The contributions.
    pub contributions: Vec<FrontendContribution>,
    /// The max active sessions.
    pub max_active_sessions: usize,
    /// The session file limits.
    pub session_file_limits: SessionFileLimits,
    /// Content revision of each cacheable section.
    pub revisions: BTreeMap<ReadySection, String>,
    /// Sections sent empty because the client holds them at these revisions or skips them.
    pub omitted: BTreeSet<ReadySection>,
}

impl ReadyPayload {
    /// Exchanges one cacheable section with `other`.
    pub fn swap_section(&mut self, other: &mut Self, section: ReadySection) {
        use std::mem::swap;
        match section {
            ReadySection::Config => {
                swap(&mut self.providers, &mut other.providers);
                swap(&mut self.provider_instances, &mut other.provider_instances);
                swap(&mut self.bot_defaults, &mut other.bot_defaults);
                swap(&mut self.models, &mut other.models);
                swap(&mut self.model_providers, &mut other.model_providers);
                swap(&mut self.image_models, &mut other.image_models);
                swap(&mut self.voice_models, &mut other.voice_models);
                swap(
                    &mut self.middleware_features,
                    &mut other.middleware_features,
                );
                swap(&mut self.extensions, &mut other.extensions);
                swap(&mut self.contributions, &mut other.contributions);
            }
            ReadySection::Bots => swap(&mut self.bots, &mut other.bots),
            ReadySection::Sessions => swap(&mut self.sessions, &mut other.sessions),
        }
    }

    /// Moves the sections the gateway omitted back in from the catalog the client holds.
    pub fn restore_omitted(&mut self, held: &mut Self) {
        for section in std::mem::take(&mut self.omitted) {
            self.swap_section(held, section);
        }
    }

    /// Replaces this held catalog with a newer Ready, keeping the sections it omitted.
    pub fn update(&mut self, mut next: Self) {
        next.restore_omitted(self);
        *self = next;
    }

    /// Content revision of one section, comparable across connections to one gateway build.
    #[must_use]
    pub fn revision(&self, section: ReadySection) -> String {
        match section {
            ReadySection::Config => content_revision(&(
                &self.providers,
                &self.provider_instances,
                &self.bot_defaults,
                &self.models,
                &self.model_providers,
                &self.image_models,
                &self.voice_models,
                &self.middleware_features,
                &self.extensions,
                &self.contributions,
            )),
            ReadySection::Bots => content_revision(&self.bots),
            ReadySection::Sessions => content_revision(&self.sessions),
        }
    }

    /// The same gateway identity with no catalog content.
    pub(crate) fn blank(&self) -> Self {
        Self {
            gateway_version: String::new(),
            machine_name: String::new(),
            computer_view: self.computer_view,
            bots: Vec::new(),
            sessions: Vec::new(),
            background_approvals: Vec::new(),
            providers: Vec::new(),
            provider_instances: Vec::new(),
            bot_defaults: None,
            models: Vec::new(),
            model_providers: BTreeMap::new(),
            image_models: Vec::new(),
            voice_models: Vec::new(),
            middleware_features: Vec::new(),
            extensions: Vec::new(),
            contributions: Vec::new(),
            max_active_sessions: self.max_active_sessions,
            session_file_limits: self.session_file_limits,
            revisions: BTreeMap::new(),
            omitted: BTreeSet::new(),
        }
    }
}

/// A stable digest of one catalog value's wire form.
#[must_use]
pub fn content_revision(value: &impl Serialize) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    // Serializing into a digest cannot fail for catalog records.
    let _ = serde_json::to_writer(&mut hasher, value);
    hasher.finalize()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A cacheable part of the Ready catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadySection {
    /// Providers, models, Bot defaults, middleware, extensions and contributions.
    Config,
    /// The Bot catalog.
    Bots,
    /// The visible session catalog.
    Sessions,
}

/// Every section a Ready payload can omit.
pub const READY_SECTIONS: [ReadySection; 3] = [
    ReadySection::Config,
    ReadySection::Bots,
    ReadySection::Sessions,
];

/// What a client already holds of the Ready catalog, sent with `authenticate`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogHint {
    /// Sections the client holds, by the revision the gateway reported.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub known: BTreeMap<ReadySection, String>,
    /// Sections this connection never needs, such as on a file-transfer connection.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub skip: BTreeSet<ReadySection>,
}

impl CatalogHint {
    /// A hint that names no section.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.known.is_empty() && self.skip.is_empty()
    }
}

/// One position of the session catalog in a [`ServerMessage::SessionsChanged`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SessionSlot {
    /// A session unchanged since the catalog this connection last received, by ID.
    Unchanged(String),
    /// A new or changed session.
    Changed(Box<SessionRecord>),
}

/// Rebuilds the catalog `sessions` held from a [`ServerMessage::SessionsChanged`].
pub fn apply_session_changes(sessions: &mut Vec<SessionRecord>, changes: Vec<SessionSlot>) {
    let mut held: Vec<_> = std::mem::take(sessions).into_iter().map(Some).collect();
    *sessions = changes
        .into_iter()
        .filter_map(|slot| match slot {
            SessionSlot::Changed(session) => Some(*session),
            SessionSlot::Unchanged(id) => held
                .iter_mut()
                .find(|held| held.as_ref().is_some_and(|held| held.session_id == id))
                .and_then(Option::take),
        })
        .collect();
}

/// `+added −removed` lines of a Git diff, as Git's `--numstat` counts them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffTotals {
    /// Added lines.
    pub additions: u64,
    /// Removed lines.
    pub deletions: u64,
}

/// One hidden Bot conversation currently waiting for a human execution decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundApproval {
    /// The session identifier.
    pub session_id: String,
    /// The bot identifier.
    pub bot_id: String,
    /// The turn identifier.
    pub turn_id: String,
    /// The request identifier.
    pub request_id: String,
}

/// Frontend-safe state for one opened session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionReadyPayload {
    /// The active turn identifiers.
    pub active_turn_ids: Vec<String>,
    /// The pending approvals.
    pub pending_approvals: Vec<mobius::protocol::ExecApprovalRequestEvent>,
    /// The latest sequence.
    pub latest_sequence: u64,
    /// The next before sequence.
    pub next_before_sequence: Option<u64>,
    /// The workspace.
    pub workspace: Option<WorkspaceInfo>,
    /// The attached folders.
    pub attached_folders: Vec<PathBuf>,
    /// The git.
    pub git: Option<GitStatus>,
    /// The session.
    pub session: SessionConfiguredEvent,
    /// The contributions.
    pub contributions: Vec<FrontendContribution>,
    /// The widgets.
    pub widgets: Vec<SessionWidget>,
    /// The tool count.
    pub tool_count: usize,
    /// The compaction count.
    pub compaction_count: u64,
    /// The context limit tokens.
    pub context_limit_tokens: Option<i64>,
    /// The active message delivery.
    pub active_message_delivery: mobius::protocol::ActiveMessageDelivery,
    /// The run stats.
    pub run_stats: RunStats,
}

/// One currently mounted capability widget and its owning namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWidget {
    /// The capability.
    pub capability: String,
    /// The item.
    pub item: FrontendWidget,
}

/// One visible session with gateway-owned catalog presentation metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// The session identifier.
    pub session_id: String,
    /// The session context.
    pub session_context: mobius::protocol::SessionContext,
    /// The parent session identifier.
    pub parent_session_id: Option<String>,
    /// The parent sequence.
    pub parent_sequence: Option<u64>,
    /// The sequence.
    pub sequence: u64,
    /// The first user message.
    pub first_user_message: Option<String>,
    /// The execution stats.
    pub execution_stats: mobius::backend::checkpoint::ExecutionStats,
    /// The title.
    pub title: Option<String>,
    /// The pinned.
    pub pinned: bool,
    /// The activity.
    pub activity: SessionActivity,
    /// The created at.
    pub created_at: i64,
    /// The updated at.
    pub updated_at: i64,
}

/// Gateway-observed lifecycle state for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionActivity {
    /// Pending capability attention items, independent of turn activity.
    pub attention: u32,
    /// The state.
    pub state: SessionActivityState,
    /// The turn identifier.
    pub turn_id: Option<String>,
    /// The approval request identifier.
    pub approval_request_id: Option<String>,
    /// The started at.
    pub started_at: Option<i64>,
    /// The last outcome.
    pub last_outcome: Option<mobius::backend::checkpoint::ExecutionOutcome>,
    /// The message.
    pub message: Option<String>,
}

impl Default for SessionActivity {
    fn default() -> Self {
        Self {
            attention: 0,
            state: SessionActivityState::Idle,
            turn_id: None,
            approval_request_id: None,
            started_at: None,
            last_outcome: None,
            message: None,
        }
    }
}

/// Current work state advertised in the session catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionActivityState {
    /// Selects the idle case.
    Idle,
    /// Selects the running case.
    Running,
    /// Selects the awaiting approval case.
    AwaitingApproval,
}

/// Canonical workspace identity and path for one chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    /// The identifier.
    pub id: String,
    /// The path.
    pub path: PathBuf,
}

/// Local branch state for a Git-backed workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatus {
    /// The current branch.
    pub current_branch: String,
    /// The branches.
    pub branches: Vec<String>,
}

/// Public metadata for one SSH identity found on the gateway host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshIdentityRecord {
    /// The label.
    pub label: String,
    /// The algorithm.
    pub algorithm: String,
    /// The fingerprint.
    pub fingerprint: String,
}

/// One explicit Git patch selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffScope {
    /// Selects the staged case.
    Staged,
    /// Selects the unstaged case.
    Unstaged,
    /// Selects the committed case.
    Committed,
}

/// Which openable files to include in a workspace catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceFileScope {
    /// Selects the modified case.
    Modified,
    /// Selects the all case.
    All,
}

/// One regular file confined to the selected chat workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceFileRecord {
    /// The path.
    pub path: String,
    /// The size.
    pub size: u64,
}

/// One bounded folder listing from the gateway host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryListing {
    /// The path.
    pub path: PathBuf,
    /// The parent.
    pub parent: Option<PathBuf>,
    /// The entries.
    pub entries: Vec<DirectoryEntry>,
}

/// A selectable child folder on the gateway host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    /// The name.
    pub name: String,
    /// The path.
    pub path: PathBuf,
    /// The is directory.
    pub is_directory: bool,
}

/// A frontend-safe agent composition guarded by an optimistic revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionedAgentConfig {
    /// The revision.
    pub revision: u64,
    /// The config.
    pub config: AgentComposition,
}

/// Runtime settings an authenticated client may read and replace atomically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentComposition {
    /// The provider.
    pub provider: ProviderConfig,
    /// The realtime voice.
    pub realtime_voice: Option<String>,
    /// The middleware.
    pub middleware: MiddlewareConfig,
    /// The extensions.
    pub extensions: BTreeSet<String>,
    /// The system prompt.
    pub system_prompt: String,
    /// The max model steps.
    pub max_model_steps: u64,
}

/// Package format of one gateway-managed extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionKind {
    /// Selects the skill case.
    Skill,
    /// Selects the plugin case.
    Plugin,
}

/// One executable plugin hook shown before digest-bound trust is granted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionHookRecord {
    /// The event.
    pub event: String,
    /// The matcher.
    pub matcher: Option<String>,
    /// The command.
    pub command: String,
    /// The timeout seconds.
    pub timeout_seconds: u64,
}

/// Frontend-safe metadata for one installed extension snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionRecord {
    /// The identifier.
    pub id: String,
    /// The capability.
    pub capability: String,
    /// The kind.
    pub kind: ExtensionKind,
    /// The name.
    pub name: String,
    /// The description.
    pub description: String,
    /// The version.
    pub version: Option<String>,
    /// The source.
    pub source: String,
    /// The reference.
    pub reference: Option<String>,
    /// The subdirectory.
    pub subdirectory: Option<String>,
    /// The resolved revision.
    pub resolved_revision: String,
    /// The digest.
    pub digest: String,
    /// The skills.
    pub skills: Vec<String>,
    /// The hooks.
    pub hooks: Vec<ExtensionHookRecord>,
    /// The hooks trusted.
    pub hooks_trusted: bool,
}

/// Provider and model settings. Credentials are resolved only on the gateway host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Stable identity of one configured setup of `provider`. A gateway may hold
    /// several instances of the same provider with separate credentials.
    pub instance: String,
    /// The provider.
    pub provider: String,
    /// The model.
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The base URL.
    pub base_url: Option<String>,
    /// The endpoint auth.
    pub endpoint_auth: ProviderEndpointAuth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The reasoning effort.
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional native Responses processing tier.
    pub service_tier: Option<String>,
    /// The web search.
    pub web_search: HostedWebSearch,
}

/// Authentication applied when calling one configured provider endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderEndpointAuth {
    /// Selects the provider default case.
    ProviderDefault,
    /// Selects the credentialless case.
    Credentialless,
}

/// Credential availability exposed without returning credential material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderStatus {
    /// The provider.
    pub provider: String,
    /// The label.
    pub label: String,
    /// The symbol.
    pub symbol: FrontendSymbol,
    /// The description.
    pub description: String,
    /// The model identifiers configurable.
    pub model_ids_configurable: bool,
    /// The auth.
    pub auth: ProviderAuthKind,
    /// The default base URL.
    pub default_base_url: Option<String>,
    /// Always true: custom roots keep every native capability. Kept for older clients.
    pub native_custom_endpoints: bool,
    /// The default API key env.
    pub default_api_key_env: Option<String>,
    /// The models.
    pub models: Vec<ProviderModel>,
    /// Image models the provider advertises; empty when setups list their own.
    #[serde(default)]
    pub image_models: Cow<'static, [MediaModelPreset]>,
    /// Whether setups list their own image model identifiers.
    #[serde(default)]
    pub image_model_ids_configurable: bool,
    /// Realtime voice models the provider advertises, default first.
    #[serde(default)]
    pub voice_models: Cow<'static, [MediaModelPreset]>,
    /// The web search.
    pub web_search: Vec<FrontendSettingOption>,
    /// The tool discovery.
    pub tool_discovery: ToolDiscoveryMode,
    /// The custom endpoint tool discovery.
    pub custom_endpoint_tool_discovery: Option<ToolDiscoveryMode>,
    /// The realtime voices.
    pub realtime_voices: Vec<String>,
}

/// User-chosen accent for distinguishing provider instances in model selectors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTint {
    #[default]
    /// Selects the blue case.
    Blue,
    /// Selects the teal case.
    Teal,
    /// Selects the green case.
    Green,
    /// Selects the yellow case.
    Yellow,
    /// Selects the orange case.
    Orange,
    /// Selects the red case.
    Red,
    /// Selects the purple case.
    Purple,
    /// Selects the white case.
    White,
}

/// The silhouette of a Bot's face.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotShape {
    /// Selects the circle case.
    Circle,
    /// Selects the squircle case.
    Squircle,
    /// Selects the triangle case.
    Triangle,
    /// Selects the diamond case.
    Diamond,
    /// Selects the hexagon case.
    Hexagon,
    /// Selects the star case.
    Star,
    /// Selects the flower case.
    Flower,
}

impl BotShape {
    /// Every shape, in the order pickers show them.
    pub const ALL: [Self; 7] = [
        Self::Circle,
        Self::Squircle,
        Self::Triangle,
        Self::Diamond,
        Self::Hexagon,
        Self::Star,
        Self::Flower,
    ];
}

/// One durable setup of a provider. Several may share one `provider`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInstance {
    /// The label.
    pub label: String,
    /// The tint.
    pub tint: ProviderTint,
    /// The configured.
    pub configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The credential hint.
    pub credential_hint: Option<String>,
    /// The selection.
    pub selection: ProviderConfig,
    /// The model identifiers.
    pub model_ids: Vec<String>,
    /// The reasoning efforts.
    pub reasoning_efforts: Vec<String>,
    /// Image model identifiers listed by this setup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_model_ids: Vec<String>,
}

/// Frontend type attached to one authenticated connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    /// Selects the CLI case.
    Cli,
    /// Selects the macos case.
    Macos,
    /// Selects the ios case.
    Ios,
    /// Selects the ipados case.
    Ipados,
    /// Selects the gateway dashboard case.
    GatewayDashboard,
}

/// One paired client and its current connection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientStatus {
    /// The client identifier.
    pub client_id: String,
    /// The label.
    pub label: String,
    /// The kinds.
    pub kinds: Vec<ClientKind>,
    /// The connections.
    pub connections: usize,
}

impl ProviderStatus {
    #[must_use]
    /// Returns the configurable base URL.
    pub fn configurable_base_url(&self) -> bool {
        self.default_base_url.is_some()
    }

    #[must_use]
    /// Returns the default model.
    pub fn default_model(&self) -> Option<&ProviderModel> {
        self.models.first()
    }
}

/// One model advertised by a provider manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModel {
    /// The identifier.
    pub id: String,
    /// The label.
    pub label: String,
    /// The description.
    pub description: String,
    /// The context window.
    pub context_window: i64,
    /// The reasoning.
    pub reasoning: Vec<ReasoningChoice>,
    /// The default reasoning.
    pub default_reasoning: Option<String>,
    /// The tool discovery.
    pub tool_discovery: ToolDiscoveryMode,
}

/// One reasoning effort advertised for a provider model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningChoice {
    /// The identifier.
    pub id: String,
    /// The label.
    pub label: String,
    /// The description.
    pub description: String,
}

/// Frontend-safe provider authentication mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuthKind {
    /// Selects the API key case.
    ApiKey,
    /// Selects the device code case.
    DeviceCode,
}

/// Enabled optional middleware IDs and their schema-backed settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiddlewareConfig {
    pub(crate) enabled: BTreeSet<String>,
    /// The settings.
    pub settings: BTreeMap<String, BTreeMap<String, FrontendSettingValue>>,
}

impl MiddlewareConfig {
    /// Returns the selected policy that excludes an optional capability.
    #[must_use]
    pub fn disabled_by<'a>(
        &self,
        features: &'a [mobius::protocol::MiddlewareFeature],
        id: &str,
        selected_model: Option<&mobius::protocol::ModelChoice>,
    ) -> Option<&'a str> {
        if let Some(capability) = features
            .iter()
            .find(|feature| feature.id == id)
            .and_then(|feature| feature.required_model_capability)
            && selected_model.is_some_and(|model| !model.supports(capability))
        {
            return Some(match capability {
                mobius::protocol::ModelCapability::ImageGeneration => {
                    "a model without image generation"
                }
                mobius::protocol::ModelCapability::RealtimeVoice => {
                    "a model without realtime voice"
                }
            });
        }
        features
            .iter()
            .filter(|feature| feature.required || self.enabled(&feature.id))
            .find_map(|feature| {
                feature.settings.iter().find_map(|setting| {
                    let mobius::protocol::FrontendSettingKind::Select { options, .. } =
                        &setting.kind
                    else {
                        return None;
                    };
                    let Some(FrontendSettingValue::String(value)) =
                        self.setting(&feature.id, &setting.id)
                    else {
                        return None;
                    };
                    options
                        .iter()
                        .find(|option| {
                            option.value == *value
                                && option.disables.iter().any(|disabled| disabled == id)
                        })
                        .map(|option| option.label.as_str())
                })
            })
    }

    /// Applies exclusions advertised by the currently selected policies.
    pub fn reconcile(
        &mut self,
        features: &[mobius::protocol::MiddlewareFeature],
        selected_model: Option<&mobius::protocol::ModelChoice>,
    ) {
        let excluded = self
            .enabled
            .iter()
            .filter(|id| self.disabled_by(features, id, selected_model).is_some())
            // Evaluate all exclusions against the unchanged enabled set before removing any selected IDs.
            .cloned()
            .collect::<Vec<_>>();
        for id in excluded {
            self.enabled.remove(&id);
        }
    }

    /// Returns whether one advertised optional middleware is enabled.
    #[must_use]
    pub fn enabled(&self, id: &str) -> bool {
        self.enabled.contains(id)
    }

    /// Updates one advertised optional middleware before gateway validation.
    pub fn set_enabled(&mut self, id: impl Into<String>, enabled: bool) {
        let id = id.into();
        if enabled {
            self.enabled.insert(id);
        } else {
            self.enabled.remove(&id);
        }
    }

    /// Returns one advertised middleware setting.
    #[must_use]
    pub fn setting(&self, middleware: &str, setting: &str) -> Option<&FrontendSettingValue> {
        self.settings.get(middleware)?.get(setting)
    }

    /// Sets or clears one advertised middleware setting before gateway validation.
    pub fn set_setting(
        &mut self,
        middleware: impl Into<String>,
        setting: impl Into<String>,
        value: Option<FrontendSettingValue>,
    ) {
        let middleware = middleware.into();
        let setting = setting.into();
        if let Some(value) = value {
            self.settings
                .entry(middleware)
                .or_default()
                .insert(setting, value);
        } else if let Some(settings) = self.settings.get_mut(&middleware) {
            settings.remove(&setting);
            if settings.is_empty() {
                self.settings.remove(&middleware);
            }
        }
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &str> {
        self.enabled.iter().map(String::as_str)
    }
}

/// Capability-rendered preview whose inner events remain provider-neutral.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderedPreview {
    /// The semantic preview symbol.
    pub symbol: Option<FrontendSymbol>,
    /// Accumulated completed duration in milliseconds.
    pub duration_ms: Option<u64>,
    /// Start of the current activity, when still running.
    pub started_at_ms: Option<i64>,
    /// The identifier.
    pub id: String,
    /// The title.
    pub title: String,
    /// The subtitle.
    pub subtitle: String,
    /// The page identifier.
    pub page_id: String,
    /// The update.
    pub update: FrontendPreviewUpdate,
    /// The events.
    pub events: Vec<RenderedEvent>,
    /// The next.
    pub next: Option<Op>,
}

/// One preview event and its capability-rendered blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderedEvent {
    /// The submission identifier.
    pub submission_id: Option<Arc<str>>,
    /// The recorded at milliseconds.
    pub recorded_at_ms: i64,
    /// The event.
    pub event: EventMsg,
    /// The blocks.
    pub blocks: Vec<RenderedBlock>,
}

/// One timestamped semantic event and its deterministic presentation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedEvent {
    /// The sequence.
    pub sequence: u64,
    /// The recorded at milliseconds.
    pub recorded_at_ms: i64,
    /// The event.
    pub event: Event,
    /// The stream metrics.
    pub stream_metrics: Vec<StreamMetrics>,
    /// The blocks.
    pub blocks: Vec<RenderedBlock>,
    /// The preview.
    pub preview: Option<RenderedPreview>,
}

/// Gateway-owned profile and aggregate usage information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileSnapshot {
    /// The user name.
    pub user_name: Option<String>,
    /// The daily usage.
    pub daily_usage: Vec<DailyUsage>,
    /// The provider usage.
    pub provider_usage: Vec<ProviderUsage>,
    /// The run stats.
    pub run_stats: RunStats,
    /// The recent run groups.
    pub recent_run_groups: Vec<SessionRunGroup>,
}

/// One configured provider's remote subscription usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderUsage {
    /// The provider.
    pub provider: String,
    /// The limits.
    pub limits: Option<Vec<UsageLimit>>,
    /// The error.
    pub error: Option<String>,
}

/// Recent executions grouped under their nearest visible session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRunGroup {
    /// The session identifier.
    pub session_id: String,
    /// The title.
    pub title: String,
    /// The runs.
    pub runs: Vec<RunSummary>,
}

/// Completed execution totals plus the active run, when one exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStats {
    #[serde(flatten)]
    /// The completed.
    pub completed: mobius::backend::checkpoint::ExecutionStats,
    /// The active.
    pub active: Option<RunSummary>,
}

/// Frontend-safe summary of one completed or active user turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    /// The session identifier.
    pub session_id: String,
    /// The submission identifier.
    pub submission_id: String,
    /// The turn identifier.
    pub turn_id: String,
    /// The started at milliseconds.
    pub started_at_ms: i64,
    /// The finished at milliseconds.
    pub finished_at_ms: Option<i64>,
    /// The elapsed milliseconds.
    pub elapsed_ms: u64,
    /// The outcome.
    pub outcome: Option<mobius::backend::checkpoint::ExecutionOutcome>,
    /// The model calls.
    pub model_calls: u64,
    /// The tool calls.
    pub tool_calls: u64,
    /// The failed tool calls.
    pub failed_tool_calls: u64,
    /// The usage.
    pub usage: TokenUsage,
}

/// Usage accrued during one Unix day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyUsage {
    /// The unix day.
    pub unix_day: u64,
    /// The provider.
    pub provider: String,
    /// The usage.
    pub usage: TokenUsage,
}

/// One durable Bot profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotRecord {
    /// Canonical project-free conversation, opened through the ordinary session API.
    pub conversation_session_id: String,
    /// The identifier.
    pub id: String,
    /// The handle.
    pub handle: String,
    /// The name.
    pub name: String,
    /// The description.
    pub description: String,
    /// The tint.
    pub tint: ProviderTint,
    /// The face's silhouette.
    pub shape: BotShape,
    /// The config.
    pub config: VersionedAgentConfig,
    /// The accepts file attachments.
    pub accepts_file_attachments: bool,
    /// The routine interaction policy.
    pub routine_interaction_policy: RoutineInteractionPolicy,
}

/// Whether unattended routine work can stop for a human execution decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineInteractionPolicy {
    /// Selects the unattended case.
    Unattended,
    /// Selects the may pause for approval case.
    MayPauseForApproval,
}

/// One Bot-owned routine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routine {
    /// The identifier.
    pub id: String,
    /// The bot identifier.
    pub bot_id: String,
    /// The workspace.
    pub workspace: PathBuf,
    /// The instructions.
    pub instructions: String,
    /// User-authored event and timer bindings.
    pub bindings: Vec<RoutineBinding>,
    /// The enabled.
    pub enabled: bool,
    /// The finished.
    pub finished: bool,
    /// The next run at.
    pub next_run_at: Option<i64>,
}

/// A user-selected scheduling rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineSchedule {
    /// The kind.
    pub kind: RoutineScheduleKind,
    /// The at.
    pub at: Option<i64>,
    /// The every seconds.
    pub every_seconds: Option<u64>,
    /// The expression.
    pub expression: Option<String>,
    /// The time zone.
    pub time_zone: Option<String>,
}

/// The supported scheduling rule families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineScheduleKind {
    /// Selects the once case.
    Once,
    /// Selects the interval case.
    Interval,
    /// Selects the cron case.
    Cron,
}

/// A read-only page of a Bot routine transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutineRunPreview {
    /// The routine.
    pub routine: Routine,
    /// The run.
    pub run: RoutineRun,
    /// The records.
    pub records: Vec<RecordedEvent>,
    /// The next before sequence.
    pub next_before_sequence: Option<u64>,
}

/// One completed or active invocation of a Bot routine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineRun {
    /// The identifier.
    pub id: String,
    /// The routine identifier.
    pub routine_id: String,
    /// The bot identifier.
    pub bot_id: String,
    /// The started at.
    pub started_at: i64,
    /// The finished at.
    pub finished_at: Option<i64>,
    /// The status.
    pub status: RoutineRunStatus,
    /// The session identifier.
    pub session_id: Option<String>,
    /// The message.
    pub message: Option<String>,
}

/// Durable state of one routine invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineRunStatus {
    /// Selects the running case.
    Running,
    /// Selects the succeeded case.
    Succeeded,
    /// Selects the failed case.
    Failed,
    /// Selects the skipped case.
    Skipped,
    /// The user stopped the invocation.
    Cancelled,
}

/// Editable routine content and its user-authorized bindings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineDefinition {
    /// Explicit execution workspace.
    pub workspace: PathBuf,
    /// Saved task instructions.
    pub instructions: String,
    /// Event and timer triggers with exact actions.
    pub bindings: Vec<RoutineBinding>,
}

/// One user-authored trigger on its containing routine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookBinding<A> {
    /// Stable binding identity within the routine.
    pub id: String,
    /// Exact event or timer trigger.
    pub on: HookSelector,
    /// Action on this routine.
    pub action: A,
}

/// A trigger whose action addresses its containing routine.
pub type RoutineBinding = HookBinding<RoutineAction>;

/// Typed actions shared by reporting and control subscriptions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BotAction {
    /// Deliver the saved reporting mandate to the Bot conversation.
    Report {
        /// User-authored mandate.
        instruction: String,
    },
    /// Invoke the ordinary routine command handler.
    Routine {
        /// Addressed command.
        command: RoutineCommand,
    },
    /// Submit an ordinary message or interrupt to an owned session.
    Session {
        /// Owned destination.
        session_id: String,
        /// Existing core operation.
        op: Box<mobius::protocol::Op>,
    },
}

/// Finite commands shared by UI, tools, timers and event bindings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RoutineAction {
    /// Start one invocation.
    Start,
    /// Stop only the identified invocation.
    Stop {
        /// Invocation identity.
        run_id: String,
    },
    /// Disable future starts without stopping an active run.
    Pause,
    /// Enable future starts from the current time.
    Resume,
    /// Delete this routine through the ordinary cleanup path.
    Delete,
    /// Replace the editable definition.
    Update {
        /// Validated replacement.
        definition: RoutineDefinition,
    },
}

/// A command addressed to one routine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineCommand {
    /// Target routine identity.
    pub routine_id: String,
    /// Typed action.
    pub action: RoutineAction,
}

/// Gateway-established origin of a committed hook event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookSource {
    /// A Bot command.
    Bot {
        /// Owner identity.
        bot_id: String,
    },
    /// An owned routine.
    Routine {
        /// Routine identity.
        routine_id: String,
    },
    /// An owned session.
    Session {
        /// Session identity.
        session_id: String,
    },
    /// A timer on an owned routine.
    Schedule {
        /// Routine identity.
        routine_id: String,
        /// Binding identity.
        binding_id: String,
    },
    /// A gateway lifecycle operation.
    Gateway,
    /// An authenticated client.
    Client {
        /// Paired client identity.
        client_id: String,
    },
}

/// Exact trigger shared by routine bindings and reporting consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookSelector {
    /// The existing schedule model, owned by this binding.
    Schedule {
        /// Timer rule.
        schedule: RoutineSchedule,
        /// Optional cutoff.
        ends_at: Option<i64>,
    },
    /// An exact source and typed boundary, with applicable outcome filters.
    Event {
        /// Source identity.
        source: HookSource,
        /// Boundary.
        kind: HookKind,
        /// Optional terminal routine outcome.
        routine_outcome: Option<RoutineRunStatus>,
        /// Optional terminal session outcome.
        session_outcome: Option<mobius::backend::checkpoint::ExecutionOutcome>,
        /// Optional custom event name.
        custom_name: Option<String>,
    },
}

/// Finite lifecycle boundaries; source payloads cannot introduce commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// Routine registered.
    RoutineCreated,
    /// Routine definition replaced.
    RoutineUpdated,
    /// Routine disabled.
    RoutinePaused,
    /// Routine enabled.
    RoutineResumed,
    /// Routine removed.
    RoutineDeleted,
    /// Invocation reserved.
    RunStarted,
    /// Invocation completed, failed or was cancelled.
    RunFinished,
    /// Invocation skipped before starting.
    RunSkipped,
    /// A binding's timer became due.
    ScheduleDue,
    /// Session created.
    SessionCreated,
    /// A turn began.
    SessionTurnStarted,
    /// A turn reached a durable outcome.
    SessionTurnFinished,
    /// A turn awaits a human decision.
    SessionApproval,
    /// A capability requests a new user decision.
    SessionAttention,
    /// Session deleted.
    SessionDeleted,
    /// Session owner changed.
    SessionOwnerChanged,
    /// Client authenticated.
    ClientConnected,
    /// Client disconnected.
    ClientDisconnected,
    /// A Bot emitted a named event.
    CustomReceived,
}

/// One durable gateway event used by all hook consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookEvent {
    /// Stable event identity.
    pub id: String,
    /// Authenticated or internally established source.
    pub source: HookSource,
    /// Immediate causing event, if any.
    pub cause_id: Option<String>,
    /// Bounded prior event chain used to prevent feedback.
    pub ancestry: Vec<String>,
    /// Owning Bot.
    pub bot_id: String,
    /// Unix timestamp in seconds.
    pub occurred_at: i64,
    /// Small typed lifecycle fact.
    pub data: HookData,
}

/// Typed event facts, without executable source-provided instructions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookData {
    /// Routine registered.
    RoutineCreated {
        /// Routine identity.
        routine_id: String,
    },
    /// Routine definition changed.
    RoutineUpdated {
        /// Routine identity.
        routine_id: String,
    },
    /// Routine disabled.
    RoutinePaused {
        /// Routine identity.
        routine_id: String,
    },
    /// Routine enabled.
    RoutineResumed {
        /// Routine identity.
        routine_id: String,
    },
    /// Routine removed.
    RoutineDeleted {
        /// Routine identity.
        routine_id: String,
    },
    /// Invocation reserved.
    RunStarted {
        /// Routine identity.
        routine_id: String,
        /// Invocation identity.
        run_id: String,
        /// Execution transcript.
        session_id: Option<String>,
    },
    /// Invocation completed, failed or was cancelled.
    RunFinished {
        /// Routine identity.
        routine_id: String,
        /// Invocation identity.
        run_id: String,
        /// Terminal outcome.
        status: RoutineRunStatus,
        /// Execution transcript.
        session_id: Option<String>,
        /// Failure or cancellation reason.
        reason: Option<String>,
    },
    /// An overlapping or unavailable invocation was skipped.
    RunSkipped {
        /// Routine identity.
        routine_id: String,
        /// Skipped invocation identity.
        run_id: String,
        /// Saved reason.
        reason: String,
    },
    /// A binding's timer became due.
    ScheduleDue {
        /// Binding identity.
        binding_id: String,
    },
    /// A session was created.
    SessionCreated {
        /// Session identity.
        session_id: String,
    },
    /// A session turn began.
    SessionTurnStarted {
        /// Session identity.
        session_id: String,
        /// Turn identity.
        turn_id: String,
    },
    /// A session turn reached its committed outcome.
    SessionTurnFinished {
        /// Session identity.
        session_id: String,
        /// Turn identity.
        turn_id: String,
        /// Core execution outcome.
        outcome: mobius::backend::checkpoint::ExecutionOutcome,
    },
    /// A session awaits an execution decision.
    SessionApproval {
        /// Session identity.
        session_id: String,
        /// Turn identity.
        turn_id: String,
        /// Approval identity.
        request_id: String,
    },
    /// A capability requests a user decision.
    SessionAttention {
        /// Session identity.
        session_id: String,
        /// Owning capability.
        capability: String,
        /// Stable item identity.
        item_id: String,
        /// Bounded user-facing question or decision.
        text: String,
    },
    /// Session removed.
    SessionDeleted {
        /// Session identity.
        session_id: String,
    },
    /// Session transferred to another Bot.
    SessionOwnerChanged {
        /// Session identity.
        session_id: String,
        /// Previous Bot identity.
        previous_bot_id: String,
    },
    /// Paired client authenticated.
    ClientConnected {
        /// Paired client identity.
        client_id: String,
    },
    /// Paired client disconnected.
    ClientDisconnected {
        /// Paired client identity.
        client_id: String,
    },
    /// A Bot emitted a named JSON event.
    CustomReceived {
        /// User-selected event name.
        name: String,
        /// Bounded untrusted JSON evidence.
        data: serde_json::Value,
    },
}

impl HookData {
    /// The selector boundary for this fact.
    #[must_use]
    pub fn kind(&self) -> HookKind {
        match self {
            Self::RoutineCreated { .. } => HookKind::RoutineCreated,
            Self::RoutineUpdated { .. } => HookKind::RoutineUpdated,
            Self::RoutinePaused { .. } => HookKind::RoutinePaused,
            Self::RoutineResumed { .. } => HookKind::RoutineResumed,
            Self::RoutineDeleted { .. } => HookKind::RoutineDeleted,
            Self::RunStarted { .. } => HookKind::RunStarted,
            Self::RunFinished { .. } => HookKind::RunFinished,
            Self::RunSkipped { .. } => HookKind::RunSkipped,
            Self::ScheduleDue { .. } => HookKind::ScheduleDue,
            Self::SessionCreated { .. } => HookKind::SessionCreated,
            Self::SessionTurnStarted { .. } => HookKind::SessionTurnStarted,
            Self::SessionTurnFinished { .. } => HookKind::SessionTurnFinished,
            Self::SessionApproval { .. } => HookKind::SessionApproval,
            Self::SessionAttention { .. } => HookKind::SessionAttention,
            Self::SessionDeleted { .. } => HookKind::SessionDeleted,
            Self::SessionOwnerChanged { .. } => HookKind::SessionOwnerChanged,
            Self::ClientConnected { .. } => HookKind::ClientConnected,
            Self::ClientDisconnected { .. } => HookKind::ClientDisconnected,
            Self::CustomReceived { .. } => HookKind::CustomReceived,
        }
    }
}

/// User-authored reporting consumer of the same typed hook stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotSubscription {
    /// Owning Bot.
    pub bot_id: String,
    /// Exact trigger and its saved action.
    pub binding: HookBinding<BotAction>,
    /// Whether future matching facts should report.
    pub enabled: bool,
}
