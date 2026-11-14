//! Minimal state-driven möbius terminal frontend.

mod attention;
mod clipboard;
mod diff;
mod events;
mod highlight;
mod input;
mod markdown;
mod references;
pub(super) mod runtime;
mod shimmer;
mod view;

use std::sync::Arc;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use ratatui::text::Line;

use self::events::UsageStatus;
#[cfg(test)]
use self::input::UiAction;
use self::view::{bounded_terminal_text, initial_widgets};
use super::catalog::{GatewayAction, MenuItem, UiCatalog};
use super::dashboard::CapabilityOverlay;
use super::terminal::{terminal_text, truncate_bytes};
#[cfg(test)]
use mobius::protocol::EventMsg;
use mobius::protocol::{
    ActiveMessageDelivery, ExecApprovalRequestEvent, FrontendBlock, FrontendBlockFormat,
    FrontendBlockRole, FrontendBlockState, FrontendBlockUpdate, FrontendPickerOption, FrontendTone,
    FrontendWidget, ModelInfo, Op, RenderedBlock, SessionFileReference,
    SessionResumeRequestedEvent, ToolContent,
};
use mobius_gateway::wire::BlockDelta;

const MAX_ENTRY_BYTES: usize = 40_000;
const MAX_COMPOSER_HISTORY_ENTRIES: usize = 100;
const MAX_TOOL_DETAIL_BYTES: usize = 512;
const MAX_TOOL_DETAIL_LINES: usize = 3;
const MAX_TITLE_BYTES: usize = 160;
const MAX_TRANSCRIPT_ENTRIES: usize = 512;

fn attachment_label(file: &SessionFileReference) -> String {
    format!("[file] {} · {} bytes", file.name, file.size)
}

#[derive(Clone, Copy)]
enum TranscriptTone {
    Welcome,
    Assistant,
    Reasoning,
    User,
    Neutral,
    Success,
    Warning,
    Error,
}

impl From<FrontendTone> for TranscriptTone {
    fn from(value: FrontendTone) -> Self {
        match value {
            FrontendTone::Neutral => Self::Neutral,
            FrontendTone::Success => Self::Success,
            FrontendTone::Warning => Self::Warning,
            FrontendTone::Error => Self::Error,
        }
    }
}

struct TranscriptEntry {
    id: Option<BlockKey>,
    group: Option<BlockKey>,
    title: Option<String>,
    role: Option<FrontendBlockRole>,
    detail: Option<String>,
    text: String,
    format: FrontendBlockFormat,
    tone: TranscriptTone,
    pending: bool,
    rendered: Option<(u16, RenderedTranscript)>,
    /// Incremental Markdown state while gateway text deltas extend this entry.
    stream: Option<Box<markdown::StreamCache>>,
}

struct RenderedTranscript {
    lines: Vec<Line<'static>>,
    wrapped_heights: Vec<usize>,
    /// The sum of `wrapped_heights`: the rows this entry fills.
    height: usize,
    /// A running tool whose first line leads with a marker the frame replaces with a spinner.
    spinner: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct BlockKey {
    capability: Arc<str>,
    value: String,
}

#[derive(Clone)]
enum PickerAction {
    Submit(Box<Op>),
    Resume(mobius::SessionId),
    Gateway(GatewayAction),
    Download(SessionFileReference),
    CreateSession {
        workspace: PathBuf,
        bot_id: Arc<str>,
        clear: bool,
    },
    ConfirmDeleteFile(SessionFileReference),
}

struct PickerOption {
    label: String,
    description: String,
    detail: String,
    shows_detail: bool,
    action: PickerAction,
    secondary: Option<PickerAction>,
}

impl From<FrontendPickerOption> for PickerOption {
    fn from(option: FrontendPickerOption) -> Self {
        Self {
            label: option.label,
            description: option.description,
            detail: option.detail,
            shows_detail: false,
            action: PickerAction::Submit(Box::new(option.op)),
            secondary: None,
        }
    }
}

struct PickerState {
    title: String,
    options: Vec<PickerOption>,
    selected: usize,
    query: String,
    /// Indices of the options matching `query`, recomputed only when the query changes.
    matches: Vec<usize>,
}

impl PickerState {
    fn new(title: String, options: Vec<PickerOption>, selected: usize) -> Self {
        let matches = (0..options.len()).collect();
        Self {
            title,
            options,
            selected,
            query: String::new(),
            matches,
        }
    }

