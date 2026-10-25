//! The gateway-reduced transcript every client renders: items, the rows that group them,
//! and the ops that change both.
//!
//! A client keeps items by id and rows by id, and shows the rows no other row lists as a
//! child sorted by `position`, each row's children in the listed order: an item child is
//! the item, a row child is that nested row drawn in place. Ops apply in order and each
//! carries only what changed:
//! `Item` and `Row` insert or replace, `AppendText`, `AppendBlock` and `ExtendRow` add to
//! what the client already has, and `RemoveRow` drops a row. See [`TranscriptOp`] for the
//! exact rule of each. A page replaces or, for older history, prepends; an older page never
//! replaces an id the client already has.

use std::cmp::Ordering;
use std::ops::Not;
use std::sync::Arc;

use mobius::SessionId;
use mobius::protocol::{
    ContentPart, FrontendBlock, FrontendBlockFormat, FrontendBlockRole, FrontendBlockState,
    FrontendLink, FrontendSymbol, FrontendTone, MessageEvent, MessageTarget, ModelStepAnnotation,
    ModelStepContentPhase, SessionFileReference, UiBlock,
};
use serde::{Deserialize, Serialize};

use super::DiffTotals;

/// Where a row sorts: farther inherited history first, then journal sequence, then the
/// order within one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TranscriptPosition {
    /// How many forks up the item's chat is; the chat itself is 0.
    pub depth: u32,
    /// The journal sequence of the event that created the item.
    pub sequence: u64,
    /// The order within that event.
    pub index: u32,
}

impl Ord for TranscriptPosition {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .depth
            .cmp(&self.depth)
            .then(self.sequence.cmp(&other.sequence))
            .then(self.index.cmp(&other.index))
    }
}

impl PartialOrd for TranscriptPosition {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Whether an item is still changing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptItemState {
    /// Text still arriving as `append_text` deltas.
    Streaming,
    /// A block its capability has not finished.
    Pending,
    /// Final until a later op replaces it.
    #[default]
    Complete,
}

/// One semantic transcript entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptItem {
    /// Stable identity: `message:{submission}`, `assistant:{step}:{phase}`,
    /// `block:{capability}:{id}`, `block:{sequence}:{index}`, `notice:{sequence}:{index}`
    /// or `fork-point`; inherited items add a `fork:{depth}:` prefix.
    pub id: Arc<str>,
    /// The turn the item arrived in.
    pub turn_id: Option<Arc<str>>,
    /// When the item was first recorded, in Unix milliseconds.
    pub recorded_at_ms: i64,
    /// Whether the item is still changing.
    pub state: TranscriptItemState,
    /// The answer or failure that ended its turn.
    #[serde(default, skip_serializing_if = "Not::not")]
    pub terminal: bool,
    /// The message that opened its turn.
    #[serde(default, skip_serializing_if = "Not::not")]
    pub starts_turn: bool,
    /// The opening message of the first turn after the agent restarted on an existing chat:
    /// the chat was left and taken up again here.
    #[serde(default, skip_serializing_if = "Not::not")]
    pub resumed: bool,
    /// History of the chat this one was forked from.
    #[serde(default, skip_serializing_if = "Not::not")]
    pub inherited: bool,
    /// The durable message this item is, for replies, pins and message actions; never set
    /// on inherited items.
    pub target: Option<MessageTarget>,
    /// What the item shows.
    #[serde(flatten)]
    pub content: TranscriptItemContent,
}

/// What a transcript item shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptItemContent {
    /// A user, peer or relayed message, with its attachments and reply.
    Message {
        /// The message.
        message: MessageEvent,
    },
    /// One phase of a model step's text.
    Assistant {
        /// The phase.
        phase: ModelStepContentPhase,
        /// The phase's text parts, joined by newlines.
        text: String,
        /// Citations of the phase's text parts.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        annotations: Vec<ModelStepAnnotation>,
    },
    /// A capability-rendered block, with every append already merged: `update` is always
    /// `replace`.
    Block {
        /// The capability that rendered it.
        capability: Arc<str>,
        /// The block.
        block: FrontendBlock,
        /// The title of the first block of this block's group, when that is another
        /// block, such as the command a background-command poll is about.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_title: Option<Arc<str>>,
    },
    /// A framework notice for an event no capability rendered.
    Notice {
        /// The tone.
        tone: FrontendTone,
        /// The text.
        text: Arc<str>,
    },
    /// Where a fork of `parent` begins: inherited history above it, the chat's own below.
    ForkPoint {
        /// The chat this one was forked from.
        parent: SessionId,
    },
}

/// One visible transcript row grouping one or more items, and for a `worked` row the
/// nested `activity` rows of its runs of work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptRow {
    /// Stable identity: the item id for single-item rows, `worked:{turn}` or
    /// `activity:{first item}`, with the same `fork:{depth}:` prefix as inherited items.
    pub id: Arc<str>,
    /// Where the row sorts: its first item's position.
    pub position: TranscriptPosition,
    /// The row's children in display order.
    pub items: Vec<TranscriptChild>,
    /// How the row shows its items.
    #[serde(flatten)]
    pub kind: TranscriptRowKind,
}

/// One child of a row: `{"item": id}` or `{"row": id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptChild {
    /// An item, drawn as a row of its own kind would draw it.
    Item(Arc<str>),
    /// A nested row, drawn in place and never among the top-level rows.
    Row(Arc<str>),
}

impl TranscriptChild {
    /// The item id, for an item child.
    pub const fn item(&self) -> Option<&Arc<str>> {
        match self {
            Self::Item(id) => Some(id),
            Self::Row(_) => None,
        }
    }
}

