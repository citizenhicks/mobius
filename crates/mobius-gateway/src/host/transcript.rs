//! The one transcript reducer: journal events and their rendered blocks become the
//! items and rows every client shows, and the ops that bring a client from one state to
//! the next.
//!
//! A started turn shows as its opening message, one `worked` row holding its work
//! ("Working" while it runs, "Worked for" once it ends) with each run of consecutive
//! activity between its commentary nested as an `activity` row, the deliverables and
//! errors that stay in view, and its terminal answer. Everything outside a started turn stands
//! on its own row, with consecutive activity of one turn or block group joined.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use mobius::SessionId;
use mobius::protocol::{
    AssistantContentDeltaEvent, AssistantMessageEvent, Event, EventMsg, FrontendBlock,
    FrontendBlockFormat, FrontendBlockRole, FrontendBlockState, FrontendBlockUpdate, FrontendTone,
    MessageAuthor, MessageDelivery, MessageEvent, MessageSource, ModelStepContentPhase,
    RenderedBlock,
};

use crate::wire::{
    ActivityCounts, BlockDelta, DiffTotals, TranscriptChild, TranscriptItem, TranscriptItemContent,
    TranscriptItemState, TranscriptOp, TranscriptPage, TranscriptPosition, TranscriptRow,
    TranscriptRowKind, TurnChanges,
};

/// Indices each event reserves ahead of its blocks: one message or notice, or one item per
/// model step phase.
const BLOCK_INDEX_BASE: u32 = 3;

#[derive(Debug)]
struct Slot {
    item: Arc<TranscriptItem>,
    position: TranscriptPosition,
    /// The started turn holding the item, or `None` for items shown on their own.
    turn: Option<Arc<str>>,
}

#[derive(Debug)]
struct Turn {
    started_at_ms: i64,
    start_sequence: u64,
    ended_at_ms: Option<i64>,
    items: Vec<Arc<str>>,
    rows: Vec<Arc<str>>,
}

/// What one event changed, published as ops once the event is reduced.
struct Step {
    sequence: u64,
    base: u32,
    at: i64,
    ops: Vec<TranscriptOp>,
    /// Changed items, each with what it gained when only an append changed it.
    items: Vec<(Arc<str>, Option<BlockDelta>)>,
    turns: Vec<Arc<str>>,
    loose: bool,
}

impl Step {
    /// Marks the item `id` changed in a way only a whole `Item` op carries.
    fn touch(&mut self, id: &Arc<str>, turn: Option<&Arc<str>>) {
        match self.items.iter_mut().find(|(item, _)| item == id) {
            Some((_, delta)) => *delta = None,
            None => self.items.push((Arc::clone(id), None)),
        }
        self.touch_turn(turn);
    }

    /// Marks the block item `id` appended to. A second change to it in this event sends
    /// the whole item instead.
    fn touch_delta(&mut self, id: &Arc<str>, turn: Option<&Arc<str>>, delta: BlockDelta) {
        match self.items.iter_mut().find(|(item, _)| item == id) {
            Some((_, current)) => *current = None,
            None => self.items.push((Arc::clone(id), Some(delta))),
        }
        self.touch_turn(turn);
    }

    fn touch_turn(&mut self, turn: Option<&Arc<str>>) {
        match turn {
            Some(turn) if !self.turns.contains(turn) => self.turns.push(Arc::clone(turn)),
            Some(_) => {}
            None => self.loose = true,
        }
    }
}

/// Reduces one chat's events, or one inherited chat's events `depth` forks up.
#[derive(Debug)]
#[cfg_attr(
    not(feature = "fixtures"),
    expect(unreachable_pub, reason = "public only to the fixtures feature")
)]
pub struct TranscriptReducer {
    depth: u32,
    items: HashMap<Arc<str>, Slot>,
    rows: HashMap<Arc<str>, Arc<TranscriptRow>>,
    turns: HashMap<Arc<str>, Turn>,
    loose: Vec<Arc<str>>,
    loose_rows: Vec<Arc<str>>,
    /// Each block group's first titled block and its title.
    groups: HashMap<Arc<str>, (Arc<str>, Arc<str>)>,
    current_turn: Option<Arc<str>>,
    /// The started turn whose opening message has not arrived yet.
    awaiting_input: Option<Arc<str>>,
    /// The last reduced sequence and its next free index, for events sharing a sequence.
    last_event: Option<(u64, u32)>,
    /// Events before this sequence were left out or trimmed.
    cut: Option<u64>,
    /// A turn started before the newest agent start.
    follows_turn: bool,
    /// The agent restarted after a turn; the next opening message is a resumed one.
    resumed: bool,
}

#[cfg_attr(
    not(feature = "fixtures"),
    expect(unreachable_pub, reason = "public only to the fixtures feature")
)]
impl TranscriptReducer {
    /// A reducer for a chat `depth` forks up from the one shown; `0` is the chat itself.
    pub fn new(depth: u32) -> Self {
        Self {
            depth,
            items: HashMap::new(),
            rows: HashMap::new(),
            turns: HashMap::new(),
            loose: Vec::new(),
            loose_rows: Vec::new(),
            groups: HashMap::new(),
            current_turn: None,
            awaiting_input: None,
            last_event: None,
            cut: None,
            follows_turn: false,
            resumed: false,
        }
    }