    /// Replaces the filter text and selects the first match.
    fn edit_query(&mut self, edit: impl FnOnce(&mut String)) {
        edit(&mut self.query);
        self.selected = 0;
        let query = self.query.to_lowercase();
        self.matches = self
            .options
            .iter()
            .enumerate()
            .filter(|(_, option)| {
                query.is_empty()
                    || option.label.to_lowercase().contains(&query)
                    || option.description.to_lowercase().contains(&query)
                    || option.detail.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
    }

    fn matching_options(&self) -> impl Iterator<Item = &PickerOption> {
        self.matches
            .iter()
            .filter_map(|index| self.options.get(*index))
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }

    fn selected_option(&self) -> Option<&PickerOption> {
        self.options.get(self.selected_index()?)
    }
}

struct Viewport {
    scroll: usize,
    view_height: usize,
    content_height: usize,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            scroll: usize::MAX,
            view_height: 0,
            content_height: 0,
        }
    }
}

impl Viewport {
    fn scroll_up(&mut self, rows: usize) {
        if self.max_scroll() > 0 {
            self.scroll = self.effective_scroll().saturating_sub(rows);
        }
    }

    fn scroll_down(&mut self, rows: usize) {
        let max_scroll = self.max_scroll();
        let next = self.effective_scroll().saturating_add(rows);
        self.scroll = if next >= max_scroll { usize::MAX } else { next };
    }

    fn page_height(&self) -> usize {
        self.view_height.max(1)
    }

    fn effective_scroll(&self) -> usize {
        self.scroll.min(self.max_scroll())
    }

    fn max_scroll(&self) -> usize {
        self.content_height.saturating_sub(self.view_height)
    }

    fn update(&mut self, content_height: usize, view_height: usize) {
        self.content_height = content_height;
        self.view_height = view_height;
        if self.max_scroll() == 0 {
            self.scroll = usize::MAX;
        } else if self.scroll != usize::MAX {
            self.scroll = self.scroll.min(self.max_scroll());
        }
    }

    fn top(&mut self) {
        self.scroll = 0;
    }

    fn bottom(&mut self) {
        self.scroll = usize::MAX;
    }
}

struct PreviewState {
    title: String,
    subtitle: String,
    content: PreviewContent,
    viewport: Viewport,
}

impl PreviewState {
    fn new(title: &str, content: PreviewContent) -> Self {
        Self {
            title: bounded_title(title),
            subtitle: String::new(),
            content,
            viewport: Viewport::default(),
        }
    }

    fn snapshot(
        id: Arc<str>,
        title: &str,
        subtitle: &str,
        page_id: Arc<str>,
        transcript: VecDeque<TranscriptEntry>,
        next: Option<Op>,
    ) -> Self {
        Self {
            title: bounded_title(title),
            subtitle: bounded_title(subtitle),
            content: PreviewContent::Snapshot(Box::new(SnapshotPreview::new(
                id, page_id, transcript, next,
            ))),
            viewport: Viewport::default(),
        }
    }
}

enum PreviewContent {
    Diff(Box<diff::DiffBrowser>),
    LiveTranscript,
    Snapshot(Box<SnapshotPreview>),
}

struct PendingFileDownload {
    request_id: Arc<str>,
    session_id: mobius::SessionId,
    file: SessionFileReference,
    destination: PathBuf,
    output: tokio::fs::File,
    preview: Vec<u8>,
    preview_truncated: bool,
}

struct SnapshotPreview {
    id: Arc<str>,
    page_ids: BTreeSet<Arc<str>>,
    transcript: VecDeque<TranscriptEntry>,
    next: Option<Op>,
}

impl SnapshotPreview {
    fn new(
        id: Arc<str>,
        page_id: Arc<str>,
        transcript: VecDeque<TranscriptEntry>,
        next: Option<Op>,
    ) -> Self {
        Self {
            id,
            page_ids: BTreeSet::from([page_id]),
            transcript,
            next,
        }
    }

