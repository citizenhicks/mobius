use super::*;

/// Gateway-wide frontend-safe state sent after authentication.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadyPayload {
    /// Release version of the connected gateway process.
    pub gateway_version: String,
    /// The machine name.
    pub machine_name: String,
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
    pub workspace: WorkspaceInfo,
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
    /// The default API key env.
    pub default_api_key_env: Option<String>,
    /// The models.
    pub models: Vec<ProviderModel>,
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
    /// Returns the realtime voices.
    pub fn realtime_voices(&self, base_url: Option<&str>) -> &[String] {
        if mobius::backend::model::provider::uses_default_endpoint(
            self.default_base_url.as_deref(),
            base_url,
        ) {
            &self.realtime_voices
        } else {
            &[]
        }
    }

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
    pub submission_id: Option<String>,
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
    /// The schedule.
    pub schedule: RoutineSchedule,
    /// The ends at.
    pub ends_at: Option<i64>,
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
}