    /// Reduces one event and its rendered blocks, returning the ops a client applies.
    pub fn apply(
        &mut self,
        sequence: u64,
        recorded_at_ms: i64,
        event: &Event,
        blocks: Vec<RenderedBlock>,
    ) -> Vec<TranscriptOp> {
        let base = match self.last_event {
            Some((last, next)) if last == sequence => next,
            _ => 0,
        };
        let reserved = u32::try_from(blocks.len())
            .unwrap_or(u32::MAX)
            .saturating_add(BLOCK_INDEX_BASE);
        self.last_event = Some((sequence, base.saturating_add(reserved)));
        let mut step = Step {
            sequence,
            base,
            at: recorded_at_ms,
            ops: Vec::new(),
            items: Vec::new(),
            turns: Vec::new(),
            loose: false,
        };
        let submission = event.submission_id.as_deref();
        let rendered = !blocks.is_empty();
        match &event.msg {
            EventMsg::SessionConfigured(_) => self.resumed |= self.follows_turn,
            EventMsg::TurnStarted(turn) => self.start_turn(&mut step, &turn.turn_id),
            EventMsg::TurnComplete(turn) => self.finish_turn(&mut step, &turn.turn_id),
            EventMsg::TurnAborted(turn) => {
                self.finish_turn(&mut step, &turn.turn_id);
                if !rendered {
                    self.notice(&mut step, FrontendTone::Warning, &turn.reason, true);
                }
            }
            // External reports keep their capability-owned presentation.
            EventMsg::Message(message)
                if rendered && message.author != MessageAuthor::User && !from_session(message) => {}
            EventMsg::Message(message) => self.message(&mut step, submission, message),
            EventMsg::MessageDelta(delta) => self.message_delta(&mut step, submission, &delta.text),
            EventMsg::AssistantContentDelta(delta) => self.assistant_delta(&mut step, delta),
            EventMsg::AssistantMessage(message) => self.assistant(&mut step, message),
            EventMsg::Error(error) if !rendered => {
                self.notice(&mut step, FrontendTone::Error, &error.message, false);
            }
            EventMsg::Warning(warning) if !rendered => {
                self.notice(&mut step, FrontendTone::Warning, &warning.message, false);
            }
            EventMsg::SubmissionRejected(rejected) if !rendered => {
                let text: Arc<str> = rejected.message.as_str().into();
                self.notice(&mut step, FrontendTone::Error, &text, false);
            }
            _ => {}
        }
        let ends_turn = matches!(event.msg, EventMsg::Error(_) | EventMsg::TurnAborted(_));
        let first_block = self.blocks(&mut step, blocks, ends_turn);
        if let EventMsg::Message(message) = &event.msg
            && message.author != MessageAuthor::User
            && !from_session(message)
            && let Some(id) = first_block
        {
            let opens = self.opens_turn(message);
            let target = self.own(message.message_target);
            if let Some(item) = self.edit(&mut step, &id) {
                item.target = target;
                item.starts_turn = opens;
            }
        }
        // Whatever arrives between turns belongs to neither.
        if let EventMsg::TurnComplete(mobius::protocol::TurnCompleteEvent { turn_id })
        | EventMsg::TurnAborted(mobius::protocol::TurnAbortedEvent { turn_id, .. }) = &event.msg
            && self.current_turn.as_ref() == Some(turn_id)
        {
            self.current_turn = None;
            self.awaiting_input = None;
        }
        self.publish(step)
    }

    /// Adds the divider between inherited history and this chat's own.
    pub fn fork_point(&mut self, parent: SessionId) {
        let current = self.current_turn.take();
        let mut step = Self::quiet_step(0, 0);
        let id = self.id(format_args!("fork-point"));
        self.set(
            &mut step,
            id,
            0,
            TranscriptItemContent::ForkPoint { parent },
        );
        self.publish(step);
        self.current_turn = current;
    }

    /// Marks the events about to be reduced as following an earlier turn left out of them.
    pub(super) const fn follow_turn(&mut self) {
        self.follows_turn = true;
    }

    /// Ends every turn still open: an inherited chat continued past the fork point in
    /// its own history, not in this one.
    pub(super) fn finish_open_turns(&mut self) {
        let open = self
            .turns
            .iter()
            .filter(|(_, turn)| turn.ended_at_ms.is_none())
            .map(|(id, turn)| {
                let ended = turn
                    .items
                    .iter()
                    .filter_map(|item| Some(self.items.get(item)?.item.recorded_at_ms))
                    .max()
                    .unwrap_or(turn.started_at_ms);
                (Arc::clone(id), ended)
            })
            .collect::<Vec<_>>();
        for (turn, ended) in open {
            let mut step = Self::quiet_step(0, ended);
            self.finish_turn(&mut step, &turn);
            self.publish(step);
        }
    }

    /// Drops whole finished turns that started before `sequence`, and rows outside turns
    /// that lie entirely before it, mirroring a replay-window truncation. The running
    /// turn always stays.
    pub(super) fn trim_before(&mut self, sequence: u64) {
        let old_turns = self
            .turns
            .iter()
            .filter(|(id, turn)| {
                turn.ended_at_ms.is_some()
                    && turn.start_sequence < sequence
                    && self.current_turn.as_ref() != Some(*id)
            })
            .map(|(id, _)| Arc::clone(id))
            .collect::<Vec<_>>();
        for id in old_turns {
            if let Some(turn) = self.turns.remove(&id) {
                for item in turn.items {
                    self.items.remove(&item);
                }
                for row in turn.rows {
                    self.rows.remove(&row);
                }
            }
        }
        let old_rows = self
            .loose_rows
            .iter()
            .filter(|row| {
                self.rows.get(*row).is_some_and(|row| {
                    row.items
                        .iter()
                        .filter_map(TranscriptChild::item)
                        .all(|item| {
                            self.items
                                .get(item)
                                .is_none_or(|slot| slot.position.sequence < sequence)
                        })
                })
            })
            .map(Arc::clone)
            .collect::<Vec<_>>();
        for id in &old_rows {
            if let Some(row) = self.rows.remove(id) {
                for item in row.items.iter().filter_map(TranscriptChild::item) {
                    self.items.remove(item);
                }
            }
        }
        self.loose_rows.retain(|row| !old_rows.contains(row));
        // Items left without a row, such as a dropped repeat, go with the rows before them.
        let shown = self
            .loose_rows
            .iter()
            .filter_map(|row| self.rows.get(row))
            .flat_map(|row| row.items.iter().filter_map(TranscriptChild::item))
            .map(Arc::clone)
            .collect::<HashSet<_>>();
        let (kept, dropped): (Vec<_>, Vec<_>) = std::mem::take(&mut self.loose)
            .into_iter()
            .partition(|item| {
                shown.contains(item)
                    || self
                        .items
                        .get(item)
                        .is_some_and(|slot| slot.position.sequence >= sequence)
            });
        for item in dropped {
            self.items.remove(&item);
        }
        self.loose = kept;
        self.cut = Some(self.cut.map_or(sequence, |cut| cut.max(sequence)));
    }

