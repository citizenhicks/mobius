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
    /// The name.
    pub name: String,
    /// The arguments.
    pub arguments: String,
    /// The description.
    pub description: String,
    /// Whether the frontend must wait for the current turn to finish before submitting this command.
    pub requires_idle: bool,
}

/// UI metadata exported by one capability.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendContribution {
    /// The capability.
    pub capability: String,
    /// Whether the composed runtime installs session-bound file attachment endpoints.
    pub accepts_file_attachments: bool,
    /// Optional capability-owned item count for generic summaries.
    pub count: Option<usize>,
    /// The commands.
    pub commands: Vec<FrontendCommand>,
    /// The widgets.
    pub widgets: Vec<FrontendWidget>,
    /// The references.
    pub references: Vec<FrontendReference>,
}

/// One middleware entry and its frontend-neutral configuration controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MiddlewareFeature {
    /// The identifier.
    pub id: String,
    /// The label.
    pub label: String,
    /// The description.
    pub description: String,
    /// The required.
    pub required: bool,
    /// The settings.
    pub settings: Vec<FrontendSetting>,
}

/// One schema-advertised setting rendered by a thin frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendSetting {
    /// The identifier.
    pub id: String,
    /// The label.
    pub label: String,
    /// The description.
    pub description: String,
    /// Whether thin frontends should expose this setting beside the message composer.
    pub composer: bool,
    #[serde(flatten)]
    /// The kind.
    pub kind: FrontendSettingKind,
}

/// Generic control metadata for a schema-advertised setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrontendSettingKind {
    /// Selects the integer case.
    Integer {
        /// The min.
        min: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// The max.
        max: Option<i64>,
        /// The step.
        step: i64,
    },
    /// Selects the select case.
    Select {
        /// The options.
        options: Vec<FrontendSettingOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// The unset label.
        unset_label: Option<String>,
    },
}

/// One exact value in a schema-advertised select control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendSettingOption {
    /// The value.
    pub value: String,
    /// The label.
    pub label: String,
    /// The description.
    pub description: String,
    /// The symbol.
    pub symbol: Option<FrontendSymbol>,
    /// The tone.
    pub tone: FrontendTone,
    /// Optional capabilities excluded while this choice is active.
    pub disables: Vec<String>,
}

/// Scalar value accepted by the generic setting controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FrontendSettingValue {
    /// Selects the integer case.
    Integer(i64),
    /// Selects the string case.
    String(String),
}

/// One chat reference supplied by a capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendReference {
    /// The trigger.
    pub trigger: char,
    /// The value.
    pub value: String,
    /// The description.
    pub description: String,
}

/// One capability-rendered view mounted into a standard frontend slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendWidget {
    /// The identifier.
    pub id: String,
    /// The slot.
    pub slot: FrontendSlot,
    /// The text.
    pub text: String,
    /// The tone.
    pub tone: FrontendTone,
    /// The symbol.
    pub symbol: Option<FrontendSymbol>,
    /// The icon only.
    pub icon_only: bool,
    /// The progress.
    pub progress: Option<FrontendProgress>,
    /// The content.
    pub content: Option<FrontendWidgetContent>,
    /// Optional operation invoked when a frontend activates this widget.
    pub action: Option<Op>,
}

/// Determinate progress rendered by a frontend widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendProgress {
    /// The completed.
    pub completed: usize,
    /// The total.
    pub total: usize,
}

/// Capability-owned content shown when a frontend widget is opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendWidgetContent {
    /// Selects the blocks case.
    Blocks {
        /// The title.
        title: String,
        /// The blocks.
        blocks: Vec<FrontendBlock>,
    },
    /// Selects the picker case.
    Picker {
        /// The title.
        title: String,
        /// The options.
        options: Vec<FrontendPickerOption>,
    },
    /// Selects the action list case.
    ActionList {
        /// The title.
        title: String,
        /// The items.
        items: Vec<FrontendActionListItem>,
        /// Actions on the whole list, such as adding an item.
        actions: Vec<FrontendAction>,
    },
}

/// Stable locations a thin frontend shell makes available to capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendSlot {
    /// Selects the header case.
    Header,
    /// Selects the composer header case.
    ComposerHeader,
    /// Selects the composer footer case.
    ComposerFooter,
    /// Selects the message actions case.
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
    /// The identifier.
    pub id: Option<String>,
    /// The group.
    pub group: Option<String>,
    /// The update.
    pub update: FrontendBlockUpdate,
    /// The state.
    pub state: FrontendBlockState,
    /// The role.
    pub role: FrontendBlockRole,
    /// Compact, standalone row label. Frontends must not derive this from `text`.
    pub title: String,
    /// Expandable body or artifact content.
    pub text: String,
    /// The symbol.
    pub symbol: Option<FrontendSymbol>,
    /// Downloadable files owned by the session rendering this block.
    pub files: Vec<SessionFileReference>,
    /// Ordered observations rendered after the block summary.
    pub content: super::ToolContent,
    /// The format.
    pub format: FrontendBlockFormat,
    /// The tone.
    pub tone: FrontendTone,
}

/// A block together with its explicit semantic owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedBlock {
    /// The capability.
    pub capability: String,
    /// The block.
    pub block: FrontendBlock,
}

