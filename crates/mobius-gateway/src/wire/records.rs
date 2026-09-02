use super::*;

/// Gateway-wide frontend-safe state sent after authentication.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadyPayload {
    pub machine_name: String,
    pub bots: Vec<BotRecord>,
    pub sessions: Vec<SessionRecord>,
    pub swarms: Vec<SwarmRecord>,
    pub providers: Vec<ProviderStatus>,
    pub provider_instances: Vec<ProviderInstance>,
    pub bot_defaults: Option<VersionedAgentConfig>,
    pub models: Vec<ModelChoice>,
    pub model_providers: BTreeMap<String, String>,
    pub middleware_features: Vec<MiddlewareFeature>,
    pub extensions: Vec<ExtensionRecord>,
    pub contributions: Vec<FrontendContribution>,
    pub max_active_sessions: usize,
    pub session_file_limits: SessionFileLimits,
}

/// One gateway-managed group of durable Bots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmRecord {
    pub id: String,
    pub title: String,
    pub leader_bot_id: String,
    pub members: Vec<SwarmMemberRecord>,
    pub messages: Vec<SwarmMessageRecord>,
    pub updated_at_ms: i64,
}

/// Stable identity for one Bot in a Swarm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmMemberRecord {
    pub bot_id: String,
    pub handle: String,
}

/// One retained post on a swarm's shared message board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmMessageRecord {
    pub id: String,
    pub sequence: u64,
    pub author_bot_id: String,
    pub author_handle: String,
    pub source_session_id: String,
    pub text: String,
    pub created_at_ms: i64,
    pub in_reply_to_message_id: Option<String>,
    pub reply_depth: u8,
}

/// Gateway-managed scratchpad selected by a human-facing management request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScratchpadScope {
    Global,
    Swarm { id: String },
}

/// Frontend-safe state for one opened session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionReadyPayload {
    pub latest_sequence: u64,
    pub next_before_sequence: Option<u64>,
    pub workspace: WorkspaceInfo,
    pub git: Option<GitStatus>,
    pub session: SessionConfiguredEvent,
    pub contributions: Vec<FrontendContribution>,
    pub widgets: Vec<SessionWidget>,
    pub tool_count: usize,
    pub compaction_count: u64,
    pub context_limit_tokens: Option<i64>,
    pub run_stats: RunStats,
}

/// One currently mounted capability widget and its owning namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWidget {
    pub capability: String,
    pub item: FrontendWidget,
}

/// One visible session with gateway-owned catalog presentation metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub session_context: mobius::protocol::SessionContext,
    pub parent_session_id: Option<String>,
    pub parent_sequence: Option<u64>,
    pub sequence: u64,
    pub first_user_message: Option<String>,
    pub execution_stats: mobius::backend::checkpoint::ExecutionStats,
    pub title: Option<String>,
    pub pinned: bool,
    pub activity: SessionActivity,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Gateway-observed lifecycle state for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionActivity {
    pub state: SessionActivityState,
    pub turn_id: Option<String>,
    pub started_at: Option<i64>,
    pub last_outcome: Option<SessionOutcome>,
    pub message: Option<String>,
}

impl Default for SessionActivity {
    fn default() -> Self {
        Self {
            state: SessionActivityState::Idle,
            turn_id: None,
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
    Idle,
    Running,
    AwaitingApproval,
}

/// Most recent terminal outcome advertised in the session catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOutcome {
    Completed,
    Aborted,
    Failed,
}

/// Canonical workspace identity and path for one chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: String,
    pub path: PathBuf,
}

/// Local branch state for a Git-backed workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatus {
    pub current_branch: String,
    pub branches: Vec<String>,
}

/// Public metadata for one SSH identity found on the gateway host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshIdentityRecord {
    pub label: String,
    pub algorithm: String,
    pub fingerprint: String,
}

/// One explicit Git patch selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffScope {
    Staged,
    Unstaged,
    Committed,
}