    fn prepend(
        &mut self,
        page_id: Arc<str>,
        mut transcript: VecDeque<TranscriptEntry>,
        next: Option<Op>,
    ) {
        if !self.page_ids.insert(page_id) {
            return;
        }
        transcript.append(&mut self.transcript);
        self.transcript = transcript;
        self.next = next;
    }
}

/// The composer's editable content, also kept as a draft while the composer holds something else.
#[derive(Default, Clone)]
struct InputDraft {
    text: String,
    cursor: usize,
    pastes: BTreeMap<char, String>,
    attachments: Vec<SessionFileReference>,
}

#[derive(Default)]
struct TuiState {
    widgets: Vec<(Arc<str>, Arc<FrontendWidget>)>,
    transcript: VecDeque<TranscriptEntry>,
    transcript_viewport: Viewport,
    reasoning_title: Option<String>,
    git_diff: [Option<String>; 2],
    git_diff_requests: BTreeMap<Arc<str>, usize>,
    git_diff_refresh: bool,
    composer: InputDraft,
    input_limit_reached: bool,
    upload_in_progress: bool,
    composer_history: VecDeque<String>,
    composer_history_index: Option<usize>,
    composer_history_draft: Option<InputDraft>,
    pending_message_drafts: BTreeMap<Arc<str>, InputDraft>,
    failed_message_drafts: VecDeque<InputDraft>,
    approvals: VecDeque<ExecApprovalRequestEvent>,
    approval_draft: Option<InputDraft>,
    /// The highlighted choice on the front approval card.
    approval_selection: usize,
    active_turns: Vec<Arc<str>>,
    active_message_delivery: Option<ActiveMessageDelivery>,
    turn_started_at: Option<Instant>,
    usage: UsageStatus,
    context_limit: Option<i64>,
    cwd: String,
    model: ModelInfo,
    model_route: Arc<str>,
    agent_summary: String,
    disconnected: bool,
    slash_selection: usize,
    slash_menu_dismissed: bool,
    reference_selection: usize,
    reference_menu_dismissed: bool,
    reference_cache: Option<(char, String, Vec<MenuItem>)>,
    picker: Option<PickerState>,
    preview: Option<PreviewState>,
    preview_request_id: Option<Arc<str>>,
    capability_overlay: Option<CapabilityOverlay>,
    requested_resume: Option<SessionResumeRequestedEvent>,
    background_approval_count: usize,
    history_cursor: Option<mobius_gateway::wire::HistoryCursor>,
    history_request_id: Option<Arc<str>>,
    deletion_request_id: Option<Arc<str>>,
    file_download: Option<PendingFileDownload>,
    /// The entry key of each block item still shown, for the gateway's block appends.
    block_items: HashMap<Arc<str>, BlockKey>,
}

impl TuiState {
    fn new(
        catalog: &UiCatalog,
        cwd: Option<std::path::PathBuf>,
        mut model: ModelInfo,
        model_route: Arc<str>,
        agent_summary: String,
    ) -> Self {
        model.model = terminal_text(&model.model).into();
        model.reasoning_effort = model
            .reasoning_effort
            .map(|effort| terminal_text(&effort).into());
        Self {
            widgets: initial_widgets(catalog),
            active_message_delivery: Some(ActiveMessageDelivery::Steer),
            cwd: cwd.map_or_else(
                || "Bot conversation".into(),
                |cwd| terminal_text(&cwd.display().to_string()),
            ),
            model,
            model_route,
            agent_summary,
            ..Self::default()
        }
    }

    fn is_working(&self) -> bool {
        self.active_turn().is_some() && self.approval().is_none() && !self.disconnected
    }

    fn message_delivery(&self) -> ActiveMessageDelivery {
        self.active_message_delivery
            .unwrap_or(ActiveMessageDelivery::Steer)
    }

    fn alternate_message_delivery(&self) -> ActiveMessageDelivery {
        match self.message_delivery() {
            ActiveMessageDelivery::Steer => ActiveMessageDelivery::Queue,
            ActiveMessageDelivery::Queue => ActiveMessageDelivery::Steer,
        }
    }

    fn active_turn(&self) -> Option<&Arc<str>> {
        self.approval()
            .map(|request| &request.turn_id)
            .or_else(|| self.active_turns.first())
    }

    fn composer_target_turn(&self) -> Option<&str> {
        self.active_message_delivery
            .and_then(|_| self.active_turn())
            .map(|turn_id| &**turn_id)
    }

    fn start_turn(&mut self, turn_id: Arc<str>) {
        if !self.active_turns.contains(&turn_id) {
            self.active_turns.push(turn_id);
        }
        self.turn_started_at.get_or_insert_with(Instant::now);
    }

    fn approval(&self) -> Option<&ExecApprovalRequestEvent> {
        self.approvals.front()
    }

