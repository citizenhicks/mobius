//! The agent runtimes a Bot can run on, behind one session interface.
//!
//! The session actor (`host/session.rs`) drives a [`HarnessSession`] and never names a
//! harness; [`start`] picks the Bot's harness from the explicit registry in [`harness`].

pub(crate) mod memory;
pub(crate) mod mobius;

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, LazyLock};

use ::mobius::backend::checkpoint::{CheckpointStore, JournalEvent};
use ::mobius::backend::sandbox::ApprovalPolicy;
use ::mobius::backend::session_files::SessionFileStore;
use ::mobius::middleware::memory::Memory;
use ::mobius::middleware::sessions::LiveChats;
use ::mobius::protocol::{
    ActiveMessageDelivery, FrontendContribution, FrontendSymbol, SessionConfiguredEvent,
};
use ::mobius::{BoxFuture, SessionId};
use tokio::sync::{Mutex, mpsc};

pub(crate) use self::mobius::{MessageAcceptance, ValidatedSubmission};
use crate::bots::BotStore;
use crate::computer_runtime::{desktop::DesktopControl, remote_desktop::RemoteDesktop};
use crate::config::{ChatSpec, ConfigStore, CredentialStore, GatewayConfig};
use crate::host::{HostAccess, Rejection, RenderEvents, SessionRuntimeState};
use crate::sandbox::GatewaySandbox;
use crate::wire::{
    AgentComposition, BotAccess, BotCapabilities, BotMemory, BotRecord, HarnessAuth,
    HarnessCapabilities, HarnessConfig, HarnessDescriptor, HarnessInstall, HarnessKind,
    RoutineInteractionPolicy, VoiceCapability,
};
use crate::{Error, Result};

/// One harness: starts chats for the Bots that run on it.
pub(crate) trait Harness: Sync {
    /// Starts `session_id` for `spec`'s Bot.
    fn start<'a>(
        &'a self,
        context: &'a Arc<HarnessContext>,
        spec: &'a ChatSpec,
        session_id: &'a SessionId,
        origin_label: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn HarnessSession>>>;
}

/// One running chat, as the session actor drives it.
pub(crate) trait HarnessSession: RenderEvents + Send + Sync {
    fn session_id(&self) -> &SessionId;

    /// The configuration journaled when this run started.
    fn session(&self) -> &Arc<SessionConfiguredEvent>;

    /// The Bot this run was started for.
    fn bot(&self) -> &Arc<BotRecord>;

    /// Whether this run still matches `bot`'s saved configuration and the gateway's providers.
    fn is_current(&self, bot: &BotRecord) -> bool;

    /// The workspace and execution boundary the chat's files and Git requests use.
    fn sandbox(&self) -> &Arc<GatewaySandbox>;

    /// Queues one operation; `None` once [`HarnessSession::shutdown`] closed the input.
    fn submit(&self, submission: ValidatedSubmission) -> Option<::mobius::Result<()>>;

    /// Queues a source message whose future resolves once the harness accepted or refused it;
    /// `None` once the input is closed.
    fn admit(&self, submission: ValidatedSubmission) -> Option<::mobius::Result<Admission>>;

    /// The journaled events, in sequence; ends after [`HarnessSession::shutdown`].
    fn events(&mut self) -> &mut mpsc::Receiver<JournalEvent>;

    /// The turn, approval and queue state the durable checkpoint holds.
    fn runtime_state(&self) -> BoxFuture<'_, Result<Option<SessionRuntimeState>>>;

    /// No background command or capability work outlives the current turn.
    fn is_idle(&self) -> BoxFuture<'_, Result<bool>>;

    fn contributions(&self) -> Result<Vec<FrontendContribution>>;

    /// The usage-meter denominator for a model with `window` tokens of context.
    fn context_limit(&self, window: i64) -> i64;

    fn active_message_delivery(&self) -> ActiveMessageDelivery;

    /// The voice call this chat can start, or `None` when its harness has no voice.
    fn realtime(
        &self,
        _call: RealtimeCall,
    ) -> Option<std::result::Result<mobius::RealtimeModel, Rejection>> {
        None
    }

    /// Starts a fresh run of the same Bot preparation for `spec`, after this one drained.
    fn restart<'a>(
        &'a self,
        spec: &'a ChatSpec,
        origin_label: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn HarnessSession>>>;

    /// Stops accepting input; [`HarnessSession::events`] ends after the remaining events.
    fn shutdown(&mut self);
}