    /// The journal sequence the next older history page starts before, once older
    /// events were left out of this reducer.
    pub(super) fn history_before(&self) -> Option<u64> {
        let cut = self.cut?;
        Some(
            self.turns
                .values()
                .map(|turn| turn.start_sequence)
                .min()
                .map_or(cut, |start| start.min(cut)),
        )
    }

    /// Everything reduced so far, in position order.
    pub(super) fn snapshot(&self) -> TranscriptPage {
        let mut items = self.items.values().collect::<Vec<_>>();
        items.sort_by_key(|slot| slot.position);
        let mut rows = self.rows.values().collect::<Vec<_>>();
        rows.sort_by(|a, b| row_order(a, b));
        TranscriptPage {
            items: items
                .into_iter()
                .map(|slot| Arc::clone(&slot.item))
                .collect(),
            rows: rows.into_iter().map(Arc::clone).collect(),
        }
    }

    /// Everything reduced, for a page that is read once.
    pub fn page(self) -> TranscriptPage {
        let mut items = self.items.into_values().collect::<Vec<_>>();
        items.sort_by_key(|slot| slot.position);
        let mut rows = self.rows.into_values().collect::<Vec<_>>();
        rows.sort_by(|a, b| row_order(a, b));
        TranscriptPage {
            items: items.into_iter().map(|slot| slot.item).collect(),
            rows,
        }
    }

    const fn quiet_step(sequence: u64, at: i64) -> Step {
        Step {
            sequence,
            base: 0,
            at,
            ops: Vec::new(),
            items: Vec::new(),
            turns: Vec::new(),
            loose: false,
        }
    }

    /// An id of this reducer's chat: inherited ids say how many forks up they come from.
    fn id(&self, base: fmt::Arguments<'_>) -> Arc<str> {
        if self.depth == 0 {
            fmt::format(base).into()
        } else {
            format!("fork:{}:{base}", self.depth).into()
        }
    }

    /// A message target, which only this chat's own items keep.
    const fn own(
        &self,
        target: Option<mobius::protocol::MessageTarget>,
    ) -> Option<mobius::protocol::MessageTarget> {
        if self.depth == 0 { target } else { None }
    }

    fn start_turn(&mut self, step: &mut Step, turn_id: &Arc<str>) {
        if let Some(current) = self.current_turn.take()
            && current != *turn_id
        {
            self.finish_turn(step, &current);
        }
        let turn = self.turns.entry(Arc::clone(turn_id)).or_insert(Turn {
            started_at_ms: step.at,
            start_sequence: step.sequence,
            ended_at_ms: None,
            items: Vec::new(),
            rows: Vec::new(),
        });
        turn.started_at_ms = step.at;
        turn.ended_at_ms = None;
        if !step.turns.contains(turn_id) {
            step.turns.push(Arc::clone(turn_id));
        }
        self.current_turn = Some(Arc::clone(turn_id));
        self.awaiting_input = Some(Arc::clone(turn_id));
        self.follows_turn = true;
    }

    /// Ends a started turn: its streams are complete and its last answer is terminal.
    fn finish_turn(&mut self, step: &mut Step, turn_id: &Arc<str>) {
        let Some(turn) = self.turns.get_mut(turn_id) else {
            return;
        };
        if turn.ended_at_ms.is_some() {
            return;
        }
        turn.ended_at_ms = Some(step.at);
        let items = std::mem::take(&mut turn.items);
        let answer = items
            .iter()
            .filter_map(|id| Some((id, self.items.get(id)?)))
            .filter(|(_, slot)| is_answer(&slot.item))
            .max_by_key(|(_, slot)| slot.position)
            .map(|(id, _)| Arc::clone(id));
        for id in &items {
            let terminal = answer.as_ref() == Some(id);
            if let Some(item) = self.edit_if(step, id, |item| {
                item.state == TranscriptItemState::Streaming || terminal && !item.terminal
            }) {
                if item.state == TranscriptItemState::Streaming {
                    item.state = TranscriptItemState::Complete;
                }
                item.terminal |= terminal;
            }
        }
        if let Some(turn) = self.turns.get_mut(turn_id) {
            turn.items = items;
        }
        if !step.turns.contains(turn_id) {
            step.turns.push(Arc::clone(turn_id));
        }
    }

    /// Whether `message` opens the started turn: its first message that is not a steer.
    fn opens_turn(&mut self, message: &MessageEvent) -> bool {
        let opens = message.delivery != MessageDelivery::Steer
            && self.awaiting_input.is_some()
            && self.awaiting_input == self.current_turn;
        if opens {
            self.awaiting_input = None;
        }
        opens
    }

    fn message_id(&self, step: &Step, submission: Option<&str>) -> Arc<str> {
        match submission {
            Some(submission) => self.id(format_args!("message:{submission}")),
            None => self.id(format_args!("event:{}:{}", step.sequence, step.base)),
        }
    }