    fn begin_approval(&mut self, request: ExecApprovalRequestEvent) {
        self.start_turn(Arc::clone(&request.turn_id));
        if self
            .approvals
            .iter()
            .any(|pending| pending.id == request.id)
        {
            return;
        }
        if self.approvals.is_empty() {
            self.approval_draft = Some(self.take_input_draft());
            self.approval_selection = 0;
        }
        self.approvals.push_back(request);
        self.sync_attention_approvals();
    }

    fn remove_approval(&mut self, id: &str) {
        if self.approval().is_some_and(|approval| &*approval.id == id) {
            self.take_approval();
        } else {
            self.approvals.retain(|approval| &*approval.id != id);
            self.sync_attention_approvals();
        }
    }

    fn take_approval(&mut self) -> Option<Arc<str>> {
        let request = self.approvals.pop_front()?;
        self.approval_selection = 0;
        self.sync_attention_approvals();
        self.composer.text.clear();
        self.composer.pastes.clear();
        self.composer.cursor = 0;
        self.input_limit_reached = false;
        self.slash_menu_dismissed = false;
        self.reference_menu_dismissed = false;
        self.restore_draft();
        Some(request.id)
    }

    fn restore_draft(&mut self) {
        if self.approvals.is_empty()
            && let Some(draft) = self.approval_draft.take()
        {
            self.restore_input_draft(draft);
        }
    }

    fn take_input_draft(&mut self) -> InputDraft {
        std::mem::take(&mut self.composer)
    }

    fn snapshot_input_draft(&self) -> InputDraft {
        self.composer.clone() // rules-allow: the snapshot restores the composer after a failed send
    }

    fn restore_failed_message(&mut self, draft: InputDraft) {
        if self.stash_message_draft(draft) {
            self.push(
                "message not sent · Ctrl-R to recover it",
                TranscriptTone::Warning,
            );
        }
    }

    fn recover_unconfirmed_messages(&mut self) {
        let pending = std::mem::take(&mut self.pending_message_drafts);
        if pending.is_empty() {
            return;
        }
        let mut queued = false;
        for (_, draft) in pending {
            queued |= self.stash_message_draft(draft);
        }
        self.push(
            if queued {
                "message delivery unconfirmed · check chat before resending · Ctrl-R recovers saved drafts"
            } else {
                "message delivery unconfirmed · check chat before resending"
            },
            TranscriptTone::Warning,
        );
    }

    fn stash_message_draft(&mut self, draft: InputDraft) -> bool {
        if self.composer.text.is_empty()
            && self.composer.attachments.is_empty()
            && self.approval().is_none()
        {
            self.replace_input_draft(draft);
            false
        } else {
            self.failed_message_drafts.push_back(draft);
            true
        }
    }

    fn restore_input_draft(&mut self, mut draft: InputDraft) {
        for attachment in std::mem::take(&mut self.composer.attachments) {
            if !draft
                .attachments
                .iter()
                .any(|existing| existing.id == attachment.id)
            {
                draft.attachments.push(attachment);
            }
        }
        self.set_composer(draft);
    }

    fn replace_input_draft(&mut self, draft: InputDraft) {
        self.set_composer(draft);
        self.input_limit_reached = false;
        self.slash_menu_dismissed = false;
        self.reference_menu_dismissed = false;
    }

    fn set_composer(&mut self, mut draft: InputDraft) {
        draft.cursor = draft.cursor.min(draft.text.len());
        self.composer = draft;
    }

    fn remember_composer_input(&mut self, input: String) {
        if input.is_empty() {
            return;
        }
        if self.composer_history.len() >= MAX_COMPOSER_HISTORY_ENTRIES {
            self.composer_history.pop_front();
        }
        self.composer_history.push_back(input);
        self.composer_history_index = None;
        self.composer_history_draft = None;
    }

    fn composer_history_up(&mut self) {
        if self.composer_history.is_empty() {
            return;
        }
        let index = if let Some(index) = self.composer_history_index {
            index.saturating_sub(1)
        } else {
            let draft = self.take_input_draft();
            self.composer.attachments.clone_from(&draft.attachments);
            self.composer_history_draft = Some(draft);
            self.composer_history.len() - 1
        };
        self.recall_composer_input(index);
    }

    fn recover_saved_message(&mut self) {
        let Some(saved) = self.failed_message_drafts.pop_front() else {
            return;
        };
        if !self.composer.text.is_empty() || !self.composer.attachments.is_empty() {
            let current = self.take_input_draft();
            self.failed_message_drafts.push_back(current);
        }
        self.replace_input_draft(saved);
        self.composer_history_index = None;
        self.composer_history_draft = None;
    }