/// Resolves once a harness accepted or refused one source message.
pub(crate) type Admission = std::pin::Pin<
    Box<dyn std::future::Future<Output = ::mobius::Result<MessageAcceptance>> + Send + Sync>,
>;

/// What a voice call needs from the chat actor.
pub(crate) struct RealtimeCall {
    pub(crate) bot: BotRecord,
    pub(crate) active_turn_id: Option<Arc<str>>,
}

/// The gateway dependencies every harness starts chats with.
pub(crate) struct HarnessContext {
    pub(crate) store: ConfigStore,
    pub(crate) gateway: Arc<std::sync::Mutex<GatewayConfig>>,
    pub(crate) credentials: Arc<CredentialStore>,
    pub(crate) bots: Arc<BotStore>,
    pub(crate) checkpoints: Arc<dyn CheckpointStore>,
    pub(crate) memory: Arc<Memory>,
    pub(crate) session_files: Arc<SessionFileStore>,
    pub(crate) discovery_gate: Arc<Mutex<()>>,
    pub(crate) desktop: Arc<DesktopControl>,
    pub(crate) remote_desktop: Arc<RemoteDesktop>,
    pub(crate) provider_epoch: Arc<AtomicU64>,
    pub(crate) live_chats: Arc<dyn LiveChats>,
    pub(crate) host_access: HostAccess,
}

/// The registry: each kind's harness, or `None` when this gateway has no adapter for it.
fn harness(kind: HarnessKind) -> Option<&'static dyn Harness> {
    match kind {
        HarnessKind::Mobius => Some(&mobius::Mobius),
        HarnessKind::Codex | HarnessKind::Claude => None,
    }
}

/// The error for work that needs `kind`'s missing adapter.
pub(crate) fn unavailable(kind: HarnessKind) -> Error {
    let label = match kind {
        HarnessKind::Mobius => "möbius",
        HarnessKind::Codex => "Codex",
        HarnessKind::Claude => "Claude",
    };
    Error::HarnessUnavailable(format!(
        "the {label} harness is not available on this gateway"
    ))
}

/// Starts `session_id` on the harness of `spec`'s Bot.
pub(crate) async fn start(
    context: &Arc<HarnessContext>,
    spec: &ChatSpec,
    session_id: &SessionId,
    origin_label: &str,
) -> Result<Box<dyn HarnessSession>> {
    let kind = context.bots.bot(&spec.bot_id)?.harness;
    harness(kind)
        .ok_or_else(|| unavailable(kind))?
        .start(context, spec, session_id, origin_label)
        .await
}

/// How a Bot's tool calls are approved, parsed once from its configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalMode {
    /// The möbius sandbox's approval policy.
    Mobius(ApprovalPolicy),
    /// A CLI harness's access level.
    Cli(crate::wire::BotAccess),
}

impl ApprovalMode {
    /// Whether unattended routine work can stop for a human decision.
    pub(crate) const fn routine_interaction_policy(self) -> RoutineInteractionPolicy {
        match self {
            Self::Mobius(policy) => mobius::assembly::routine_interaction_policy(policy),
            Self::Cli(access) => cli_routine_interaction_policy(access),
        }
    }
}

/// A Bot configuration checked at the gateway boundary, with what the check parsed.
#[derive(Debug)]
pub(crate) struct ValidatedConfig {
    config: HarnessConfig,
    approval: ApprovalMode,
}

impl ValidatedConfig {
    pub(crate) const fn mobius(composition: Arc<AgentComposition>, policy: ApprovalPolicy) -> Self {
        Self {
            config: HarnessConfig::Mobius(composition),
            approval: ApprovalMode::Mobius(policy),
        }
    }

    pub(crate) const fn config(&self) -> &HarnessConfig {
        &self.config
    }

    pub(crate) fn into_parts(self) -> (HarnessConfig, ApprovalMode) {
        (self.config, self.approval)
    }
}

#[cfg(test)]
impl From<AgentComposition> for ValidatedConfig {
    fn from(composition: AgentComposition) -> Self {
        let policy = crate::config::validate_agent_composition(&composition)
            .expect("valid test composition");
        Self::mobius(Arc::new(composition), policy)
    }
}

#[cfg(test)]
impl From<HarnessConfig> for ValidatedConfig {
    fn from(config: HarnessConfig) -> Self {
        let approval = validate_stored(&config).expect("valid test configuration");
        Self { config, approval }
    }
}