/// Which openable files to include in a workspace catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceFileScope {
    Modified,
    All,
}

/// One regular file confined to the selected chat workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceFileRecord {
    pub path: String,
    pub size: u64,
}

/// One bounded folder listing from the gateway host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryListing {
    pub path: PathBuf,
    pub parent: Option<PathBuf>,
    pub entries: Vec<DirectoryEntry>,
}

/// A selectable child folder on the gateway host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_directory: bool,
}

/// A frontend-safe agent composition guarded by an optimistic revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionedAgentConfig {
    pub revision: u64,
    pub config: AgentComposition,
}

/// Runtime settings an authenticated client may read and replace atomically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentComposition {
    pub provider: ProviderConfig,
    pub middleware: MiddlewareConfig,
    pub extensions: BTreeSet<String>,
    pub system_prompt: String,
    pub max_model_steps: u64,
}

/// Package format of one gateway-managed extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionKind {
    Skill,
    Plugin,
}

/// One executable plugin hook shown before digest-bound trust is granted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionHookRecord {
    pub event: String,
    pub matcher: Option<String>,
    pub command: String,
    pub timeout_seconds: u64,
}

/// Frontend-safe metadata for one installed extension snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionRecord {
    pub id: String,
    pub capability: String,
    pub kind: ExtensionKind,
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub source: String,
    pub reference: Option<String>,
    pub subdirectory: Option<String>,
    pub resolved_revision: String,
    pub digest: String,
    pub skills: Vec<String>,
    pub hooks: Vec<ExtensionHookRecord>,
    pub hooks_trusted: bool,
}

/// Provider and model settings. Credentials are resolved only on the gateway host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Stable identity of one configured setup of `provider`. A gateway may hold
    /// several instances of the same provider with separate credentials.
    pub instance: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub endpoint_auth: ProviderEndpointAuth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub web_search: HostedWebSearch,
}

/// Authentication applied when calling one configured provider endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderEndpointAuth {
    ProviderDefault,
    Credentialless,
}

/// Credential availability exposed without returning credential material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub provider: String,
    pub label: String,
    pub symbol: FrontendSymbol,
    pub description: String,
    pub model_ids_configurable: bool,
    pub auth: ProviderAuthKind,
    pub default_base_url: Option<String>,
    pub default_api_key_env: Option<String>,
    pub models: Vec<ProviderModel>,
    pub web_search: Vec<FrontendSettingOption>,
    pub tool_discovery: ToolDiscoveryMode,
    pub custom_endpoint_tool_discovery: Option<ToolDiscoveryMode>,
}

/// User-chosen accent for distinguishing provider instances in model selectors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTint {
    #[default]
    Blue,
    Teal,
    Green,
    Yellow,
    Orange,
    Red,
    Purple,
}

/// One durable setup of a provider. Several may share one `provider`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInstance {
    pub label: String,
    pub tint: ProviderTint,
    pub configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_hint: Option<String>,
    pub selection: ProviderConfig,
    pub model_ids: Vec<String>,
    pub reasoning_efforts: Vec<String>,
}

/// Frontend type attached to one authenticated connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Cli,
    Macos,
    Ios,
    Ipados,
    GatewayDashboard,
}

/// One paired client and its current connection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientStatus {
    pub client_id: String,
    pub label: String,
    pub kinds: Vec<ClientKind>,
    pub connections: usize,
}

impl ProviderStatus {
    #[must_use]
    pub fn configurable_base_url(&self) -> bool {
        self.default_base_url.is_some()
    }

    #[must_use]
    pub fn default_model(&self) -> Option<&ProviderModel> {
        self.models.first()
    }
}

/// One model advertised by a provider manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModel {
    pub id: String,
    pub label: String,
    pub description: String,
    pub context_window: i64,
    pub reasoning: Vec<ReasoningChoice>,
    pub default_reasoning: Option<String>,
    pub tool_discovery: ToolDiscoveryMode,
}

