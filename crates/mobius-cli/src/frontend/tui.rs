//! Minimal state-driven Sora terminal frontend.

mod clipboard;
mod events;
mod highlight;
mod input;
mod markdown;
mod references;
pub(super) mod runtime;
mod shimmer;
mod view;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

use ratatui::text::Line;

use self::events::UsageStatus;
#[cfg(test)]
use self::input::UiAction;
use self::view::bounded_terminal_text;
use self::view::initial_widgets;
use super::catalog::{MenuItem, UiCatalog};
use super::terminal::terminal_text;
use mobius::protocol::AgentMessagePhase;
#[cfg(test)]
use mobius::protocol::EventMsg;
use mobius::protocol::FrontendBlockFormat;
use mobius::protocol::FrontendBlockState;
use mobius::protocol::FrontendBlockUpdate;
use mobius::protocol::FrontendPickerOption;
use mobius::protocol::FrontendTone;
use mobius::protocol::FrontendWidget;
use mobius::protocol::ModelChoice;
use mobius::protocol::ModelInfo;
use mobius::protocol::ModelStepContentPhase;
use mobius::protocol::Op;
use mobius::protocol::RenderedBlock;
use mobius::protocol::SessionFileReference;
use mobius::protocol::SessionResumeRequestedEvent;

const MAX_ENTRY_BYTES: usize = 40_000;
const MAX_COMPOSER_HISTORY_ENTRIES: usize = 100;
const MAX_TITLE_BYTES: usize = 160;
const MAX_STREAM_BYTES: usize = 64 * 1024;
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
    text: String,
    format: FrontendBlockFormat,
    tone: TranscriptTone,
    pending: bool,
    rendered: Option<(u16, Vec<Line<'static>>)>,
}

#[derive(Clone, PartialEq, Eq)]
struct BlockKey {
    capability: String,
    value: String,
}

#[derive(Default)]
struct StreamedStepPhases {
    reasoning: bool,
    commentary: bool,
    final_answer: bool,
}

impl StreamedStepPhases {
    fn insert(&mut self, phase: ModelStepContentPhase) {
        match phase {
            ModelStepContentPhase::Reasoning => self.reasoning = true,
            ModelStepContentPhase::Commentary => self.commentary = true,
            ModelStepContentPhase::FinalAnswer => self.final_answer = true,
        }
    }

    const fn contains(&self, phase: ModelStepContentPhase) -> bool {
        match phase {
            ModelStepContentPhase::Reasoning => self.reasoning,
            ModelStepContentPhase::Commentary => self.commentary,
            ModelStepContentPhase::FinalAnswer => self.final_answer,
        }
    }
}

struct PickerState {
    title: String,
    options: Vec<FrontendPickerOption>,
    selected: usize,
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
    fn new(title: String, content: PreviewContent) -> Self {
        Self {
            title: bounded_title(&title),
            subtitle: String::new(),
            content,
            viewport: Viewport::default(),
        }
    }

    fn snapshot(
        id: String,
        title: String,
        subtitle: String,
        page_id: String,
        transcript: VecDeque<TranscriptEntry>,
        next: Option<Op>,
    ) -> Self {
        Self {
            title: bounded_title(&title),
            subtitle: bounded_title(&subtitle),
            content: PreviewContent::Snapshot(SnapshotPreview::new(id, page_id, transcript, next)),
            viewport: Viewport::default(),
        }
    }
}

enum PreviewContent {
    LiveTranscript,
    Snapshot(SnapshotPreview),
}

struct SnapshotPreview {
    id: String,
    page_ids: BTreeSet<String>,
    transcript: VecDeque<TranscriptEntry>,
    next: Option<Op>,
}