    fn message(&mut self, step: &mut Step, submission: Option<&str>, message: &MessageEvent) {
        if message.text.is_empty() && message.attachments.is_empty() {
            return;
        }
        let id = self.message_id(step, submission);
        let opens = self.opens_turn(message);
        let target = self.own(message.message_target);
        let mut message = message.clone(); // rules-allow: the journal record keeps its event and the item owns the message it shows
        message.message_target = target;
        let resumed = opens && std::mem::take(&mut self.resumed);
        let item = self.set(step, id, 0, TranscriptItemContent::Message { message });
        item.state = TranscriptItemState::Complete;
        item.starts_turn |= opens;
        item.resumed |= resumed;
        item.target = target;
    }

    fn message_delta(&mut self, step: &mut Step, submission: Option<&str>, text: &str) {
        let id = self.message_id(step, submission);
        if self.append(step, &id, text) {
            return;
        }
        let message = MessageEvent {
            author: MessageAuthor::User,
            delivery: MessageDelivery::Turn,
            text: text.to_owned(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        };
        let item = self.set(step, id, 0, TranscriptItemContent::Message { message });
        item.state = TranscriptItemState::Streaming;
    }

    fn assistant_delta(&mut self, step: &mut Step, delta: &AssistantContentDeltaEvent) {
        self.current_turn = Some(Arc::clone(&delta.turn_id));
        let id = self.id(format_args!(
            "assistant:{}:{}",
            delta.model_step_id,
            phase_name(delta.phase)
        ));
        if self.append(step, &id, &delta.delta) {
            return;
        }
        let content = TranscriptItemContent::Assistant {
            phase: delta.phase,
            text: delta.delta.clone(), // rules-allow: the journal record keeps its event and the item owns the text it shows
            annotations: Vec::new(),
        };
        let item = self.set(step, id, phase_index(delta.phase), content);
        item.state = TranscriptItemState::Streaming;
    }

    fn assistant(&mut self, step: &mut Step, message: &AssistantMessageEvent) {
        self.current_turn = Some(Arc::clone(&message.turn_id));
        let target_phase = message
            .content
            .iter()
            .rfind(|part| part.phase != ModelStepContentPhase::Reasoning && !part.text.is_empty())
            .map(|part| part.phase);
        for phase in [
            ModelStepContentPhase::Reasoning,
            ModelStepContentPhase::Commentary,
            ModelStepContentPhase::FinalAnswer,
        ] {
            let parts = || {
                message
                    .content
                    .iter()
                    .filter(move |part| part.phase == phase)
            };
            let text = parts()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                continue;
            }
            let annotations = parts()
                .flat_map(|part| part.annotations.iter().cloned()) // rules-allow: the journal record keeps its event and the item owns the citations it shows
                .collect();
            let id = self.id(format_args!(
                "assistant:{}:{}",
                message.model_step_id,
                phase_name(phase)
            ));
            let target = self.own(
                message
                    .message_target
                    .filter(|_| target_phase == Some(phase)),
            );
            let content = TranscriptItemContent::Assistant {
                phase,
                text,
                annotations,
            };
            let item = self.set(step, id, phase_index(phase), content);
            item.state = TranscriptItemState::Complete;
            item.target = target;
        }
    }

    fn notice(&mut self, step: &mut Step, tone: FrontendTone, text: &Arc<str>, terminal: bool) {
        let id = self.id(format_args!("notice:{}:{}", step.sequence, step.base));
        let content = TranscriptItemContent::Notice {
            tone,
            text: Arc::clone(text),
        };
        let item = self.set(step, id, 0, content);
        item.terminal |= terminal;
    }

    /// Applies the event's blocks, merging appends, and returns the first block's id.
    fn blocks(
        &mut self,
        step: &mut Step,
        blocks: Vec<RenderedBlock>,
        ends_turn: bool,
    ) -> Option<Arc<str>> {
        let mut first = None;
        for (offset, rendered) in (0_u32..).zip(blocks) {
            let RenderedBlock {
                capability,
                mut block,
            } = rendered;
            let id = match &block.id {
                Some(block_id) => self.id(format_args!("block:{capability}:{block_id}")),
                None => self.id(format_args!(
                    "block:{}:{}",
                    step.sequence,
                    step.base
                        .saturating_add(BLOCK_INDEX_BASE)
                        .saturating_add(offset)
                )),
            };
            if let Some(slot) = self.items.get(&id)
                && let TranscriptItemContent::Block {
                    block: previous, ..
                } = &slot.item.content
            {
                if block.update == FrontendBlockUpdate::Append {
                    self.append_block(step, &id, block, ends_turn);
                    first.get_or_insert(id);
                    continue;
                }
                if block.group.is_none() {
                    block.group.clone_from(&previous.group); // rules-allow: a replacement without a group keeps the published block's group
                }
            }
            block.update = FrontendBlockUpdate::Replace;
            let group_title = group_title(&mut self.groups, &id, &block);
            let state = item_state(&block);
            let index = BLOCK_INDEX_BASE.saturating_add(offset);
            let content = TranscriptItemContent::Block {
                capability: capability.into(),
                block,
                group_title,
            };
            let item = self.set(step, Arc::clone(&id), index, content);
            item.state = state;
            item.terminal |= ends_turn;
            first.get_or_insert(id);
        }
        first
    }