    fn composer_history_down(&mut self) {
        let Some(index) = self.composer_history_index else {
            return;
        };
        if index + 1 < self.composer_history.len() {
            self.recall_composer_input(index + 1);
        } else {
            self.composer_history_index = None;
            if let Some(draft) = self.composer_history_draft.take() {
                self.restore_input_draft(draft);
            }
        }
    }

    fn recall_composer_input(&mut self, index: usize) {
        let Some(entry) = self.composer_history.get_mut(index) else {
            return;
        };
        let input = std::mem::take(entry);
        self.composer.text.clear();
        self.composer.pastes.clear();
        self.composer.cursor = 0;
        self.insert_paste(&input);
        if let Some(entry) = self.composer_history.get_mut(index) {
            *entry = input;
        }
        self.composer_history_index = Some(index);
    }

    fn apply_block(&mut self, rendered: RenderedBlock) {
        let capability = Arc::<str>::from(rendered.capability);
        let mut block = rendered.block;
        let title = bounded_title(&std::mem::take(&mut block.title));
        let mut text = bounded_terminal_text(&super::block_text(&block), MAX_ENTRY_BYTES);
        let detail = (block.state == FrontendBlockState::Pending
            && matches!(
                block.role,
                FrontendBlockRole::Tool | FrontendBlockRole::WebSearch
            )
            && !text.is_empty())
        .then(|| std::mem::take(&mut text));
        let id = block.id.map(|value| BlockKey {
            capability: Arc::clone(&capability),
            value,
        });
        let group = block.group.map(|value| BlockKey { capability, value });
        if let Some(id) = id.as_ref()
            && let Some(entry) = self
                .transcript
                .iter_mut()
                .rev()
                .find(|entry| entry.id.as_ref() == Some(id))
        {
            if block.update == FrontendBlockUpdate::Append {
                block.update.apply(&mut entry.text, &text);
                view::truncate_display_text(&mut entry.text, MAX_ENTRY_BYTES);
                if detail.is_some() {
                    entry.detail = detail;
                }
            } else {
                entry.text = text;
                entry.detail = detail;
            }
            if !title.is_empty() {
                entry.title = Some(title);
            }
            entry.role = Some(block.role);
            entry.format = block.format;
            entry.tone = block.tone.into();
            if group.is_some() {
                entry.group = group;
            }
            entry.pending = block.state == FrontendBlockState::Pending;
            entry.rendered = None;
            entry.stream = None;
            return;
        }
        self.transcript.push_back(TranscriptEntry {
            id,
            group,
            title: (!title.is_empty()).then_some(title),
            role: Some(block.role),
            detail,
            text,
            format: block.format,
            tone: block.tone.into(),
            pending: block.state == FrontendBlockState::Pending,
            rendered: None,
            stream: None,
        });
        self.trim_transcript();
    }

    /// Remembers which entry the gateway's block item `item` draws into.
    fn remember_block(&mut self, item: Arc<str>, capability: &str, block: &FrontendBlock) {
        if let Some(value) = &block.id {
            let key = BlockKey {
                capability: capability.into(),
                value: value.clone(), // rules-allow: the entry key and the block each own the block id
            };
            self.block_items.insert(item, key);
        }
    }

    /// Applies a gateway block append to the entry its item draws into.
    fn apply_block_delta(&mut self, item: &str, delta: BlockDelta) {
        let Some(key) = self.block_items.get(item) else {
            return;
        };
        let Some(entry) = self
            .transcript
            .iter_mut()
            .rev()
            .find(|entry| entry.id.as_ref() == Some(key))
        else {
            return;
        };
        let BlockDelta {
            text,
            files,
            content,
            title,
            state,
            role,
            tone,
            format,
            group,
            ..
        } = delta;
        let added = bounded_terminal_text(
            &super::block_text(&FrontendBlock {
                text,
                files,
                content: ToolContent(content),
                ..FrontendBlock::default()
            }),
            MAX_ENTRY_BYTES,
        );
        let mut body = entry
            .detail
            .take()
            .unwrap_or_else(|| std::mem::take(&mut entry.text));
        if body.is_empty() {
            body.push_str(added.trim_prefix('\n'));
        } else {
            body.push_str(&added);
        }
        view::truncate_display_text(&mut body, MAX_ENTRY_BYTES);
        if let Some(title) = title.filter(|title| !title.is_empty()) {
            entry.title = Some(bounded_title(&title));
        }
        if let Some(role) = role {
            entry.role = Some(role);
        }
        if let Some(format) = format {
            entry.format = format;
        }
        if let Some(tone) = tone {
            entry.tone = tone.into();
        }
        if let Some(value) = group {
            entry.group = Some(BlockKey {
                capability: Arc::clone(&key.capability),
                value,
            });
        }
        if let Some(state) = state {
            entry.pending = state == FrontendBlockState::Pending;
        }
        if entry.pending
            && matches!(
                entry.role,
                Some(FrontendBlockRole::Tool | FrontendBlockRole::WebSearch)
            )
            && !body.is_empty()
        {
            entry.detail = Some(body);
        } else {
            entry.text = body;
        }
        entry.rendered = None;
        entry.stream = None;
    }