/// How a block changes the matching capability-scoped ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendBlockUpdate {
    /// Selects the replace case.
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
    /// Selects the pending case.
    Pending,
    /// Selects the complete case.
    Complete,
}

/// Semantic category used for grouping, summaries, filtering, and icons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendBlockRole {
    /// Selects the activity case.
    Activity,
    /// Selects the tool case.
    Tool,
    /// Selects the web search case.
    WebSearch,
    /// Selects the artifact case.
    Artifact,
    /// Selects the approval case.
    Approval,
    /// Selects the notice case.
    Notice,
}

/// Frontend-neutral structure carried by a transcript block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendBlockFormat {
    /// Selects the plain text case.
    PlainText,
    /// Selects the unified diff case.
    UnifiedDiff,
}

/// One selectable action supplied by a capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendPickerOption {
    /// The label.
    pub label: String,
    /// The description.
    pub description: String,
    /// The detail.
    pub detail: String,
    /// The symbol.
    pub symbol: Option<FrontendSymbol>,
    /// The shows detail.
    pub shows_detail: bool,
    /// The op.
    pub op: Op,
}

/// One compact status row with optional trailing actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendActionListItem {
    /// The identifier.
    pub id: String,
    /// The text.
    pub text: String,
    /// The state.
    pub state: FrontendListItemState,
    /// The actions.
    pub actions: Vec<FrontendAction>,
}

/// Semantic state for one compact list row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendListItemState {
    /// Selects the plain case.
    Plain,
    /// Selects the pending case.
    Pending,
    /// Selects the in progress case.
    InProgress,
    /// Selects the completed case.
    Completed,
}

/// One labeled, icon-forward action attached to a list item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendAction {
    /// The identifier.
    pub id: String,
    /// The label.
    pub label: String,
    /// The symbol.
    pub symbol: FrontendSymbol,
    /// The tone.
    pub tone: FrontendTone,
    /// The op.
    pub op: Op,
    /// Optional capability-owned copy for editing input before submitting the action.
    pub editor: Option<FrontendEditor>,
}

/// Labels for a frontend-native single-text-input editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendEditor {
    /// The title.
    pub title: String,
    /// The label.
    pub label: String,
    /// The description.
    pub description: String,
    /// The submit label.
    pub submit_label: String,
}

/// One timestamped semantic event shown inside a capability preview.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrontendPreviewEvent {
    /// Canonical message identity retained from the recorded event.
    pub submission_id: Option<String>,
    /// The recorded at milliseconds.
    pub recorded_at_ms: i64,
    /// The event.
    pub event: EventMsg,
}

/// Generic capability UI updates understood by every frontend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frontend_type", rename_all = "snake_case")]
pub enum FrontendEvent {
    /// Selects the render case.
    Render {
        /// The capability.
        capability: String,
        /// The block.
        block: FrontendBlock,
    },
    /// Selects the widget case.
    Widget {
        /// The capability.
        capability: String,
        /// The item.
        item: FrontendWidget,
    },
    /// Selects the remove widget case.
    RemoveWidget {
        /// The capability.
        capability: String,
        /// The identifier.
        id: String,
    },
    /// Selects the picker case.
    Picker {
        /// The title.
        title: String,
        /// The options.
        options: Vec<FrontendPickerOption>,
    },
    /// Selects the preview case.
    Preview {
        /// The identifier.
        id: String,
        /// The title.
        title: String,
        /// The subtitle.
        subtitle: String,
        /// The page identifier.
        page_id: String,
        /// The update.
        update: FrontendPreviewUpdate,
        /// The events.
        events: Vec<FrontendPreviewEvent>,
        /// The next.
        next: Option<Op>,
    },
}

/// How one preview page changes the matching frontend preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendPreviewUpdate {
    /// Selects the replace case.
    Replace,
    /// Selects the prepend case.
    Prepend,
}

/// A presentation hint rather than a terminal-specific color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendTone {
    /// Selects the neutral case.
    Neutral,
    /// Selects the success case.
    Success,
    /// Selects the warning case.
    Warning,
    /// Selects the error case.
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
                content: Default::default(),
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
                content: Default::default(),
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
                content: Default::default(),
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
                    content: Default::default(),
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
                content: Default::default(),
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
                    content: Default::default(),
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
    /// Selects the agent case.
    Agent,
    /// Selects the brain case.
    Brain,
    /// Selects the branch case.
    Branch,
    /// Selects the chat case.
    Chat,
    /// Selects the delete case.
    Delete,
    /// Selects the edit case.
    Edit,
    /// Selects the progress case.
    Progress,
    /// Selects the promote case.
    Promote,
    /// Selects the route case.
    Route,
    /// Selects the search case.
    Search,
    /// Selects the shield case.
    Shield,
    /// Selects the shield alert case.
    ShieldAlert,
    /// Selects the shield check case.
    ShieldCheck,
    /// Selects the shield off case.
    ShieldOff,
    /// Selects the sparkle case.
    Sparkle,
    /// Selects the storage case.
    Storage,
    /// Selects the task case.
    Task,
    /// Selects the custom case.
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
            Self::Progress => "progress",
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
            "progress" => Self::Progress,
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
