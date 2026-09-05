//! Frontend-neutral contribution and presentation records.

use serde::Deserialize;
use serde::Serialize;

use super::EventMsg;
use super::ModelStepOutcome;
use super::Op;
use super::SessionFileReference;
use super::WebSearchAction;

/// A frontend command declared by a capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendCommand {
    pub name: String,
    pub arguments: String,
    pub description: String,
    /// Whether the frontend must wait for the current turn to finish before submitting this command.
    pub requires_idle: bool,
}

/// UI metadata exported by one capability.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendContribution {
    pub capability: String,
    /// Whether the composed runtime installs session-bound file attachment endpoints.
    pub accepts_file_attachments: bool,
    /// Optional capability-owned item count for generic summaries.
    pub count: Option<usize>,
    pub commands: Vec<FrontendCommand>,
    pub widgets: Vec<FrontendWidget>,
    pub references: Vec<FrontendReference>,
}

/// One middleware entry and its frontend-neutral configuration controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MiddlewareFeature {
    pub id: String,
    pub label: String,
    pub description: String,
    pub required: bool,
    pub settings: Vec<FrontendSetting>,
}

/// One schema-advertised setting rendered by a thin frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendSetting {
    pub id: String,
    pub label: String,
    pub description: String,
    /// Whether thin frontends should expose this setting beside the message composer.
    pub composer: bool,
    #[serde(flatten)]
    pub kind: FrontendSettingKind,
}

/// Generic control metadata for a schema-advertised setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrontendSettingKind {
    Integer {
        min: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
        step: i64,
    },
    Select {
        options: Vec<FrontendSettingOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unset_label: Option<String>,
    },
}

/// One exact value in a schema-advertised select control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendSettingOption {
    pub value: String,
    pub label: String,
    pub description: String,
    pub symbol: Option<FrontendSymbol>,
    pub tone: FrontendTone,
}

/// Scalar value accepted by the generic setting controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FrontendSettingValue {
    Integer(i64),
    String(String),
}

/// One chat reference supplied by a capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendReference {
    pub trigger: char,
    pub value: String,
    pub description: String,
}

/// One capability-rendered view mounted into a standard frontend slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendWidget {
    pub id: String,
    pub slot: FrontendSlot,
    pub text: String,
    pub tone: FrontendTone,
    pub symbol: Option<FrontendSymbol>,
    pub icon_only: bool,
    pub progress: Option<FrontendProgress>,
    pub content: Option<FrontendWidgetContent>,
    /// Optional operation invoked when a frontend activates this widget.
    pub action: Option<Op>,
}

/// Determinate progress rendered by a frontend widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendProgress {
    pub completed: usize,
    pub total: usize,
}

/// Capability-owned content shown when a frontend widget is opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendWidgetContent {
    Blocks {
        title: String,
        blocks: Vec<FrontendBlock>,
    },
    Picker {
        title: String,
        options: Vec<FrontendPickerOption>,
    },
    ActionList {
        title: String,
        items: Vec<FrontendActionListItem>,
        /// Actions on the whole list, such as adding an item.
        actions: Vec<FrontendAction>,
    },
}

/// Stable locations a thin frontend shell makes available to capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendSlot {
    Header,
    ComposerHeader,
    ComposerFooter,
    MessageActions,
    /// A transient capability-owned item after the live transcript.
    TranscriptTail,
    /// A capability destination mounted by the frontend shell.
    Navigation,
    /// A capability action mounted in the current chat's menu.
    ChatMenu,
}

/// Capability-rendered transcript content with frontend-neutral formatting and tone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendBlock {
    pub id: Option<String>,
    pub group: Option<String>,
    pub update: FrontendBlockUpdate,
    pub state: FrontendBlockState,
    pub role: FrontendBlockRole,
    /// Compact, standalone row label. Frontends must not derive this from `text`.
    pub title: String,
    /// Expandable body or artifact content.
    pub text: String,
    pub symbol: Option<FrontendSymbol>,
    /// Downloadable files owned by the session rendering this block.
    pub files: Vec<SessionFileReference>,
    pub format: FrontendBlockFormat,
    pub tone: FrontendTone,
}

/// A block together with its explicit semantic owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedBlock {
    pub capability: String,
    pub block: FrontendBlock,
}

/// How a block changes the matching capability-scoped ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendBlockUpdate {
    Replace,
    /// Append a block, separating nonempty text with a newline unless a boundary already has one.
    Append,
}

impl FrontendBlockUpdate {
    /// Applies a rendered text block without removing meaningful whitespace.
    pub fn apply(self, current: &mut String, text: &str) {
        if self == Self::Replace {
            current.clear();
        } else if !current.is_empty()
            && !text.is_empty()
            && !current.ends_with('\n')
            && !text.starts_with('\n')
        {
            current.push('\n');
        }
        current.push_str(text);
    }
}

/// Lifecycle state of one rendered transcript block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendBlockState {
    Pending,
    Complete,
}