/// Checks a configuration of a harness other than möbius.
/// # Errors
///
/// Returns an error for a möbius composition, which the host checks against the gateway, or
/// for a harness this gateway has no adapter for.
pub(crate) fn validate_cli(config: HarnessConfig) -> Result<ValidatedConfig> {
    match config {
        HarnessConfig::Mobius(_) => Err(Error::Config(
            "möbius compositions are checked against the gateway".into(),
        )),
        config => Err(unavailable(config.kind())),
    }
}

/// Checks a configuration read back from disk. A Bot whose harness has no adapter here keeps its
/// configuration and stays unavailable.
/// # Errors
///
/// Returns an error if a möbius composition is invalid.
pub(crate) fn validate_stored(config: &HarnessConfig) -> Result<ApprovalMode> {
    match config {
        HarnessConfig::Mobius(composition) => {
            crate::config::validate_agent_composition(composition).map(ApprovalMode::Mobius)
        }
        HarnessConfig::Codex(options) | HarnessConfig::Claude(options) => {
            Ok(ApprovalMode::Cli(options.access))
        }
    }
}

/// Whether unattended routine work with `access` can stop for a human decision.
const fn cli_routine_interaction_policy(access: BotAccess) -> RoutineInteractionPolicy {
    match access {
        BotAccess::FullAccess => RoutineInteractionPolicy::Unattended,
        BotAccess::Supervised | BotAccess::AcceptEdits | BotAccess::Auto => {
            RoutineInteractionPolicy::MayPauseForApproval
        }
    }
}

/// What chats composed from `config` can do.
/// # Errors
///
/// Returns an error if a möbius composition's voice setting cannot be read.
pub(crate) fn bot_capabilities(
    config: &HarnessConfig,
    voices: &mobius::provider_catalog::VoiceRoutes,
) -> Result<BotCapabilities> {
    match config {
        HarnessConfig::Mobius(composition) => {
            mobius::assembly::bot_capabilities(composition, voices)
        }
        HarnessConfig::Codex(_) | HarnessConfig::Claude(_) => Ok(BotCapabilities {
            attachments: false,
            computer: false,
            voice: VoiceCapability::Off,
            memory: BotMemory {
                enabled: false,
                global: false,
            },
        }),
    }
}

/// Whether `bot` starts the same agent as one with `description` and `config`.
pub(crate) fn same_runtime(bot: &BotRecord, description: &str, config: &HarnessConfig) -> bool {
    *bot.description == *description && bot.config.config == *config
}

/// A system prompt with the Bot's description as its preamble.
pub(crate) fn compose_instructions(description: &str, system_prompt: &str) -> String {
    match description.trim() {
        "" => system_prompt.to_owned(),
        preamble => format!("{preamble}\n\n{system_prompt}"),
    }
}

/// The möbius harness's descriptor text.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MobiusManifest {
    label: String,
    symbol: FrontendSymbol,
    capabilities: HarnessCapabilities,
}

static MOBIUS: LazyLock<MobiusManifest> =
    LazyLock::new(|| ::mobius::config::embedded(include_str!("mobius/mobius.toml")));

/// Every harness's descriptor: only harnesses this gateway has an adapter for.
pub(crate) fn descriptors() -> Vec<HarnessDescriptor> {
    vec![HarnessDescriptor {
        kind: HarnessKind::Mobius,
        label: MOBIUS.label.clone(), // rules-allow: each Ready payload owns its descriptor text
        symbol: MOBIUS.symbol.clone(), // rules-allow: each Ready payload owns its descriptor text
        install: HarnessInstall::NotApplicable,
        auth: HarnessAuth::NotApplicable,
        capabilities: MOBIUS.capabilities,
        models: Vec::new(),
        settings: Vec::new(),
        access: None,
        defaults: None,
        cli_path: None,
    }]
}

/// Moves a reassigned chat's durable checkpoint to the Bot that now starts it.
pub(crate) async fn adopt_checkpoint_owner(
    checkpoints: &dyn CheckpointStore,
    session_id: &SessionId,
    spec: &ChatSpec,
) -> Result<()> {
    let Some(mut checkpoint) = checkpoints.load(session_id).await? else {
        return Ok(());
    };
    if checkpoint.session_context.owner_id == spec.bot_id {
        return Ok(());
    }
    checkpoint.sequence = checkpoint
        .sequence
        .checked_add(1)
        .ok_or_else(|| Error::Config("checkpoint sequence overflow".into()))?;
    Arc::make_mut(&mut checkpoint.session_context).owner_id = Arc::clone(&spec.bot_id);
    checkpoint.metadata.extend(spec.metadata()?);
    checkpoints.save(checkpoint, Vec::new(), None).await?;
    Ok(())
}