    fn push(&mut self, text: impl AsRef<str>, tone: TranscriptTone) {
        let text = bounded_terminal_text(text.as_ref(), MAX_ENTRY_BYTES);
        self.push_entry(text, tone);
    }

    fn push_entry(&mut self, text: String, tone: TranscriptTone) {
        self.push_titled_entry(None, text, tone);
    }

    /// Adds a titled block, such as a downloaded file's text under its name.
    fn push_titled(&mut self, title: &str, text: &str, tone: TranscriptTone) {
        let text = bounded_terminal_text(text, MAX_ENTRY_BYTES);
        self.push_titled_entry(Some(bounded_title(title)), text, tone);
    }

    fn push_titled_entry(&mut self, title: Option<String>, text: String, tone: TranscriptTone) {
        if text.is_empty() {
            return;
        }
        self.transcript.push_back(TranscriptEntry {
            id: None,
            group: None,
            title,
            role: None,
            detail: None,
            text,
            format: FrontendBlockFormat::PlainText,
            tone,
            pending: false,
            rendered: None,
            stream: None,
        });
        self.trim_transcript();
    }

    fn trim_transcript(&mut self) {
        while self.transcript.len() > MAX_TRANSCRIPT_ENTRIES {
            if let Some(TranscriptEntry { id: Some(id), .. }) = self.transcript.pop_front()
                && !self
                    .transcript
                    .iter()
                    .any(|entry| entry.id.as_ref() == Some(&id))
            {
                self.block_items.retain(|_, key| *key != id);
            }
        }
    }

    fn finish_turn(&mut self, turn_id: &str) {
        self.active_turns.retain(|active| **active != *turn_id);
        self.approvals
            .retain(|request| *request.turn_id != *turn_id);
        self.sync_attention_approvals();
        if self.active_turns.is_empty() {
            self.turn_started_at = None;
            self.reasoning_title = None;
        }
        self.restore_draft();
    }

    fn open_transcript_preview(&mut self) {
        self.picker = None;
        self.capability_overlay = None;
        self.preview = Some(PreviewState::new(
            "Transcript",
            PreviewContent::LiveTranscript,
        ));
    }

    fn open_diff_preview(&mut self, title: &str, text: &str) {
        self.picker = None;
        self.capability_overlay = None;
        self.preview = Some(PreviewState::new(
            title,
            PreviewContent::Diff(Box::new(diff::DiffBrowser::new(text))),
        ));
    }
}

fn latest_summary_line(text: &str) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with("<!--") {
            return None;
        }
        let line = line.trim_start_matches('#').trim();
        let title: std::borrow::Cow<'_, str> = if let Some(bold) = line.strip_prefix("**") {
            let (title, suffix) = bold.split_once("**")?;
            format!("{title}{suffix}").into()
        } else {
            line.into()
        };
        (!title.is_empty()).then(|| title.chars().take(160).collect())
    })
}

fn compact_tool_detail(value: &str) -> String {
    let mut lines = value.lines();
    let mut compact = lines
        .by_ref()
        .take(MAX_TOOL_DETAIL_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if lines.next().is_some() {
        compact.push_str("\n…");
    }
    bounded_terminal_text(&compact, MAX_TOOL_DETAIL_BYTES)
}

fn bounded_title(value: &str) -> String {
    let mut value = crate::frontend::terminal::terminal_line(value);
    truncate_bytes(&mut value, MAX_TITLE_BYTES);
    value
}

#[cfg(test)]
#[path = "tui_tests.rs"]
mod tests;