impl SnapshotPreview {
    fn new(
        id: String,
        page_id: String,
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
        page_id: String,
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

struct InputDraft {
    text: String,
    cursor: usize,
    pastes: BTreeMap<char, String>,
    attachments: Vec<SessionFileReference>,
}

#[derive(Default)]
struct TuiState {
    widgets: Vec<((String, String), FrontendWidget)>,
    transcript: VecDeque<TranscriptEntry>,
    transcript_viewport: Viewport,
    streaming: String,
    streaming_phase: Option<AgentMessagePhase>,
    reasoning: String,
    streamed_step_phases: BTreeMap<String, StreamedStepPhases>,
    completed_model_steps: BTreeSet<String>,
    input: String,
    cursor: usize,
    pastes: BTreeMap<char, String>,
    attachments: Vec<SessionFileReference>,
    input_limit_reached: bool,
    upload_in_progress: bool,
    composer_history: VecDeque<String>,
    composer_history_index: Option<usize>,
    composer_history_draft: Option<InputDraft>,
    approval: Option<String>,
    approval_draft: Option<InputDraft>,
    active_turn: Option<String>,
    turn_started_at: Option<Instant>,
    usage: UsageStatus,
    context_limit: Option<i64>,
    cwd: String,
    model: ModelInfo,
    model_route: String,
    model_choices: Vec<ModelChoice>,
    agent_summary: String,
    disconnected: bool,
    slash_selection: usize,
    slash_menu_dismissed: bool,
    reference_selection: usize,
    reference_menu_dismissed: bool,
    reference_cache: Option<(char, String, Vec<MenuItem>)>,
    picker: Option<PickerState>,
    preview: Option<PreviewState>,
    requested_resume: Option<SessionResumeRequestedEvent>,
}

impl TuiState {
    fn new(
        catalog: &UiCatalog,
        cwd: std::path::PathBuf,
        mut model: ModelInfo,
        model_route: String,
        agent_summary: String,
    ) -> Self {
        model.model = terminal_text(&model.model);
        model.reasoning_effort = model.reasoning_effort.map(|effort| terminal_text(&effort));
        Self {
            widgets: initial_widgets(catalog),
            transcript: VecDeque::new(),
            transcript_viewport: Viewport::default(),
            streaming: String::new(),
            streaming_phase: None,
            reasoning: String::new(),
            streamed_step_phases: BTreeMap::new(),
            completed_model_steps: BTreeSet::new(),
            input: String::new(),
            cursor: 0,
            pastes: BTreeMap::new(),
            attachments: Vec::new(),
            input_limit_reached: false,
            upload_in_progress: false,
            composer_history: VecDeque::new(),
            composer_history_index: None,
            composer_history_draft: None,
            approval: None,
            approval_draft: None,
            active_turn: None,
            turn_started_at: None,
            usage: UsageStatus::default(),
            context_limit: None,
            cwd: terminal_text(&cwd.display().to_string()),
            model,
            model_route,
            model_choices: catalog.model_choices().to_vec(),
            agent_summary,
            disconnected: false,
            slash_selection: 0,
            slash_menu_dismissed: false,
            reference_selection: 0,
            reference_menu_dismissed: false,
            reference_cache: None,
            picker: None,
            preview: None,
            requested_resume: None,
        }
    }

    fn is_working(&self) -> bool {
        self.active_turn.is_some() && self.approval.is_none() && !self.disconnected
    }

    fn begin_approval(&mut self, id: String) {
        if self.approval.is_none() {
            self.approval_draft = Some(self.take_input_draft());
        }
        self.approval = Some(id);
    }

    fn restore_draft(&mut self) {
        if let Some(draft) = self.approval_draft.take() {
            self.restore_input_draft(draft);
        }
    }

    fn clear_approval(&mut self) {
        self.approval = None;
        self.restore_draft();
    }

    fn take_input_draft(&mut self) -> InputDraft {
        let draft = InputDraft {
            text: std::mem::take(&mut self.input),
            cursor: self.cursor,
            pastes: std::mem::take(&mut self.pastes),
            attachments: std::mem::take(&mut self.attachments),
        };
        self.cursor = 0;
        draft
    }

    fn restore_input_draft(&mut self, mut draft: InputDraft) {
        for attachment in std::mem::take(&mut self.attachments) {
            if !draft
                .attachments
                .iter()
                .any(|existing| existing.id == attachment.id)
            {
                draft.attachments.push(attachment);
            }
        }
        self.input = draft.text;
        self.cursor = draft.cursor.min(self.input.len());
        self.pastes = draft.pastes;
        self.attachments = draft.attachments;
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
        let index = match self.composer_history_index {
            Some(index) => index.saturating_sub(1),
            None => {
                let draft = self.take_input_draft();
                self.attachments.clone_from(&draft.attachments);
                self.composer_history_draft = Some(draft);
                self.composer_history.len() - 1
            }
        };
        self.recall_composer_input(index);
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
        let input = self.composer_history[index].clone();
        self.input.clear();
        self.pastes.clear();
        self.cursor = 0;
        self.insert_paste(&input);
        self.composer_history_index = Some(index);
    }

    fn apply_block(&mut self, rendered: RenderedBlock) {
        self.commit_reasoning();
        let capability = rendered.capability;
        let block = rendered.block;
        let mut text = if block.format == FrontendBlockFormat::UnifiedDiff && block.files.is_empty()
        {
            bounded_terminal_text(&block.text, MAX_ENTRY_BYTES)
        } else {
            bounded_terminal_text(&super::block_text(&block), MAX_ENTRY_BYTES)
        };
        let id = block.id.map(|value| BlockKey {
            capability: capability.clone(),
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
                text.insert_str(0, &entry.text);
            }
            entry.text = bounded_terminal_text(&text, MAX_ENTRY_BYTES);
            entry.format = block.format;
            entry.tone = block.tone.into();
            if group.is_some() {
                entry.group = group;
            }
            entry.pending = block.state == FrontendBlockState::Pending;
            entry.rendered = None;
            return;
        }
        if block.update == FrontendBlockUpdate::Append {
            text = text.trim_start_matches('\n').to_string();
        }
        self.transcript.push_back(TranscriptEntry {
            id,
            group,
            text,
            format: block.format,
            tone: block.tone.into(),
            pending: block.state == FrontendBlockState::Pending,
            rendered: None,
        });
        self.trim_transcript();
    }

    fn remember_streamed_phase(&mut self, model_step_id: &str, phase: ModelStepContentPhase) {
        self.streamed_step_phases
            .entry(model_step_id.into())
            .or_default()
            .insert(phase);
    }

    fn append_stream(&mut self, delta: &str, phase: AgentMessagePhase) {
        self.commit_reasoning();
        if self.streaming_phase.is_some_and(|current| current != phase) {
            self.commit_stream();
        }
        self.streaming_phase = Some(phase);
        if self.streaming.len() >= MAX_STREAM_BYTES {
            return;
        }
        let mut delta = terminal_text(delta);
        let available = MAX_STREAM_BYTES - self.streaming.len();
        let truncated = delta.len() > available;
        truncate_bytes(&mut delta, available);
        self.streaming.push_str(&delta);
        if truncated {
            self.streaming.push_str("\n[message truncated]");
        }
    }

    fn append_reasoning(&mut self, delta: &str) {
        if self.reasoning.len() >= MAX_STREAM_BYTES {
            return;
        }
        let mut delta = terminal_text(delta);
        let available = MAX_STREAM_BYTES - self.reasoning.len();
        let truncated = delta.len() > available;
        truncate_bytes(&mut delta, available);
        self.reasoning.push_str(&delta);
        if truncated {
            self.reasoning.push_str("\n[reasoning truncated]");
        }
    }

    fn commit_stream(&mut self) {
        if self.streaming.is_empty() {
            self.streaming_phase = None;
            return;
        }
        let text = std::mem::take(&mut self.streaming);
        self.streaming_phase = None;
        self.push_entry(text, TranscriptTone::Assistant);
    }

    fn commit_commentary_stream(&mut self) {
        if self.streaming_phase == Some(AgentMessagePhase::Commentary) {
            self.commit_stream();
        }
    }

    fn commit_reasoning(&mut self) {
        if self.reasoning.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.reasoning);
        self.push_entry(text, TranscriptTone::Reasoning);
    }

    fn push(&mut self, text: impl AsRef<str>, tone: TranscriptTone) {
        self.commit_reasoning();
        let text = bounded_terminal_text(text.as_ref(), MAX_ENTRY_BYTES);
        self.push_entry(text, tone);
    }

    fn push_entry(&mut self, text: String, tone: TranscriptTone) {
        if text.is_empty() {
            return;
        }
        self.transcript.push_back(TranscriptEntry {
            id: None,
            group: None,
            text,
            format: FrontendBlockFormat::PlainText,
            tone,
            pending: false,
            rendered: None,
        });
        self.trim_transcript();
    }

    fn trim_transcript(&mut self) {
        while self.transcript.len() > MAX_TRANSCRIPT_ENTRIES {
            self.transcript.pop_front();
        }
    }

    fn finish_turn(&mut self) {
        self.commit_reasoning();
        self.commit_stream();
        self.streamed_step_phases.clear();
        self.completed_model_steps.clear();
        self.active_turn = None;
        self.turn_started_at = None;
        self.clear_approval();
    }

    fn open_transcript_preview(&mut self) {
        self.preview = Some(PreviewState::new(
            "Transcript".into(),
            PreviewContent::LiveTranscript,
        ));
    }
}

fn bounded_title(value: &str) -> String {
    let mut value = terminal_text(value).replace(['\n', '\t'], " ");
    truncate_bytes(&mut value, MAX_TITLE_BYTES);
    value
}

fn truncate_bytes(value: &mut String, limit: usize) {
    if value.len() <= limit {
        return;
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

#[cfg(test)]
#[path = "tui_tests.rs"]
mod tests;
