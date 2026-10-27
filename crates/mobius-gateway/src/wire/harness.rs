//! The agent runtimes ("harnesses") a Bot runs on, and what the gateway knows of each.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use mobius::protocol::{FrontendSetting, FrontendSymbol};
use serde::{Deserialize, Serialize};

use super::AgentComposition;

/// The runtime a Bot runs on, chosen once when the Bot is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessKind {
    /// The möbius framework, composed by this gateway.
    Mobius,
    /// The OpenAI Codex CLI installed on the gateway machine.
    Codex,
    /// The Claude CLI installed on the gateway machine.
    Claude,
}

/// A Bot's agent configuration, tagged by its harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "harness", rename_all = "snake_case")]
pub enum HarnessConfig {
    /// A möbius composition of provider, middleware and extensions.
    Mobius(Arc<AgentComposition>),
    /// Options for the Codex CLI.
    Codex(CodexConfig),
    /// Options for the Claude CLI.
    Claude(ClaudeConfig),
}

impl HarnessConfig {
    /// The harness this configuration runs on.
    #[must_use]
    pub const fn kind(&self) -> HarnessKind {
        match self {
            Self::Mobius(_) => HarnessKind::Mobius,
            Self::Codex(_) => HarnessKind::Codex,
            Self::Claude(_) => HarnessKind::Claude,
        }
    }

    /// The möbius composition, when this configuration runs on möbius.
    #[must_use]
    pub const fn mobius(&self) -> Option<&Arc<AgentComposition>> {
        match self {
            Self::Mobius(composition) => Some(composition),
            Self::Codex(_) | Self::Claude(_) => None,
        }
    }
}

/// Options for a harness driven through its installed CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliHarnessConfig {
    /// The model id; `None` uses the harness's own default model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Arc<str>>,
    /// The selected option value of each descriptor setting, by setting id; an absent setting
    /// uses the harness's own default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, Arc<str>>,
    /// What the harness may do without asking.
    pub access: BotAccess,
}

/// Options for the Codex CLI.
pub type CodexConfig = CliHarnessConfig;

/// Options for the Claude CLI.
pub type ClaudeConfig = CliHarnessConfig;

/// What a CLI harness may do without asking, mapped onto each harness's own permission modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotAccess {
    /// Every edit and command asks first.
    Supervised,
    /// File edits run; commands ask.
    AcceptEdits,
    /// The harness decides what needs asking.
    Auto,
    /// Nothing asks.
    FullAccess,
}

/// What the gateway knows of one harness, sent in `ReadyPayload.harnesses`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessDescriptor {
    /// The harness.
    pub kind: HarnessKind,
    /// The display name.
    pub label: String,
    /// The harness's icon.
    pub symbol: FrontendSymbol,
    /// Whether its CLI is installed on the gateway machine.
    pub install: HarnessInstall,
    /// Whether its CLI is signed in.
    pub auth: HarnessAuth,
    /// What chats on this harness can do; clients branch on these flags.
    pub capabilities: HarnessCapabilities,
    /// The selectable models; empty until the harness reports its catalog.
    pub models: Vec<HarnessModel>,
    /// The options a Bot sets in `CliHarnessConfig.settings`, such as reasoning effort.
    pub settings: Vec<FrontendSetting>,
    /// The `BotAccess` choices, for harnesses configured with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<FrontendSetting>,
    /// What new Bots on this harness start from, for CLI harnesses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<CliHarnessConfig>,
    /// The configured CLI path that overrides discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_path: Option<PathBuf>,
}

/// Whether a harness's CLI is installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HarnessInstall {
    /// The CLI was found and answered `--version`.
    Installed {
        /// The version the CLI reported.
        version: Arc<str>,
        /// The resolved executable.
        path: PathBuf,
    },
    /// No working CLI was found.
    Missing {
        /// How to install it, for the user to run; the gateway never runs it.
        hint: Arc<str>,
    },
    /// The harness is part of the gateway.
    NotApplicable,
}

/// Whether a harness's CLI is signed in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HarnessAuth {
    /// Signed in, with the account when the CLI names it.
    SignedIn {
        /// The account.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<Arc<str>>,
    },
    /// Not signed in.
    SignedOut,
    /// Not checked yet.
    Unknown,
    /// The harness has no sign-in of its own.
    NotApplicable,
}

/// What chats on one harness can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessCapabilities {
    /// Messages may carry file attachments.
    pub attachments: bool,
    /// Messages may steer a running turn.
    pub steering: bool,
    /// Tool calls may stop for an approval.
    pub approvals: bool,
    /// A chat may be resumed after its process restarted.
    pub resume: bool,
    /// Voice calls can start.
    pub voice: bool,
    /// The agent may control the gateway's computer.
    pub computer: bool,
}

/// One model a harness offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessModel {
    /// The id sent to the harness.
    pub id: Arc<str>,
    /// The display name.
    pub label: Arc<str>,
    /// The reasoning efforts the model accepts, as `effort` setting values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<Arc<str>>,
}