    /// Merges an Append onto the known block item `id`, recording only what it added. What
    /// a delta cannot say, such as a field becoming `None` or the item turning terminal,
    /// sends the whole item.
    fn append_block(
        &mut self,
        step: &mut Step,
        id: &Arc<str>,
        block: FrontendBlock,
        ends_turn: bool,
    ) {
        let Some(slot) = self.items.get_mut(id) else {
            return;
        };
        // The published item stays as it was; the merge builds its successor.
        let item = Arc::make_mut(&mut slot.item);
        let TranscriptItemContent::Block {
            block: merged,
            group_title: previous_title,
            ..
        } = &mut item.content
        else {
            return;
        };
        let (mut delta, mut whole) = merge(merged, block);
        let group_title = group_title(&mut self.groups, id, merged);
        if group_title != *previous_title {
            whole |= group_title.is_none();
            delta.group_title = group_title.as_ref().map(Arc::clone);
            *previous_title = group_title;
        }
        let state = item_state(merged);
        if state != item.state {
            item.state = state;
            delta.item_state = Some(state);
        }
        whole |= ends_turn && !item.terminal;
        item.terminal |= ends_turn;
        if whole {
            step.touch(id, slot.turn.as_ref());
        } else if delta != BlockDelta::default() {
            step.touch_delta(id, slot.turn.as_ref(), delta);
        }
    }

    /// Appends streamed text to a known message or assistant item.
    fn append(&mut self, step: &mut Step, id: &Arc<str>, delta: &str) -> bool {
        let Some(slot) = self.items.get_mut(id) else {
            return false;
        };
        let item = Arc::make_mut(&mut slot.item);
        let text = match &mut item.content {
            TranscriptItemContent::Message { message } => &mut message.text,
            TranscriptItemContent::Assistant { text, .. } => text,
            _ => return false,
        };
        text.push_str(delta);
        if item.state == TranscriptItemState::Streaming {
            step.ops.push(TranscriptOp::AppendText {
                id: Arc::clone(id),
                text: delta.to_owned(),
            });
        } else {
            item.state = TranscriptItemState::Streaming;
            step.touch(id, slot.turn.as_ref());
        }
        true
    }

    /// Sets the content of the item `id`, adding it at `index` of this event when new.
    fn set(
        &mut self,
        step: &mut Step,
        id: Arc<str>,
        index: u32,
        content: TranscriptItemContent,
    ) -> &mut TranscriptItem {
        let turn = self
            .current_turn
            .as_ref()
            .filter(|turn| self.turns.contains_key(*turn))
            .map(Arc::clone);
        let slot = match self.items.entry(id) {
            Entry::Occupied(slot) => {
                let slot = slot.into_mut();
                Arc::make_mut(&mut slot.item).content = content;
                slot
            }
            Entry::Vacant(slot) => {
                let id = Arc::clone(slot.key());
                match &turn {
                    Some(turn) => {
                        if let Some(turn) = self.turns.get_mut(turn) {
                            turn.items.push(Arc::clone(&id));
                        }
                    }
                    None => self.loose.push(Arc::clone(&id)),
                }
                slot.insert(Slot {
                    item: Arc::new(TranscriptItem {
                        id: Arc::clone(&id),
                        turn_id: self.current_turn.as_ref().map(Arc::clone),
                        recorded_at_ms: step.at,
                        state: TranscriptItemState::Complete,
                        terminal: false,
                        starts_turn: false,
                        resumed: false,
                        inherited: self.depth > 0,
                        target: None,
                        content,
                    }),
                    position: TranscriptPosition {
                        depth: self.depth,
                        sequence: step.sequence,
                        index: step.base.saturating_add(index),
                    },
                    turn,
                })
            }
        };
        step.touch(&slot.item.id, slot.turn.as_ref());
        Arc::make_mut(&mut slot.item)
    }

    /// The item `id` for editing, marked changed.
    fn edit(&mut self, step: &mut Step, id: &Arc<str>) -> Option<&mut TranscriptItem> {
        self.edit_if(step, id, |_| true)
    }

    fn edit_if(
        &mut self,
        step: &mut Step,
        id: &Arc<str>,
        changes: impl FnOnce(&TranscriptItem) -> bool,
    ) -> Option<&mut TranscriptItem> {
        let slot = self.items.get_mut(id)?;
        if !changes(&slot.item) {
            return None;
        }
        step.touch(id, slot.turn.as_ref());
        Some(Arc::make_mut(&mut slot.item))
    }

    /// Emits every changed item, then every row whose grouping changed.
    fn publish(&mut self, mut step: Step) -> Vec<TranscriptOp> {
        for (id, delta) in std::mem::take(&mut step.items) {
            match delta {
                Some(delta) => step.ops.push(TranscriptOp::AppendBlock { id, delta }),
                None => {
                    if let Some(slot) = self.items.get(&id) {
                        step.ops.push(TranscriptOp::Item {
                            item: Arc::clone(&slot.item),
                        });
                    }
                }
            }
        }
        for turn_id in &step.turns {
            let Some(turn) = self.turns.get(turn_id) else {
                continue;
            };
            let rows = self.turn_rows(turn_id, turn);
            let previous = self
                .turns
                .get_mut(turn_id)
                .map(|turn| std::mem::take(&mut turn.rows))
                .unwrap_or_default();
            let published = self.replace_rows(&mut step.ops, previous, rows);
            if let Some(turn) = self.turns.get_mut(turn_id) {
                turn.rows = published;
            }
        }
        if step.loose {
            let rows = self.loose_rows();
            let previous = std::mem::take(&mut self.loose_rows);
            self.loose_rows = self.replace_rows(&mut step.ops, previous, rows);
        }
        step.ops
    }