/// Semantic category used for grouping, summaries, filtering, and icons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendBlockRole {
    Activity,
    Tool,
    WebSearch,
    Artifact,
    Approval,
    Notice,
}

/// Frontend-neutral structure carried by a transcript block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendBlockFormat {
    PlainText,
    UnifiedDiff,
}

/// One selectable action supplied by a capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendPickerOption {
    pub label: String,
    pub description: String,
    pub detail: String,
    pub symbol: Option<FrontendSymbol>,
    pub shows_detail: bool,
    pub op: Op,
}

/// One compact status row with optional trailing actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendActionListItem {
    pub id: String,
    pub text: String,
    pub state: FrontendListItemState,
    pub actions: Vec<FrontendAction>,
}

/// Semantic state for one compact list row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendListItemState {
    Plain,
    Pending,
    InProgress,
    Completed,
}

/// One labeled, icon-forward action attached to a list item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendAction {
    pub id: String,
    pub label: String,
    pub symbol: FrontendSymbol,
    pub tone: FrontendTone,
    pub op: Op,
    /// Optional capability-owned copy for editing input before submitting the action.
    pub editor: Option<FrontendEditor>,
}

/// Labels for a frontend-native single-text-input editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendEditor {
    pub title: String,
    pub label: String,
    pub description: String,
    pub submit_label: String,
}

/// One timestamped semantic event shown inside a capability preview.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrontendPreviewEvent {
    /// Canonical message identity retained from the recorded event.
    pub submission_id: Option<String>,
    pub recorded_at_ms: i64,
    pub event: EventMsg,
}

/// Generic capability UI updates understood by every frontend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frontend_type", rename_all = "snake_case")]
pub enum FrontendEvent {
    Render {
        capability: String,
        block: FrontendBlock,
    },
    Widget {
        capability: String,
        item: FrontendWidget,
    },
    RemoveWidget {
        capability: String,
        id: String,
    },
    Picker {
        title: String,
        options: Vec<FrontendPickerOption>,
    },
    Preview {
        id: String,
        title: String,
        subtitle: String,
        page_id: String,
        update: FrontendPreviewUpdate,
        events: Vec<FrontendPreviewEvent>,
        next: Option<Op>,
    },
}

/// How one preview page changes the matching frontend preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendPreviewUpdate {
    Replace,
    Prepend,
}

/// A presentation hint rather than a terminal-specific color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendTone {
    Neutral,
    Success,
    Warning,
    Error,
}

impl EventMsg {
    /// Renders framework-owned semantic events without frontend prose parsing.
    #[must_use]
    pub fn presentation(&self) -> Option<RenderedBlock> {
        let block = match self {
            Self::Error(error) => FrontendBlock {
                id: None,
                group: None,
                update: FrontendBlockUpdate::Replace,
                state: FrontendBlockState::Complete,
                role: FrontendBlockRole::Notice,
                title: "Error".into(),
                text: error.message.clone(),
                symbol: None,
                files: Vec::new(),
                format: FrontendBlockFormat::PlainText,
                tone: FrontendTone::Error,
            },
            Self::Warning(warning) => FrontendBlock {
                id: None,
                group: None,
                update: FrontendBlockUpdate::Replace,
                state: FrontendBlockState::Complete,
                role: FrontendBlockRole::Notice,
                title: "Warning".into(),
                text: warning.message.clone(),
                symbol: None,
                files: Vec::new(),
                format: FrontendBlockFormat::PlainText,
                tone: FrontendTone::Warning,
            },
            Self::TurnAborted(turn) => FrontendBlock {
                id: None,
                group: Some(turn.turn_id.clone()),
                update: FrontendBlockUpdate::Replace,
                state: FrontendBlockState::Complete,
                role: FrontendBlockRole::Notice,
                title: "Turn aborted".into(),
                text: turn.reason.clone(),
                symbol: None,
                files: Vec::new(),
                format: FrontendBlockFormat::PlainText,
                tone: FrontendTone::Warning,
            },
            Self::ModelStepCompleted(step) if step.outcome == ModelStepOutcome::Retrying => {
                FrontendBlock {
                    id: Some(format!("{}/retry", step.model_step_id)),
                    group: Some(step.turn_id.clone()),
                    update: FrontendBlockUpdate::Replace,
                    state: FrontendBlockState::Complete,
                    role: FrontendBlockRole::Notice,
                    title: "Reconnecting…".into(),
                    text: String::new(),
                    symbol: None,
                    files: Vec::new(),
                    format: FrontendBlockFormat::PlainText,
                    tone: FrontendTone::Warning,
                }
            }
            Self::WebSearchBegin(search) => FrontendBlock {
                id: Some(format!("{}/{}", search.model_step_id, search.call_id)),
                group: Some(search.turn_id.clone()),
                update: FrontendBlockUpdate::Replace,
                state: FrontendBlockState::Pending,
                role: FrontendBlockRole::WebSearch,
                title: "Searching the web".into(),
                text: String::new(),
                symbol: Some(FrontendSymbol::Search),
                files: Vec::new(),
                format: FrontendBlockFormat::PlainText,
                tone: FrontendTone::Neutral,
            },
            Self::WebSearchEnd(search) => {
                let (title, text, tone) = match &search.action {
                    WebSearchAction::Search { queries } => (
                        "Searched the web",
                        queries.join("\n"),
                        FrontendTone::Success,
                    ),
                    WebSearchAction::OpenPage { url } => (
                        "Opened a web page",
                        url.clone().unwrap_or_default(),
                        FrontendTone::Success,
                    ),
                    WebSearchAction::FindInPage { url, pattern } => {
                        let text = match (url, pattern) {
                            (Some(url), Some(pattern)) => format!("{pattern}\n{url}"),
                            (Some(url), None) => url.clone(),
                            (None, Some(pattern)) => pattern.clone(),
                            (None, None) => String::new(),
                        };
                        ("Searched a web page", text, FrontendTone::Success)
                    }
                    WebSearchAction::Interrupted => (
                        "Web search interrupted",
                        String::new(),
                        FrontendTone::Warning,
                    ),
                    WebSearchAction::Other => {
                        ("Web search complete", String::new(), FrontendTone::Success)
                    }
                };
                FrontendBlock {
                    id: Some(format!("{}/{}", search.model_step_id, search.call_id)),
                    group: Some(search.turn_id.clone()),
                    update: FrontendBlockUpdate::Replace,
                    state: FrontendBlockState::Complete,
                    role: FrontendBlockRole::WebSearch,
                    title: title.into(),
                    text,
                    symbol: Some(FrontendSymbol::Search),
                    files: Vec::new(),
                    format: FrontendBlockFormat::PlainText,
                    tone,
                }
            }
            Self::Frontend(FrontendEvent::Render { capability, block }) => {
                return Some(RenderedBlock {
                    capability: capability.clone(),
                    block: block.clone(),
                });
            }
            _ => return None,
        };
        Some(RenderedBlock {
            capability: match self {
                Self::WebSearchBegin(_) | Self::WebSearchEnd(_) => "web_search",
                _ => "agent",
            }
            .into(),
            block,
        })
    }
}

