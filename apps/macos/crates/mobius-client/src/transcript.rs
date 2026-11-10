//! The gateway-reduced transcript as this window holds it: items and rows by id, rows in
//! position order. Ops and pages apply mechanically; the gateway owns every grouping rule.

use std::any::Any;
use std::cell::OnceCell;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use mobius::protocol::{FrontendBlock, MessageAuthor, MessageSource, MessageTarget};
use mobius_gateway::wire::{
    BlockDelta, TranscriptChild, TranscriptItem, TranscriptItemContent, TranscriptItemState,
    TranscriptOp, TranscriptPage, TranscriptPosition, TranscriptRow,
};

/// One item and what the interface derived from it, built on first use.
pub struct TranscriptEntry {
    item: Arc<TranscriptItem>,
    derived: OnceCell<Box<dyn Any>>,
    generation: u64,
}

impl std::fmt::Debug for TranscriptEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptEntry")
            .field("item", &self.item)
            .finish_non_exhaustive()
    }
}

impl std::ops::Deref for TranscriptEntry {
    type Target = TranscriptItem;

    fn deref(&self) -> &TranscriptItem {
        &self.item
    }
}

impl TranscriptEntry {
    fn new(item: Arc<TranscriptItem>) -> Self {
        Self {
            item,
            derived: OnceCell::new(),
            generation: next_revision(),
        }
    }