    fn replace_rows(
        &mut self,
        ops: &mut Vec<TranscriptOp>,
        previous: Vec<Arc<str>>,
        rows: Vec<TranscriptRow>,
    ) -> Vec<Arc<str>> {
        for id in previous {
            if !rows.iter().any(|row| row.id == id) && self.rows.remove(&id).is_some() {
                ops.push(TranscriptOp::RemoveRow { id });
            }
        }
        rows.into_iter()
            .map(|row| {
                let id = Arc::clone(&row.id);
                match self.rows.get_mut(&id) {
                    Some(current) if **current == row => {}
                    Some(current) if extends(current, &row) => {
                        let items = row
                            .items
                            .iter()
                            .skip(current.items.len())
                            .map(share)
                            .collect();
                        ops.push(TranscriptOp::ExtendRow {
                            id: Arc::clone(&id),
                            items,
                            kind: row.kind.clone(), // rules-allow: the row and the op each own its small kind
                        });
                        *current = Arc::new(row);
                    }
                    _ => {
                        let row = Arc::new(row);
                        ops.push(TranscriptOp::Row {
                            row: Arc::clone(&row),
                        });
                        self.rows.insert(Arc::clone(&id), row);
                    }
                }
                id
            })
            .collect()
    }

    fn slots<'a>(&'a self, ids: &'a [Arc<str>]) -> impl Iterator<Item = &'a Slot> {
        ids.iter().filter_map(|id| self.items.get(id))
    }

    /// The items a row lists directly.
    fn child_items<'a>(
        &'a self,
        children: &'a [TranscriptChild],
    ) -> impl Iterator<Item = &'a TranscriptItem> {
        children
            .iter()
            .filter_map(|child| Some(&*self.items.get(child.item()?)?.item))
    }

    /// One started turn's rows: the opening message, one `worked` row for its work, what
    /// stays in view, and its terminal answer carrying the turn's changes.
    fn turn_rows(&self, turn_id: &str, turn: &Turn) -> Vec<TranscriptRow> {
        let mut slots = self.slots(&turn.items).collect::<Vec<_>>();
        slots.sort_by_key(|slot| slot.position);
        let running = turn.ended_at_ms.is_none();
        let places = slots
            .iter()
            .map(|slot| place(&slot.item))
            .collect::<Vec<_>>();
        // While the turn runs, answers after its last work are still the tail.
        let last_work = places.iter().rposition(|place| *place == Place::Work);
        let last_answer = slots
            .iter()
            .rposition(|slot| slot.item.terminal && is_answer(&slot.item));
        let mut rows = Vec::<TranscriptRow>::new();
        let mut work = Vec::<&Slot>::new();
        for (index, (slot, place)) in slots.iter().zip(&places).enumerate() {
            let place = match place {
                Place::Answer if !running || last_work.is_some_and(|last| last > index) => {
                    Place::Work
                }
                Place::Answer => Place::Entry,
                place => *place,
            };
            match place {
                Place::Work => work.push(slot),
                Place::Notice if repeats_error(rows.last(), self, &slot.item) => {}
                Place::Notice => rows.push(single(slot, TranscriptRowKind::Notice)),
                Place::Entry | Place::Answer => {
                    let turn_changes = (last_answer == Some(index))
                        .then(|| turn_changes(&slots))
                        .flatten();
                    rows.push(single(slot, TranscriptRowKind::Entry { turn_changes }));
                }
            }
        }
        if let Some(first) = work.first() {
            let mut items = Vec::new();
            let mut runs = Vec::<TranscriptRow>::new();
            let mut in_run = false;
            for slot in &work {
                let id = Arc::clone(&slot.item.id);
                if !is_activity(&slot.item) {
                    items.push(TranscriptChild::Item(id));
                    in_run = false;
                } else if let Some(run) = runs.last_mut().filter(|_| in_run) {
                    run.items.push(TranscriptChild::Item(id));
                } else {
                    let run = activity_row(slot);
                    items.push(TranscriptChild::Row(Arc::clone(&run.id)));
                    runs.push(run);
                    in_run = true;
                }
            }
            for run in &mut runs {
                run.kind = TranscriptRowKind::Activity {
                    counts: counts(self.child_items(&run.items)),
                };
            }
            rows.extend(runs);
            rows.push(TranscriptRow {
                id: self.id(format_args!("worked:{turn_id}")),
                position: first.position,
                items,
                kind: TranscriptRowKind::Worked {
                    counts: counts(work.iter().map(|slot| &*slot.item)),
                    started_at_ms: turn.started_at_ms,
                    duration_ms: turn.ended_at_ms.map(|ended| {
                        u64::try_from(ended.saturating_sub(turn.started_at_ms)).unwrap_or(0)
                    }),
                },
            });
        }
        rows.sort_by(row_order);
        rows
    }

    /// Rows of items outside started turns: each on its own, with consecutive activity of
    /// one turn or block group joined.
    fn loose_rows(&self) -> Vec<TranscriptRow> {
        let mut rows = Vec::<TranscriptRow>::new();
        for slot in self.slots(&self.loose) {
            let item = &*slot.item;
            if is_notice(item) {
                if !repeats_error(rows.last(), self, item) {
                    rows.push(single(slot, TranscriptRowKind::Notice));
                }
            } else if is_activity(item) {
                let key = activity_key(item);
                if let Some(row) = rows.last_mut()
                    && let TranscriptRowKind::Activity { counts: row_counts } = &mut row.kind
                    && key.is_some()
                    && row
                        .items
                        .first()
                        .and_then(|first| self.items.get(first.item()?))
                        .is_some_and(|first| activity_key(&first.item) == key)
                {
                    row.items.push(TranscriptChild::Item(Arc::clone(&item.id)));
                    *row_counts = counts(self.child_items(&row.items));
                } else {
                    rows.push(activity_row(slot));
                }
            } else {
                rows.push(single(
                    slot,
                    TranscriptRowKind::Entry { turn_changes: None },
                ));
            }
        }
        rows
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Entry,
    Notice,
    /// A non-terminal answer: work once later work follows it or its turn ends.
    Answer,
    Work,
}