/// One reasoning effort advertised for a provider model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningChoice {
    pub id: String,
    pub label: String,
    pub description: String,
}

/// Frontend-safe provider authentication mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuthKind {
    ApiKey,
    DeviceCode,
}

/// Enabled optional middleware IDs and their schema-backed settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiddlewareConfig {
    pub(crate) enabled: BTreeSet<String>,
    pub settings: BTreeMap<String, BTreeMap<String, FrontendSettingValue>>,
}

impl MiddlewareConfig {
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
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub page_id: String,
    pub update: FrontendPreviewUpdate,
    pub events: Vec<RenderedEvent>,
    pub next: Option<Op>,
}

/// One preview event and its capability-rendered blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderedEvent {
    pub recorded_at_ms: i64,
    pub event: EventMsg,
    pub blocks: Vec<RenderedBlock>,
}

/// One timestamped semantic event and its deterministic presentation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedEvent {
    pub sequence: u64,
    pub recorded_at_ms: i64,
    pub event: Event,
    pub stream_metrics: Vec<StreamMetrics>,
    pub blocks: Vec<RenderedBlock>,
    pub preview: Option<RenderedPreview>,
}

/// Gateway-owned profile and aggregate usage information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSnapshot {
    pub user_name: Option<String>,
    pub daily_usage: Vec<DailyUsage>,
    pub run_stats: RunStats,
    pub recent_run_groups: Vec<SessionRunGroup>,
}

/// Recent executions grouped under their nearest visible session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRunGroup {
    pub session_id: String,
    pub title: String,
    pub runs: Vec<RunSummary>,
}

/// Completed execution totals plus the active run, when one exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStats {
    pub run_count: u64,
    pub failed_run_count: u64,
    pub aborted_run_count: u64,
    pub model_calls: u64,
    pub tool_calls: u64,
    pub failed_tool_calls: u64,
    pub elapsed_ms: u64,
    pub usage: TokenUsage,
    pub active: Option<RunSummary>,
}

/// Frontend-safe summary of one completed or active user turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub session_id: String,
    pub submission_id: String,
    pub turn_id: String,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub elapsed_ms: u64,
    pub outcome: Option<SessionOutcome>,
    pub model_calls: u64,
    pub tool_calls: u64,
    pub failed_tool_calls: u64,
    pub usage: TokenUsage,
}

/// Usage accrued during one Unix day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyUsage {
    pub unix_day: u64,
    pub provider: String,
    pub usage: TokenUsage,
}

/// One durable Bot profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotRecord {
    pub id: String,
    pub handle: String,
    pub name: String,
    pub description: String,
    pub tint: ProviderTint,
    pub config: VersionedAgentConfig,
}

/// One Bot-owned routine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routine {
    pub id: String,
    pub bot_id: String,
    pub workspace: PathBuf,
    pub instructions: String,
    pub schedule: RoutineSchedule,
    pub ends_at: Option<i64>,
    pub enabled: bool,
    pub finished: bool,
    pub next_run_at: Option<i64>,
}

/// A user-selected scheduling rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineSchedule {
    pub kind: RoutineScheduleKind,
    pub at: Option<i64>,
    pub every_seconds: Option<u64>,
    pub expression: Option<String>,
    pub time_zone: Option<String>,
}

/// The supported scheduling rule families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineScheduleKind {
    Once,
    Interval,
    Cron,
}

/// A read-only page of a Bot routine transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutineRunPreview {
    pub routine: Routine,
    pub run: RoutineRun,
    pub records: Vec<RecordedEvent>,
    pub next_before_sequence: Option<u64>,
}

/// One completed or active invocation of a Bot routine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineRun {
    pub id: String,
    pub routine_id: String,
    pub bot_id: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub status: RoutineRunStatus,
    pub session_id: Option<String>,
    pub message: Option<String>,
}

/// Durable state of one routine invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineRunStatus {
    Running,
    Succeeded,
    Failed,
    Skipped,
}