/// How a transcript row shows its items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptRowKind {
    /// One message, answer or deliverable.
    Entry {
        /// What the turn changed, on its last terminal answer only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_changes: Option<TurnChanges>,
    },
    /// One framework or capability notice, never grouped.
    Notice,
    /// A run of consecutive activity: nested in a `worked` row between its other children,
    /// or outside a started turn, joined while consecutive items share a turn or block
    /// group.
    Activity {
        /// What the items are.
        counts: ActivityCounts,
    },
    /// A started turn's work: "Working" while `duration_ms` is `None`, then "Worked for".
    /// Its children are the turn's commentary, messages, notices and folded answers as
    /// items, and each run of work between them as one nested `activity` row.
    Worked {
        /// What all its work is, nested runs included.
        counts: ActivityCounts,
        /// When the turn started, in Unix milliseconds.
        started_at_ms: i64,
        /// How long the turn took, once it ended.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
}

/// How many items of each kind a grouped row holds; zero counts are omitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityCounts {
    /// Reasoning text.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub thoughts: u32,
    /// Tool blocks.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tool_calls: u32,
    /// Web search blocks.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub web_searches: u32,
    /// Approval blocks.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub approvals: u32,
    /// Artifact blocks.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub artifacts: u32,
    /// Everything else.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub events: u32,
    /// Error-toned blocks and notices.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub errors: u32,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes skipped fields by reference"
)]
const fn is_zero(count: &u32) -> bool {
    *count == 0
}

/// The finished file edits of one turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnChanges {
    /// Ids of the turn's complete unified-diff tool blocks, in order.
    pub diffs: Vec<Arc<str>>,
    /// Added and removed lines across them.
    pub totals: DiffTotals,
}

/// One change to a client's transcript. Within one event, item ops (`Item`, `AppendText`,
/// `AppendBlock`) come before row ops (`RemoveRow`, `Row`, `ExtendRow`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TranscriptOp {
    /// Inserts the item with this id, or replaces it whole. Sent when an item is created,
    /// when its block is replaced, and when anything [`BlockDelta`] cannot express changes,
    /// such as `terminal`, `target`, a nullable field becoming `None`, or two appends to one
    /// block in one event.
    Item {
        /// The item.
        item: Arc<TranscriptItem>,
    },
    /// Pushes `text` onto the message or assistant text of the item `id`. Ignored for an
    /// unknown id; the following `Item` repairs it.
    AppendText {
        /// The item.
        id: Arc<str>,
        /// The delta.
        text: String,
    },
    /// Applies `delta` to the block item `id`, which the client already holds; see
    /// [`BlockDelta`] for the rule.
    AppendBlock {
        /// The item.
        id: Arc<str>,
        /// What the block gained.
        delta: BlockDelta,
    },
    /// Inserts the row with this id, or replaces it whole. Sent when a row is created,
    /// when its kind variant changes, and when its children change other than by
    /// appending: a child leaves the row, or joins anywhere but its end, as when a block's
    /// tone, format or role moves it in or out of the row after later items joined. A
    /// nested row is sent like any other, in the same event as the parent op listing it.
    Row {
        /// The row.
        row: Arc<TranscriptRow>,
    },
    /// Pushes `items` onto the end of the row `id`, which the client already holds, and
    /// replaces its kind with `kind`. Its position never changes. Work joining a nested run
    /// extends that run, and its `worked` row only by kind.
    ExtendRow {
        /// The row.
        id: Arc<str>,
        /// Children to append, in order; often empty when only the kind changed.
        items: Vec<TranscriptChild>,
        /// The row's new kind, always the same variant as before.
        #[serde(flatten)]
        kind: TranscriptRowKind,
    },
    /// Drops the row with this id; its items stay for any row that still lists them. A
    /// removed row's nested rows are removed by their own ops.
    RemoveRow {
        /// The row.
        id: Arc<str>,
    },
}

/// What one append added to a block item, already merged by the gateway. A client applies
/// it mechanically to the item's `Block` content, with no merge rules of its own:
/// - `text` is concatenated onto `block.text` as is (any separating newline is included);
/// - `links`, `content` and `ui` are pushed onto `block.links`, `block.content` and
///   `block.ui`;
/// - each of `files` replaces the file with its id in `block.files`, or is pushed when no
///   file has that id;
/// - each `Some` scalar replaces the field it names (`item_state` and `group_title` on the
///   item, the rest on the block); `None` leaves it unchanged.
///
/// Absent fields are empty or `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockDelta {
    /// Text to concatenate onto the block's text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Links to push.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<FrontendLink>,
    /// Files to upsert by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<SessionFileReference>,
    /// Observations to push.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ContentPart>,
    /// Interface blocks to push.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui: Vec<UiBlock>,
    /// The block's new title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The block's new state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<FrontendBlockState>,
    /// The block's new role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<FrontendBlockRole>,
    /// The block's new tone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<FrontendTone>,
    /// The block's new format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<FrontendBlockFormat>,
    /// The block's new symbol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<FrontendSymbol>,
    /// The block's new group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The item's new `group_title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_title: Option<Arc<str>>,
    /// The item's new `state`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_state: Option<TranscriptItemState>,
}

/// A self-contained transcript: a snapshot, a history page or a preview.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptPage {
    /// Items in position order.
    pub items: Vec<Arc<TranscriptItem>>,
    /// Rows in position order.
    pub rows: Vec<Arc<TranscriptRow>>,
}

/// Where the next older history page starts; clients pass it back unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCursor {
    /// The chat the page is read from: the opened chat or one it was forked from.
    pub session_id: SessionId,
    /// The page holds the turn before this journal sequence.
    pub before_sequence: u64,
}