/// Where a started turn shows an item. The opening message, deliverables, errors and the
/// terminal answer or failure stay in view; the rest is the turn's work.
fn place(item: &TranscriptItem) -> Place {
    if item.starts_turn || stands_alone(item) {
        return Place::Entry;
    }
    if notice_tone(item) == Some(FrontendTone::Error) {
        return Place::Notice;
    }
    if item.terminal {
        return if is_notice(item) {
            Place::Notice
        } else {
            Place::Entry
        };
    }
    if is_answer(item) {
        Place::Answer
    } else {
        Place::Work
    }
}

/// `row` is `current` with items appended and a kind of the same variant.
fn extends(current: &TranscriptRow, row: &TranscriptRow) -> bool {
    current.position == row.position
        && std::mem::discriminant(&current.kind) == std::mem::discriminant(&row.kind)
        && row.items.starts_with(&current.items)
}

fn single(slot: &Slot, kind: TranscriptRowKind) -> TranscriptRow {
    TranscriptRow {
        id: Arc::clone(&slot.item.id),
        position: slot.position,
        items: vec![TranscriptChild::Item(Arc::clone(&slot.item.id))],
        kind,
    }
}

/// The same child, sharing its id.
fn share(child: &TranscriptChild) -> TranscriptChild {
    match child {
        TranscriptChild::Item(id) => TranscriptChild::Item(Arc::clone(id)),
        TranscriptChild::Row(id) => TranscriptChild::Row(Arc::clone(id)),
    }
}

/// A run of activity starting at `slot`, holding only it so far.
fn activity_row(slot: &Slot) -> TranscriptRow {
    TranscriptRow {
        id: format!("activity:{}", slot.item.id).into(),
        position: slot.position,
        items: vec![TranscriptChild::Item(Arc::clone(&slot.item.id))],
        kind: TranscriptRowKind::Activity {
            counts: counts(std::iter::once(&*slot.item)),
        },
    }
}

/// Position order, a `worked` row before the nested run sharing its position.
fn row_order(a: &TranscriptRow, b: &TranscriptRow) -> std::cmp::Ordering {
    let run = |row: &TranscriptRow| matches!(row.kind, TranscriptRowKind::Activity { .. });
    a.position
        .cmp(&b.position)
        .then_with(|| run(a).cmp(&run(b)))
}

/// `item` is a notice repeating the text of the error notice on the row before it, as a
/// failed turn's "Turn aborted" does.
fn repeats_error(
    previous: Option<&TranscriptRow>,
    reducer: &TranscriptReducer,
    item: &TranscriptItem,
) -> bool {
    let Some(text) = notice_text(item).filter(|_| notice_tone(item) != Some(FrontendTone::Error))
    else {
        return false;
    };
    previous
        .filter(|row| row.kind == TranscriptRowKind::Notice)
        .and_then(|row| reducer.items.get(row.items.first()?.item()?))
        .is_some_and(|error| {
            notice_tone(&error.item) == Some(FrontendTone::Error)
                && notice_text(&error.item).is_some_and(|error| !error.is_empty() && error == text)
        })
}

fn turn_changes(slots: &[&Slot]) -> Option<TurnChanges> {
    let mut diffs = Vec::new();
    let mut totals = DiffTotals::default();
    for slot in slots {
        if let TranscriptItemContent::Block { block, .. } = &slot.item.content
            && block.role == FrontendBlockRole::Tool
            && block.format == FrontendBlockFormat::UnifiedDiff
            && slot.item.state == TranscriptItemState::Complete
        {
            diffs.push(Arc::clone(&slot.item.id));
            for line in block
                .text
                .lines()
                .skip_while(|line| !line.starts_with("@@ "))
            {
                if line.starts_with('+') {
                    totals.additions = totals.additions.saturating_add(1);
                } else if line.starts_with('-') {
                    totals.deletions = totals.deletions.saturating_add(1);
                }
            }
        }
    }
    (!diffs.is_empty()).then_some(TurnChanges { diffs, totals })
}

fn counts<'a>(items: impl Iterator<Item = &'a TranscriptItem>) -> ActivityCounts {
    let mut counts = ActivityCounts::default();
    for item in items {
        let errored = match &item.content {
            TranscriptItemContent::Block { block, .. } => block.tone == FrontendTone::Error,
            TranscriptItemContent::Notice { tone, .. } => *tone == FrontendTone::Error,
            _ => false,
        };
        let count = match &item.content {
            _ if errored => &mut counts.errors,
            TranscriptItemContent::Assistant {
                phase: ModelStepContentPhase::Reasoning,
                ..
            } => &mut counts.thoughts,
            TranscriptItemContent::Block { block, .. } => match block.role {
                FrontendBlockRole::Tool => &mut counts.tool_calls,
                FrontendBlockRole::WebSearch => &mut counts.web_searches,
                FrontendBlockRole::Approval => &mut counts.approvals,
                FrontendBlockRole::Artifact => &mut counts.artifacts,
                FrontendBlockRole::Activity | FrontendBlockRole::Notice => &mut counts.events,
            },
            TranscriptItemContent::Message { .. }
            | TranscriptItemContent::Assistant { .. }
            | TranscriptItemContent::Notice { .. }
            | TranscriptItemContent::ForkPoint { .. } => &mut counts.events,
        };
        *count = count.saturating_add(1);
    }
    counts
}

/// The title a later block of a group shows from the group's first titled block.
fn group_title(
    groups: &mut HashMap<Arc<str>, (Arc<str>, Arc<str>)>,
    id: &Arc<str>,
    block: &FrontendBlock,
) -> Option<Arc<str>> {
    let group = block.group.as_deref()?;
    if let Some((first, title)) = groups.get(group) {
        return (first != id).then(|| Arc::clone(title));
    }
    if !block.title.is_empty() {
        groups.insert(group.into(), (Arc::clone(id), block.title.as_str().into()));
    }
    None
}