    /// Stays the same while the item only grows; replacing the item changes it. Within one
    /// generation, the text's length tells whether it changed.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn edit(&mut self) -> &mut TranscriptItem {
        self.derived = OnceCell::new();
        Arc::make_mut(&mut self.item)
    }

    /// The capability-rendered block this item shows.
    pub fn block(&self) -> Option<&FrontendBlock> {
        match &self.content {
            TranscriptItemContent::Block { block, .. } => Some(block),
            _ => None,
        }
    }

    /// The title of the block this one is about, such as a polled command.
    pub fn group_title(&self) -> Option<&str> {
        match &self.content {
            TranscriptItemContent::Block { group_title, .. } => group_title.as_deref(),
            _ => None,
        }
    }

    /// What the interface builds from this version of the item, such as a parsed diff,
    /// kept until the item changes. Each entry holds one kind, so `None` only means `T`
    /// differs from the kind built first.
    pub fn derived<T: 'static>(&self, build: impl FnOnce(&Self) -> T) -> Option<&T> {
        self.derived
            .get_or_init(|| Box::new(build(self)))
            .downcast_ref()
    }

    /// The handle and chat of a message another chat sent here.
    pub fn session_sender(&self) -> Option<(&str, &str)> {
        match &self.content {
            TranscriptItemContent::Message { message } => match &message.author {
                MessageAuthor::Source {
                    source: MessageSource::Session { session_id },
                    handle,
                    ..
                } => Some((handle, session_id)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Still being produced: streaming text or a pending block.
    pub fn is_pending(&self) -> bool {
        self.state != TranscriptItemState::Complete
    }

    /// The durable message this entry is, when it can be replied to or acted on.
    pub fn message_target(&self) -> Option<&MessageTarget> {
        match &self.content {
            TranscriptItemContent::Message { message } => message.message_target.as_ref(),
            _ => self.target.as_ref(),
        }
    }

    fn append_text(&mut self, delta: &str) {
        match &mut self.edit().content {
            TranscriptItemContent::Message { message } => message.text.push_str(delta),
            TranscriptItemContent::Assistant { text, .. } => text.push_str(delta),
            _ => {}
        }
    }

    fn append_block(&mut self, delta: BlockDelta) {
        let BlockDelta {
            text,
            links,
            files,
            content,
            ui,
            title,
            state,
            role,
            tone,
            format,
            symbol,
            group,
            group_title,
            item_state,
        } = delta;
        let item = self.edit();
        if let Some(item_state) = item_state {
            item.state = item_state;
        }
        let TranscriptItemContent::Block {
            block,
            group_title: shown,
            ..
        } = &mut item.content
        else {
            return;
        };
        block.text.push_str(&text);
        block.links.extend(links);
        for file in files {
            match block.files.iter_mut().find(|current| current.id == file.id) {
                Some(current) => *current = file,
                None => block.files.push(file),
            }
        }
        block.content.0.extend(content);
        block.ui.extend(ui);
        if let Some(title) = title {
            block.title = title;
        }
        if let Some(state) = state {
            block.state = state;
        }
        if let Some(role) = role {
            block.role = role;
        }
        if let Some(tone) = tone {
            block.tone = tone;
        }
        if let Some(format) = format {
            block.format = format;
        }
        if symbol.is_some() {
            block.symbol = symbol;
        }
        if group.is_some() {
            block.group = group;
        }
        if group_title.is_some() {
            *shown = group_title;
        }
    }
}

/// A number no earlier change in any store used: a replacement must invalidate caches even
/// when it looks the same.
fn next_revision() -> u64 {
    static REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// One child of a row, resolved: an item, or a nested row drawn in place.
#[derive(Debug)]
pub enum RowChild<'a> {
    Item(&'a TranscriptEntry),
    Row(&'a Arc<TranscriptRow>),
}

/// Items and rows by id; the rows no row lists as a child sort by position, and every row
/// shows its children in listed order.
#[derive(Debug, Default)]
pub struct TranscriptStore {
    items: HashMap<Arc<str>, TranscriptEntry>,
    rows: HashMap<Arc<str>, Arc<TranscriptRow>>,
    order: BTreeSet<(TranscriptPosition, Arc<str>)>,
    /// How many held rows list each row as a child.
    nested: HashMap<Arc<str>, usize>,
    revision: u64,
    structure_revision: u64,
}

impl TranscriptStore {
    /// Moves with every change, appended text included.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Moves when items or rows arrive, are replaced or leave, not when an item only grows,
    /// so what follows the rows need not rebuild for every streamed chunk.
    pub fn structure_revision(&self) -> u64 {
        self.structure_revision
    }

    fn changed(&mut self, structure: bool) {
        self.revision = next_revision();
        if structure {
            self.structure_revision = self.revision;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rows().next().is_none()
    }

    fn count_children(&mut self, children: &[TranscriptChild], added: bool) {
        for child in children {
            let TranscriptChild::Row(id) = child else {
                continue;
            };
            if added {
                *self.nested.entry(Arc::clone(id)).or_default() += 1;
            } else if let Some(count) = self.nested.get_mut(id) {
                *count -= 1;
                if *count == 0 {
                    self.nested.remove(id);
                }
            }
        }
    }

    /// Applies one event's ops in order and returns the ids of the items they changed.
    pub fn apply(&mut self, ops: Vec<TranscriptOp>) -> Vec<Arc<str>> {
        let mut touched = Vec::new();
        if ops.is_empty() {
            return touched;
        }
        let structure = ops.iter().any(|op| {
            !matches!(
                op,
                TranscriptOp::AppendText { .. } | TranscriptOp::AppendBlock { .. }
            )
        });
        for op in ops {
            match op {
                TranscriptOp::Item { item } => {
                    touched.push(Arc::clone(&item.id));
                    self.items
                        .insert(Arc::clone(&item.id), TranscriptEntry::new(item));
                }
                TranscriptOp::AppendText { id, text } => {
                    if let Some(entry) = self.items.get_mut(&id) {
                        entry.append_text(&text);
                        touched.push(id);
                    }
                }
                TranscriptOp::AppendBlock { id, delta } => {
                    if let Some(entry) = self.items.get_mut(&id) {
                        entry.append_block(delta);
                        touched.push(id);
                    }
                }
                TranscriptOp::Row { row } => self.insert_row(row),
                TranscriptOp::ExtendRow { id, items, kind } => {
                    if self.rows.contains_key(&id) {
                        self.count_children(&items, true);
                    }
                    if let Some(row) = self.rows.get_mut(&id) {
                        let row = Arc::make_mut(row);
                        row.items.extend(items);
                        row.kind = kind;
                    }
                }
                TranscriptOp::RemoveRow { id } => {
                    if let Some(row) = self.rows.remove(&id) {
                        self.count_children(&row.items, false);
                        self.order.remove(&(row.position, id));
                    }
                }
            }
        }
        self.changed(structure);
        touched
    }

    /// Shows `page` in place of everything held.
    pub fn replace(&mut self, page: TranscriptPage) {
        self.items.clear();
        self.rows.clear();
        self.order.clear();
        self.nested.clear();
        self.merge(page);
    }

    /// Adds an older page; an id already held stays as it is.
    pub fn prepend(&mut self, page: TranscriptPage) {
        for item in page.items {
            self.items
                .entry(Arc::clone(&item.id))
                .or_insert_with(|| TranscriptEntry::new(item));
        }
        for row in page.rows {
            if !self.rows.contains_key(&row.id) {
                self.insert_row(row);
            }
        }
        self.changed(true);
    }

    /// Lays a newer page over what is held, keeping ids it does not name.
    pub fn merge(&mut self, page: TranscriptPage) {
        for item in page.items {
            self.items
                .insert(Arc::clone(&item.id), TranscriptEntry::new(item));
        }
        for row in page.rows {
            self.insert_row(row);
        }
        self.changed(true);
    }

    /// Everything shown, as a page sharing this store's items and rows.
    pub fn page(&self) -> TranscriptPage {
        TranscriptPage {
            items: self
                .entries()
                .map(|entry| Arc::clone(&entry.item))
                .collect(),
            rows: self
                .order
                .iter()
                .filter_map(|(_, id)| self.rows.get(id))
                .map(Arc::clone)
                .collect(),
        }
    }

    fn insert_row(&mut self, row: Arc<TranscriptRow>) {
        let id = Arc::clone(&row.id);
        self.count_children(&row.items, true);
        if let Some(previous) = self.rows.insert(Arc::clone(&id), row) {
            self.count_children(&previous.items, false);
            if self
                .rows
                .get(&id)
                .is_some_and(|current| previous.position != current.position)
            {
                self.order.remove(&(previous.position, Arc::clone(&id)));
            }
        }
        if let Some(position) = self.rows.get(&id).map(|row| row.position) {
            self.order.insert((position, id));
        }
    }

    /// The top-level rows, the ones no row lists as a child, in display order.
    pub fn rows(&self) -> impl DoubleEndedIterator<Item = &Arc<TranscriptRow>> {
        self.order
            .iter()
            .filter(|(_, id)| !self.nested.contains_key(id))
            .filter_map(|(_, id)| self.rows.get(id))
    }

    pub fn row(&self, id: &str) -> Option<&Arc<TranscriptRow>> {
        self.rows.get(id)
    }

    pub fn item(&self, id: &str) -> Option<&TranscriptEntry> {
        self.items.get(id)
    }

    /// The children `row` shows, in its order; a nested row not held is left out.
    pub fn row_children<'a>(
        &'a self,
        row: &'a TranscriptRow,
    ) -> impl DoubleEndedIterator<Item = RowChild<'a>> {
        row.items.iter().filter_map(|child| match child {
            TranscriptChild::Item(id) => self.items.get(id).map(RowChild::Item),
            TranscriptChild::Row(id) => self.rows.get(id).map(RowChild::Row),
        })
    }

    /// The items `row` shows, nested rows' items in their place.
    pub fn row_items<'a>(
        &'a self,
        row: &'a TranscriptRow,
    ) -> impl DoubleEndedIterator<Item = &'a TranscriptEntry> {
        row.items
            .iter()
            .flat_map(|child| match child {
                TranscriptChild::Item(_) => std::slice::from_ref(child),
                TranscriptChild::Row(id) => self.rows.get(id).map_or(&[][..], |row| &row.items),
            })
            .filter_map(|child| self.items.get(child.item()?))
    }

    /// Every shown item: rows in order, each row's items in its order.
    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &TranscriptEntry> {
        self.rows().flat_map(|row| self.row_items(row))
    }

    /// The unified diffs a turn's changes name, in order.
    pub fn diffs<'a>(&'a self, ids: &'a [Arc<str>]) -> impl Iterator<Item = &'a TranscriptEntry> {
        ids.iter().filter_map(|id| self.items.get(id))
    }
}

impl From<TranscriptPage> for TranscriptStore {
    fn from(page: TranscriptPage) -> Self {
        let mut store = Self::default();
        store.merge(page);
        store
    }
}