/// A presentation hint rather than a name from any one icon set, the same way
/// [`FrontendTone`] names a role instead of a color.
///
/// A gateway does not know whether the frontend draws SF Symbols, terminal glyphs, or
/// SVGs, so it names what a glyph stands for and each frontend supplies its own artwork.
/// [`Self::Custom`] carries anything outside this list so a plugin can still ship a glyph
/// this enum has never heard of. It is explicitly best-effort: a frontend that cannot
/// resolve the name falls back to a placeholder. Provider manifests use it for their own
/// brand tokens so adding a provider does not expand this semantic enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontendSymbol {
    Agent,
    Brain,
    Branch,
    Chat,
    Delete,
    Edit,
    Promote,
    Route,
    Search,
    Shield,
    ShieldAlert,
    ShieldCheck,
    ShieldOff,
    Sparkle,
    Storage,
    Task,
    Custom(String),
}

impl FrontendSymbol {
    /// The wire name. Also the stable token capabilities build action ids from.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Agent => "agent",
            Self::Brain => "brain",
            Self::Branch => "branch",
            Self::Chat => "chat",
            Self::Delete => "delete",
            Self::Edit => "edit",
            Self::Promote => "promote",
            Self::Route => "route",
            Self::Search => "search",
            Self::Shield => "shield",
            Self::ShieldAlert => "shield_alert",
            Self::ShieldCheck => "shield_check",
            Self::ShieldOff => "shield_off",
            Self::Sparkle => "sparkle",
            Self::Storage => "storage",
            Self::Task => "task",
            Self::Custom(name) => name,
        }
    }

    /// Unknown names become [`Self::Custom`] rather than an error: a frontend rendering a
    /// placeholder is a better outcome than a gateway refusing to decode a whole frame.
    pub(crate) fn from_wire(name: &str) -> Self {
        match name {
            "agent" => Self::Agent,
            "brain" => Self::Brain,
            "branch" => Self::Branch,
            "chat" => Self::Chat,
            "delete" => Self::Delete,
            "edit" => Self::Edit,
            "promote" => Self::Promote,
            "route" => Self::Route,
            "search" => Self::Search,
            "shield" => Self::Shield,
            "shield_alert" => Self::ShieldAlert,
            "shield_check" => Self::ShieldCheck,
            "shield_off" => Self::ShieldOff,
            "sparkle" => Self::Sparkle,
            "storage" => Self::Storage,
            "task" => Self::Task,
            other => Self::Custom(other.to_owned()),
        }
    }
}

impl std::fmt::Display for FrontendSymbol {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for FrontendSymbol {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FrontendSymbol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A known name round-trips out of `Custom` on the way back in, so the two spellings
        // of the same glyph cannot drift apart once a frame has crossed the wire.
        String::deserialize(deserializer).map(|name| Self::from_wire(&name))
    }
}