fn item_state(block: &FrontendBlock) -> TranscriptItemState {
    if block.state == FrontendBlockState::Pending {
        TranscriptItemState::Pending
    } else {
        TranscriptItemState::Complete
    }
}

/// Merges an Append block into the block it extends and returns what changed: text by the
/// block update rule, links deduplicated, files upserted by id, observations and interface
/// blocks appended, an empty title or a missing group keeping the previous one, and every
/// other field taken from the append. The flag is set when the change needs the whole
/// item, because the symbol was cleared.
fn merge(merged: &mut FrontendBlock, block: FrontendBlock) -> (BlockDelta, bool) {
    let FrontendBlock {
        id: _,
        group,
        update,
        state,
        role,
        title,
        mut text,
        symbol,
        links,
        files,
        content,
        format,
        tone,
        ui,
    } = block;
    let mut delta = BlockDelta::default();
    let before = merged.text.len();
    update.apply(&mut merged.text, &text);
    if merged.text.len() > before.saturating_add(text.len()) {
        text.insert(0, '\n');
    }
    delta.text = text;
    for link in links {
        if !merged.links.contains(&link) {
            merged.links.push(link.clone()); // rules-allow: the merged block and the delta each own the new link
            delta.links.push(link);
        }
    }
    for file in files {
        match merged
            .files
            .iter_mut()
            .find(|current| current.id == file.id)
        {
            Some(current) if *current == file => continue,
            Some(current) => current.clone_from(&file), // rules-allow: the merged block and the delta each own the changed file
            None => merged.files.push(file.clone()), // rules-allow: the merged block and the delta each own the new file
        }
        delta.files.push(file);
    }
    merged.content.0.extend(content.0.iter().cloned()); // rules-allow: the merged block and the delta each own the new observations
    delta.content = content.0;
    merged.ui.extend(ui.iter().cloned()); // rules-allow: the merged block and the delta each own the new interface blocks
    delta.ui = ui;
    if !title.is_empty() && title != merged.title {
        merged.title.clone_from(&title); // rules-allow: the merged block and the delta each own the new title
        delta.title = Some(title);
    }
    if let Some(group) = group
        && merged.group.as_ref() != Some(&group)
    {
        merged.group = Some(group.clone()); // rules-allow: the merged block and the delta each own the new group
        delta.group = Some(group);
    }
    delta.state = change(&mut merged.state, state);
    delta.role = change(&mut merged.role, role);
    delta.tone = change(&mut merged.tone, tone);
    delta.format = change(&mut merged.format, format);
    let cleared = symbol.is_none() && merged.symbol.is_some();
    if symbol != merged.symbol {
        merged.symbol.clone_from(&symbol); // rules-allow: the merged block and the delta each own the new symbol
        delta.symbol = symbol;
    }
    (delta, cleared)
}

/// Sets `field` to `value`, returning `value` when that changed it.
fn change<T: Copy + PartialEq>(field: &mut T, value: T) -> Option<T> {
    (*field != value).then(|| {
        *field = value;
        value
    })
}

fn from_session(message: &MessageEvent) -> bool {
    matches!(
        message.author,
        MessageAuthor::Source {
            source: MessageSource::Session { .. },
            ..
        }
    )
}

fn is_answer(item: &TranscriptItem) -> bool {
    matches!(
        item.content,
        TranscriptItemContent::Assistant {
            phase: ModelStepContentPhase::FinalAnswer,
            ..
        }
    )
}

/// Images and interface documents are deliverables: never folded into work.
fn stands_alone(item: &TranscriptItem) -> bool {
    matches!(&item.content, TranscriptItemContent::Block { block, .. }
        if matches!(block.format, FrontendBlockFormat::Image | FrontendBlockFormat::Ui))
}

fn is_notice(item: &TranscriptItem) -> bool {
    notice_tone(item).is_some()
}

fn notice_tone(item: &TranscriptItem) -> Option<FrontendTone> {
    match &item.content {
        TranscriptItemContent::Notice { tone, .. } => Some(*tone),
        TranscriptItemContent::Block { block, .. }
            if block.role == FrontendBlockRole::Notice && !stands_alone(item) =>
        {
            Some(block.tone)
        }
        _ => None,
    }
}

fn notice_text(item: &TranscriptItem) -> Option<&str> {
    match &item.content {
        TranscriptItemContent::Notice { text, .. } => Some(text),
        TranscriptItemContent::Block { block, .. } => Some(&block.text),
        _ => None,
    }
}

fn is_activity(item: &TranscriptItem) -> bool {
    !item.terminal
        && match &item.content {
            TranscriptItemContent::Assistant { phase, .. } => {
                *phase == ModelStepContentPhase::Reasoning
            }
            TranscriptItemContent::Block { .. } => !stands_alone(item),
            _ => false,
        }
}

fn activity_key(item: &TranscriptItem) -> Option<&str> {
    item.turn_id.as_deref().or(match &item.content {
        TranscriptItemContent::Block { block, .. } => block.group.as_deref(),
        _ => None,
    })
}

const fn phase_name(phase: ModelStepContentPhase) -> &'static str {
    match phase {
        ModelStepContentPhase::Reasoning => "reasoning",
        ModelStepContentPhase::Commentary => "commentary",
        ModelStepContentPhase::FinalAnswer => "final_answer",
    }
}

const fn phase_index(phase: ModelStepContentPhase) -> u32 {
    match phase {
        ModelStepContentPhase::Reasoning => 0,
        ModelStepContentPhase::Commentary => 1,
        ModelStepContentPhase::FinalAnswer => 2,
    }
}

#[cfg(test)]
mod tests;
