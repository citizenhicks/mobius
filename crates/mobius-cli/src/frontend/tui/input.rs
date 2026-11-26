use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};

use super::references::{ReferenceToken, active_reference_token, replace_reference_token};
use super::view::widget_status;
use super::{PreviewContent, PreviewState, TranscriptTone, TuiState};
pub(super) use crate::frontend::catalog::UiAction;
use crate::frontend::catalog::{
    CommandAction, CommandContext, GatewayAction, KeyedWidgets, MenuItem, UiCatalog,
};
use crate::frontend::dashboard::{OverlayKey, insert_overlay_input, overlay_key};
use crate::frontend::terminal::terminal_text;
use crate::frontend::wrap_step;
use mobius::protocol::{
    ActiveMessageDelivery, FrontendSlot, FrontendWidget, MAX_MESSAGE_BYTES, MessageSubmission, Op,
    ReviewDecision, SessionFileOrigin, SessionFileRecord, SessionFileReference,
};
use mobius_gateway::wire::{BackgroundApproval, BotRecord, GitStatus};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

const COLLAPSED_PASTE_BYTES: usize = 200;
const SCROLL_ROWS: usize = 3;
const WORD_SEPARATORS: &str = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?";

impl TuiState {
    pub(super) fn handle_key(&mut self, key: KeyEvent, catalog: &UiCatalog) -> UiAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return UiAction::None;
        }
        if let Some(action) = self.handle_open_surface_key(key) {
            return action;
        }
        if self.approval().is_some() && key.kind == KeyEventKind::Repeat {
            return UiAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.handle_ctrl_c(key.kind);
        }
        if self.picker.is_some() {
            return self.handle_picker_key(key);
        }
        if let Some(action) = self.handle_transcript_scroll_key(key.code) {
            return action;
        }
        if key.kind == KeyEventKind::Press
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char(character) if character.eq_ignore_ascii_case(&'v'))
        {
            return UiAction::PasteClipboard;
        }
        if key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            self.insert_text("\n");
            return UiAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.handle_control_key(key.code, catalog);
        }
        if let Some(action) = self
            .handle_approval_key(key.code)
            .or_else(|| self.handle_question_key(key.code))
        {
            return action;
        }
        match key.code {
            KeyCode::Enter if self.approval().is_some() => {
                self.submit_input_with_delivery(catalog, None)
            }
            KeyCode::Enter if self.complete_reference(catalog) => UiAction::None,
            KeyCode::Enter => {
                let delivery = (self.composer_target_turn().is_some()
                    && key.modifiers.contains(KeyModifiers::ALT))
                .then(|| self.alternate_message_delivery());
                self.submit_selected_slash(catalog, delivery)
                    .unwrap_or_else(|| self.submit_input_with_delivery(catalog, delivery))
            }
            KeyCode::Up => {
                if !self.move_menu(catalog, -1)
                    && self.approval().is_none()
                    && !self.move_composer_line(-1)
                {
                    self.composer_history_up();
                }
                UiAction::None
            }
            KeyCode::Down => {
                if !self.move_menu(catalog, 1)
                    && self.approval().is_none()
                    && !self.move_composer_line(1)
                {
                    self.composer_history_down();
                }
                UiAction::None
            }
            KeyCode::Esc if self.approval().is_some() => {
                self.take_approval().map_or(UiAction::None, |id| {
                    UiAction::submit(Op::ExecApproval {
                        id,
                        decision: ReviewDecision::Abort,
                    })
                })
            }
            KeyCode::Esc if self.reference_suggestions(catalog).is_some() => {
                self.reference_menu_dismissed = true;
                UiAction::None
            }
            KeyCode::Esc if self.slash_suggestions(catalog).is_some() => {
                self.slash_menu_dismissed = true;
                UiAction::None
            }
            KeyCode::Esc if key.kind == KeyEventKind::Press && self.is_working() => {
                self.active_turn().map_or(UiAction::None, |turn_id| {
                    UiAction::submit(Op::Interrupt {
                        turn_id: Arc::clone(turn_id),
                    })
                })
            }
            KeyCode::Backspace => {
                if self.composer.text.is_empty() {
                    self.composer.attachments.pop();
                    return UiAction::None;
                }
                let previous = if key.modifiers.contains(KeyModifiers::ALT) {
                    previous_word_boundary(&self.composer.text, self.composer.cursor)
                } else {
                    previous_boundary(&self.composer.text, self.composer.cursor)
                };
                self.composer.text.drain(previous..self.composer.cursor);
                self.composer.cursor = previous;
                self.prune_pastes();
                self.slash_input_changed();
                UiAction::None
            }
            KeyCode::Delete => {
                let next = next_boundary(&self.composer.text, self.composer.cursor);
                self.composer.text.drain(self.composer.cursor..next);
                self.prune_pastes();
                self.slash_input_changed();
                UiAction::None
            }
            KeyCode::Left => {
                self.composer.cursor = if key.modifiers.contains(KeyModifiers::ALT) {
                    previous_word_boundary(&self.composer.text, self.composer.cursor)
                } else {
                    previous_boundary(&self.composer.text, self.composer.cursor)
                };
                UiAction::None
            }
            KeyCode::Right => {
                self.composer.cursor = if key.modifiers.contains(KeyModifiers::ALT) {
                    next_word_boundary(&self.composer.text, self.composer.cursor)
                } else {
                    next_boundary(&self.composer.text, self.composer.cursor)
                };
                UiAction::None
            }
            KeyCode::Home => {
                self.composer.cursor = 0;
                UiAction::None
            }
            KeyCode::End => {
                self.composer.cursor = self.composer.text.len();
                UiAction::None
            }
            KeyCode::Tab => {
                if !self.complete_reference(catalog) {
                    self.complete_slash(catalog);
                }
                UiAction::None
            }
            KeyCode::Char('q') if self.disconnected && self.composer.text.is_empty() => {
                UiAction::Exit
            }
            KeyCode::Char(character) => {
                self.insert_text(&character.to_string());
                UiAction::None
            }
            _ => UiAction::None,
        }
    }

    /// Moves through or picks from the approval card; other keys keep editing the reason.
    fn handle_approval_key(&mut self, code: KeyCode) -> Option<UiAction> {
        self.approval()?;
        let last = APPROVAL_CHOICES.len() - 1;
        match code {
            KeyCode::Up => {
                self.approval_selection = self.approval_selection.checked_sub(1).unwrap_or(last);
            }
            KeyCode::Left if self.composer.text.is_empty() => {
                self.approval_selection = self.approval_selection.checked_sub(1).unwrap_or(last);
            }
            KeyCode::Down => {
                self.approval_selection = if self.approval_selection >= last {
                    0
                } else {
                    self.approval_selection + 1
                };
            }
            KeyCode::Right if self.composer.text.is_empty() => {
                self.approval_selection = if self.approval_selection >= last {
                    0
                } else {
                    self.approval_selection + 1
                };
            }
            KeyCode::Char(digit) if self.composer.text.is_empty() => {
                let index = digit.to_digit(10)?.checked_sub(1)?;
                let decision = approval_choice(usize::try_from(index).ok()?)?;
                return Some(self.decide_approval(decision));
            }
            _ => return None,
        }
        Some(UiAction::None)
    }

    /// Answers the inline question: a digit on an empty composer picks a choice, and a choice
    /// that needs text turns the composer into its answer until Enter sends it or Esc drops it.
    fn handle_question_key(&mut self, code: KeyCode) -> Option<UiAction> {
        if self.approval().is_some() {
            return None;
        }
        if self.question_answer.is_some() {
            return match code {
                KeyCode::Esc => {
                    self.question_answer = None;
                    Some(UiAction::None)
                }
                KeyCode::Enter if !self.composer.text.trim().is_empty() => {
                    let mut op = self.question_answer.take()?;
                    let text = std::mem::take(&mut self.composer.text);
                    self.composer.cursor = 0;
                    self.composer.pastes.clear();
                    if let Op::CapabilityCommand { input, .. } = &mut op {
                        *input = Some(text.trim().to_owned());
                    }
                    Some(UiAction::submit(op))
                }
                _ => None,
            };
        }
        let KeyCode::Char(digit) = code else {
            return None;
        };
        if !self.composer.text.is_empty() {
            return None;
        }
        let index = usize::try_from(digit.to_digit(10)?.checked_sub(1)?).ok()?;
        let action = self.inline_question()?.actions.get(index)?;
        if action.editor.is_some() {
            self.question_answer = Some(action.op.clone()); // rules-allow: the widget keeps its action while the composer fills a copy
            return Some(UiAction::None);
        }
        Some(UiAction::submit(action.operation()))
    }

    fn decide_approval(&mut self, decision: ReviewDecision) -> UiAction {
        self.take_approval().map_or(UiAction::None, |id| {
            UiAction::submit(Op::ExecApproval { id, decision })
        })
    }

    fn handle_ctrl_c(&mut self, kind: KeyEventKind) -> UiAction {
        if kind != KeyEventKind::Press || self.picker.take().is_some() {
            return UiAction::None;
        }
        if let Some(id) = self.take_approval() {
            return UiAction::submit(Op::ExecApproval {
                id,
                decision: ReviewDecision::Abort,
            });
        }
        if let Some(turn_id) = self.active_turn().filter(|_| self.is_working()) {
            return UiAction::submit(Op::Interrupt {
                turn_id: Arc::clone(turn_id),
            });
        }
        if !self.composer.text.is_empty()
            || !self.composer.attachments.is_empty()
            || self.question_answer.is_some()
        {
            self.composer.text.clear();
            self.composer.cursor = 0;
            self.composer.pastes.clear();
            self.composer.attachments.clear();
            self.question_answer = None;
            self.composer_history_index = None;
            self.composer_history_draft = None;
            self.slash_input_changed();
            return UiAction::None;
        }
        UiAction::Exit
    }

    fn handle_control_key(&mut self, code: KeyCode, catalog: &UiCatalog) -> UiAction {
        match code {
            KeyCode::Char('d') if self.composer.text.is_empty() => UiAction::Exit,
            KeyCode::Char('j') => {
                self.insert_text("\n");
                UiAction::None
            }
            KeyCode::Char('t') => {
                self.open_transcript_preview();
                UiAction::None
            }
            KeyCode::Char('r') if self.approval().is_none() => {
                self.recover_saved_message();
                UiAction::None
            }
            KeyCode::Char('a') => {
                self.composer.cursor = self
                    .composer
                    .text
                    .get(..self.composer.cursor)
                    .unwrap_or_default()
                    .rfind('\n')
                    .map_or(0, |index| index + 1);
                UiAction::None
            }
            KeyCode::Char('e') => {
                self.composer.cursor = self
                    .composer
                    .text
                    .get(self.composer.cursor..)
                    .unwrap_or_default()
                    .find('\n')
                    .map_or(self.composer.text.len(), |index| {
                        self.composer.cursor + index
                    });
                UiAction::None
            }
            KeyCode::Char('u' | 'w') => {
                let start = if code == KeyCode::Char('u') {
                    self.composer
                        .text
                        .get(..self.composer.cursor)
                        .unwrap_or_default()
                        .rfind('\n')
                        .map_or(0, |index| index + 1)
                } else {
                    previous_word_boundary(&self.composer.text, self.composer.cursor)
                };
                self.composer.text.drain(start..self.composer.cursor);
                self.composer.cursor = start;
                self.prune_pastes();
                self.slash_input_changed();
                UiAction::None
            }
            KeyCode::Char('k') => {
                let tail = self
                    .composer
                    .text
                    .get(self.composer.cursor..)
                    .unwrap_or_default();
                let end = match tail.find('\n') {
                    Some(0) => self.composer.cursor + 1,
                    Some(index) => self.composer.cursor + index,
                    None => self.composer.text.len(),
                };
                if end > self.composer.cursor {
                    self.composer.text.drain(self.composer.cursor..end);
                    self.prune_pastes();
                    self.slash_input_changed();
                }
                UiAction::None
            }
            KeyCode::Left => {
                self.composer.cursor =
                    previous_word_boundary(&self.composer.text, self.composer.cursor);
                UiAction::None
            }
            KeyCode::Right => {
                self.composer.cursor =
                    next_word_boundary(&self.composer.text, self.composer.cursor);
                UiAction::None
            }
            KeyCode::Char('p') => {
                if !self.move_menu(catalog, -1)
                    && self.approval().is_none()
                    && !self.move_composer_line(-1)
                {
                    self.composer_history_up();
                }
                UiAction::None
            }
            KeyCode::Char('n') => {
                if !self.move_menu(catalog, 1)
                    && self.approval().is_none()
                    && !self.move_composer_line(1)
                {
                    self.composer_history_down();
                }
                UiAction::None
            }
            _ => UiAction::None,
        }
    }

    fn handle_transcript_scroll_key(&mut self, key: KeyCode) -> Option<UiAction> {
        match key {
            KeyCode::PageUp => {
                let rows = self.transcript_viewport.page_height();
                self.transcript_viewport.scroll_up(rows);
                if self.transcript_viewport.effective_scroll() == 0
                    && self.history_cursor.is_some()
                    && self.history_request_id.is_none()
                {
                    Some(UiAction::LoadEarlierHistory)
                } else {
                    Some(UiAction::None)
                }
            }
            KeyCode::PageDown => {
                let rows = self.transcript_viewport.page_height();
                self.transcript_viewport.scroll_down(rows);
                Some(UiAction::None)
            }
            _ => None,
        }
    }

    fn handle_preview_key(&mut self, key: KeyEvent) -> UiAction {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
            || (key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c' | 't')))
        {
            self.preview = None;
            return UiAction::None;
        }
        if key.kind == KeyEventKind::Press
            && matches!(key.code, KeyCode::Char('o' | 'O'))
            && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
        {
            return self
                .preview
                .as_ref()
                .and_then(|preview| {
                    let PreviewContent::Snapshot(snapshot) = &preview.content else {
                        return None;
                    };
                    snapshot.next.clone() // rules-allow: the preview keeps offering the next page until the gateway replaces it
                })
                .map_or(UiAction::None, UiAction::submit);
        }
        let Some(preview) = self.preview.as_mut() else {
            return UiAction::None;
        };
        if let PreviewContent::Diff(browser) = &mut preview.content {
            browser.handle_key(key);
            return UiAction::None;
        }
        match key.code {
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                preview
                    .viewport
                    .scroll_up(preview.viewport.page_height().div_ceil(2));
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                preview
                    .viewport
                    .scroll_down(preview.viewport.page_height().div_ceil(2));
            }
            KeyCode::Up => preview.viewport.scroll_up(SCROLL_ROWS),
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                preview.viewport.scroll_up(SCROLL_ROWS);
            }
            KeyCode::Down => preview.viewport.scroll_down(SCROLL_ROWS),
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                preview.viewport.scroll_down(SCROLL_ROWS);
            }
            KeyCode::PageUp => {
                let rows = preview.viewport.page_height();
                preview.viewport.scroll_up(rows);
            }
            KeyCode::PageDown => {
                let rows = preview.viewport.page_height();
                preview.viewport.scroll_down(rows);
            }
            KeyCode::Home => preview.viewport.top(),
            KeyCode::End => preview.viewport.bottom(),
            _ => {}
        }
        UiAction::None
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) -> (bool, UiAction) {
        if self.picker.is_some() || self.capability_overlay.is_some() {
            return (false, UiAction::None);
        }
        if let Some(PreviewState {
            content: PreviewContent::Diff(browser),
            ..
        }) = self.preview.as_mut()
        {
            return (browser.handle_mouse(mouse), UiAction::None);
        }
        let scrolling_live_transcript = self.preview.is_none();
        let viewport = self
            .preview
            .as_mut()
            .map_or(&mut self.transcript_viewport, |preview| {
                &mut preview.viewport
            });
        let before = viewport.effective_scroll();
        match mouse.kind {
            MouseEventKind::ScrollUp => viewport.scroll_up(SCROLL_ROWS),
            MouseEventKind::ScrollDown => viewport.scroll_down(SCROLL_ROWS),
            _ => {}
        }
        let changed = viewport.effective_scroll() != before;
        let action = if scrolling_live_transcript
            && matches!(mouse.kind, MouseEventKind::ScrollUp)
            && viewport.effective_scroll() == 0
            && self.history_cursor.is_some()
            && self.history_request_id.is_none()
        {
            UiAction::LoadEarlierHistory
        } else {
            UiAction::None
        };
        (changed, action)
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> UiAction {
        let Some(picker) = self.picker.as_mut() else {
            return UiAction::None;
        };
        match key.code {
            KeyCode::Esc if picker.query.is_empty() => {
                self.picker = None;
                UiAction::None
            }
            KeyCode::Esc => {
                picker.edit_query(String::clear);
                UiAction::None
            }
            KeyCode::Up => {
                picker.selected = wrap_step(picker.selected, -1, picker.match_count());
                UiAction::None
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                picker.selected = wrap_step(picker.selected, -1, picker.match_count());
                UiAction::None
            }
            KeyCode::Down => {
                picker.selected = wrap_step(picker.selected, 1, picker.match_count());
                UiAction::None
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                picker.selected = wrap_step(picker.selected, 1, picker.match_count());
                UiAction::None
            }
            KeyCode::Enter => {
                let Some(index) = picker.selected_index() else {
                    return UiAction::None;
                };
                let option = picker.options.swap_remove(index);
                self.picker = None;
                option.action
            }
            KeyCode::Delete => {
                let Some(action) = picker
                    .selected_index()
                    .and_then(|index| picker.options.get_mut(index))
                    .and_then(|option| option.secondary.take())
                else {
                    return UiAction::None;
                };
                self.picker = None;
                action
            }
            KeyCode::Backspace => {
                picker.edit_query(|query| {
                    query.pop();
                });
                UiAction::None
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                picker.edit_query(|query| query.push(character));
                UiAction::None
            }
            _ => UiAction::None,
        }
    }

    pub(super) fn open_bot_picker(
        &mut self,
        bots: &[BotRecord],
        workspace: Option<&Path>,
        current_bot_id: &str,
        clear: bool,
    ) {
        let workspace = workspace.map(Arc::<Path>::from);
        self.bot_picker("Select Bot", bots, current_bot_id, |bot| {
            workspace.as_ref().map_or_else(
                || UiAction::Resume(bot.conversation_session_id.share()),
                |workspace| UiAction::CreateSession {
                    workspace: Arc::clone(workspace),
                    bot_id: Arc::clone(&bot.id),
                    clear,
                },
            )
        });
    }

    pub(super) fn open_reassign_bot_picker(&mut self, bots: &[BotRecord], current_bot_id: &str) {
        self.bot_picker("Reassign Bot", bots, current_bot_id, |bot| {
            UiAction::Gateway(GatewayAction::Reassign(Arc::clone(&bot.id)))
        });
    }

    fn bot_picker(
        &mut self,
        title: &str,
        bots: &[BotRecord],
        current_bot_id: &str,
        action: impl Fn(&BotRecord) -> UiAction,
    ) {
        self.open_gateway_picker(
            title,
            bots.iter().map(|bot| super::PickerOption {
                label: format!("@{}", terminal_text(&bot.handle)),
                description: terminal_text(&bot.name),
                detail: terminal_text(crate::frontend::bots::bot_model(bot)),
                shows_detail: true,
                action: action(bot),
                secondary: None,
            }),
            bots.iter()
                .position(|bot| *bot.id == *current_bot_id)
                .unwrap_or_default(),
        );
    }

    pub(super) fn open_background_approvals(
        &mut self,
        approvals: &[Arc<BackgroundApproval>],
        bots: &[BotRecord],
    ) {
        if approvals.is_empty() {
            self.push("no background approvals", TranscriptTone::Neutral);
            return;
        }
        self.open_gateway_picker(
            "Background approvals",
            approvals.iter().map(|approval| {
                let handle = bots
                    .iter()
                    .find(|bot| bot.id == approval.bot_id)
                    .map_or(&*approval.bot_id, |bot| &*bot.handle);
                super::PickerOption {
                    label: format!("@{}", terminal_text(handle)),
                    description: "approval required".into(),
                    detail: terminal_text(&approval.request_id),
                    shows_detail: true,
                    action: UiAction::Resume(approval.session_id.share()),
                    secondary: None,
                }
            }),
            0,
        );
    }

    pub(super) fn open_branch_picker(&mut self, git: Option<&GitStatus>) {
        let Some(git) = git else {
            self.push(
                "this workspace is not a Git repository",
                TranscriptTone::Warning,
            );
            return;
        };
        self.open_gateway_picker(
            "Switch branch",
            git.branches.iter().map(|branch| super::PickerOption {
                label: terminal_text(branch),
                description: if branch == &git.current_branch {
                    "current".into()
                } else {
                    String::new()
                },
                detail: String::new(),
                shows_detail: false,
                action: UiAction::Gateway(GatewayAction::SwitchBranch(branch.clone())), // rules-allow: the picker option owns its action while the session keeps its Git status
                secondary: None,
            }),
            git.branches
                .iter()
                .position(|branch| branch == &git.current_branch)
                .unwrap_or_default(),
        );
    }

    pub(super) fn open_session_files(&mut self, files: Vec<SessionFileRecord>) {
        if files.is_empty() {
            self.push("this chat has no files", TranscriptTone::Neutral);
            return;
        }
        self.open_gateway_picker(
            "Chat files",
            files.into_iter().map(|record| {
                let origin = match record.origin {
                    SessionFileOrigin::User => "user",
                    SessionFileOrigin::Agent => "agent",
                };
                let file = record.file;
                super::PickerOption {
                    label: terminal_text(&file.name),
                    description: format!("{origin} · {} bytes", file.size),
                    detail: terminal_text(&file.media_type),
                    shows_detail: true,
                    action: UiAction::Download(file.clone()), // rules-allow: the download and delete actions each own the file
                    secondary: Some(UiAction::ConfirmDeleteFile(file)),
                }
            }),
            0,
        );
    }

    pub(super) fn confirm_delete_file(&mut self, file: SessionFileReference) {
        self.confirm_delete(
            &format!("Delete {}?", terminal_text(&file.name)),
            GatewayAction::DeleteStoredFile(file.id),
        );
    }

    pub(super) fn confirm_delete_current_session(&mut self) {
        self.confirm_delete("Delete this chat?", GatewayAction::DeleteCurrent);
    }

    fn confirm_delete(&mut self, title: &str, action: GatewayAction) {
        self.open_gateway_picker(
            title,
            [super::PickerOption {
                label: "Delete permanently".into(),
                description: "cannot be undone".into(),
                detail: String::new(),
                shows_detail: false,
                action: UiAction::Gateway(action),
                secondary: None,
            }],
            0,
        );
    }

    pub(super) fn open_queued_messages(&mut self) {
        let widgets = self.overlay_widgets(|_, widget| {
            widget.slot == FrontendSlot::TranscriptTail && widget.action.is_some()
        });
        if widgets.is_empty() {
            self.push(
                "there are no editable queued messages",
                TranscriptTone::Neutral,
            );
            return;
        }
        self.picker = None;
        self.preview = None;
        self.capability_overlay =
            Some(crate::frontend::dashboard::CapabilityOverlay::from_widgets(
                "Queued messages".into(),
                widgets,
            ));
    }

    fn open_gateway_picker(
        &mut self,
        title: &str,
        options: impl IntoIterator<Item = super::PickerOption>,
        selected: usize,
    ) {
        self.preview = None;
        self.capability_overlay = None;
        self.picker = Some(super::PickerState::new(
            title.into(),
            options.into_iter().collect(),
            selected,
        ));
    }

    fn handle_open_surface_key(&mut self, key: KeyEvent) -> Option<UiAction> {
        if self.preview.is_some() {
            return Some(self.handle_preview_key(key));
        }
        self.capability_overlay
            .is_some()
            .then(|| self.handle_capability_overlay_key(key))
    }

    fn handle_capability_overlay_key(&mut self, key: KeyEvent) -> UiAction {
        let Some(overlay) = self.capability_overlay.as_mut() else {
            return UiAction::None;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.capability_overlay = None;
            return UiAction::None;
        }
        match overlay_key(overlay, key) {
            OverlayKey::Handled => UiAction::None,
            OverlayKey::Close => {
                self.capability_overlay = None;
                UiAction::None
            }
            OverlayKey::Submit(op) => self.submit_attention_operation(*op),
        }
    }

    pub(super) fn insert_capability_overlay_paste(&mut self, value: &str) -> bool {
        self.capability_overlay
            .as_mut()
            .is_some_and(|overlay| insert_overlay_input(overlay, value))
    }

    pub(super) fn insert_text(&mut self, text: &str) {
        let text = terminal_text(text);
        if !self.accepts_input(text.len(), 0) {
            return;
        }
        self.composer.text.insert_str(self.composer.cursor, &text);
        self.composer.cursor += text.len();
        self.slash_input_changed();
    }

    pub(super) fn insert_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if !text.chars().any(char::is_control) && text.len() < COLLAPSED_PASTE_BYTES {
            self.insert_text(text);
            return;
        }
        if !self.accepts_input(text.len(), 0) {
            return;
        }
        let Some(token) = (1..=8)
            .chain(11..=31)
            .chain(127..=159)
            .filter_map(char::from_u32)
            .find(|token| {
                !self.composer.pastes.contains_key(token) && !self.composer.text.contains(*token)
            })
        else {
            self.insert_text(text);
            return;
        };
        self.composer.text.insert(self.composer.cursor, token);
        self.composer.cursor += token.len_utf8();
        self.composer.pastes.insert(token, text.to_owned());
        self.slash_input_changed();
    }

    #[expect(
        clippy::expect_used,
        reason = "fmt::Write for String never returns an error"
    )]
    pub(super) fn visible_input(&self) -> (String, usize) {
        let mut visible = String::new();
        let mut visible_cursor = 0;
        for (index, character) in self.composer.text.char_indices() {
            if index == self.composer.cursor {
                visible_cursor = visible.len();
            }
            if let Some(paste) = self.composer.pastes.get(&character) {
                write!(visible, "[pasted content {} chars]", paste.chars().count())
                    .expect("writing to String cannot fail");
            } else {
                visible.push(character);
            }
        }
        if self.composer.cursor == self.composer.text.len() {
            visible_cursor = visible.len();
        }
        (visible, visible_cursor)
    }

    fn expanded_input(&self) -> String {
        self.composer.text.chars().fold(
            String::with_capacity(self.expanded_input_bytes()),
            |mut expanded, character| {
                if let Some(paste) = self.composer.pastes.get(&character) {
                    expanded.push_str(paste);
                } else {
                    expanded.push(character);
                }
                expanded
            },
        )
    }

    fn prune_pastes(&mut self) {
        self.composer
            .pastes
            .retain(|token, _| self.composer.text.contains(*token));
    }

    fn complete_slash(&mut self, catalog: &UiCatalog) {
        let Some(item) = self.selected_slash_item(catalog) else {
            return;
        };
        let command = item.value;
        let token_end = self
            .composer
            .text
            .find(char::is_whitespace)
            .unwrap_or(self.composer.text.len());
        if !self.accepts_input(command.len(), token_end) {
            return;
        }
        let tail = self.composer.text.get(token_end..).unwrap_or_default();
        self.composer.text = if tail.is_empty() {
            format!("{command} ")
        } else {
            format!("{command}{tail}")
        };
        self.composer.cursor = self.composer.text.len();
        self.slash_input_changed();
    }

    fn submit_selected_slash(
        &mut self,
        catalog: &UiCatalog,
        delivery: Option<ActiveMessageDelivery>,
    ) -> Option<UiAction> {
        let item = self.selected_slash_item(catalog)?;
        let token_end = self
            .composer
            .text
            .find(char::is_whitespace)
            .unwrap_or(self.composer.text.len());
        if item.requires_arguments && token_end == self.composer.text.len() {
            self.complete_slash(catalog);
            return Some(UiAction::None);
        }
        if !self.accepts_input(item.value.len(), token_end) {
            return Some(UiAction::None);
        }
        self.composer.text.replace_range(..token_end, &item.value);
        self.composer.cursor = self.composer.text.len();
        Some(self.submit_input_with_delivery(catalog, delivery))
    }

    fn selected_slash_item(&self, catalog: &UiCatalog) -> Option<MenuItem> {
        self.slash_suggestions(catalog)?
            .into_iter()
            .nth(self.slash_selection)
    }

    pub(super) fn slash_suggestions(&self, catalog: &UiCatalog) -> Option<Vec<MenuItem>> {
        if self.slash_menu_dismissed {
            return None;
        }
        catalog.command_suggestions(&self.composer.text, self.composer.cursor)
    }

    pub(super) fn reference_suggestions(
        &mut self,
        catalog: &UiCatalog,
    ) -> Option<(ReferenceToken, &[MenuItem])> {
        if self.reference_menu_dismissed {
            return None;
        }
        let trigger = catalog.reference_triggers().find(|trigger| {
            active_reference_token(&self.composer.text, self.composer.cursor, *trigger).is_some()
        })?;
        let token = active_reference_token(&self.composer.text, self.composer.cursor, trigger)?;
        let cached =
            self.reference_cache
                .as_ref()
                .is_some_and(|(cached_trigger, cached_query, _)| {
                    *cached_trigger == trigger && cached_query == &token.query
                });
        if !cached {
            let matches = catalog.reference_suggestions(trigger, &token.query);
            self.reference_cache = Some((trigger, token.query.clone(), matches)); // rules-allow: the cache keys on the query that the returned token still carries
        }
        let matches = &self.reference_cache.as_ref()?.2;
        (!matches.is_empty()).then_some((token, matches.as_slice()))
    }

    fn complete_reference(&mut self, catalog: &UiCatalog) -> bool {
        let selection = self.reference_selection;
        let Some((token, matches)) = self.reference_suggestions(catalog) else {
            return false;
        };
        let Some(item) = matches.get(selection) else {
            return false;
        };
        let inserted = item.value.len();
        if !self.accepts_input(inserted, token.range.len()) {
            return true;
        }
        let Some((_, _, matches)) = &self.reference_cache else {
            return false;
        };
        let Some(item) = matches.get(selection) else {
            return false;
        };
        let Some(cursor) = replace_reference_token(&mut self.composer.text, &token, &item.value)
        else {
            return false;
        };
        self.composer.cursor = cursor;
        self.prune_pastes();
        self.slash_input_changed();
        self.reference_menu_dismissed = true;
        true
    }

    fn move_menu(&mut self, catalog: &UiCatalog, step: isize) -> bool {
        if let Some((_, items)) = self.reference_suggestions(catalog) {
            let count = items.len();
            self.reference_selection = wrap_step(self.reference_selection, step, count);
        } else if let Some(items) = self.slash_suggestions(catalog) {
            self.slash_selection = wrap_step(self.slash_selection, step, items.len());
        } else {
            return false;
        }
        true
    }

    fn slash_input_changed(&mut self) {
        self.input_limit_reached = false;
        self.slash_selection = 0;
        self.slash_menu_dismissed = false;
        self.reference_selection = 0;
        self.reference_menu_dismissed = false;
    }

    /// Moves the composer cursor up (`direction < 0`) or down (`direction > 0`) one line inside a
    /// multiline draft. Returns `false` when already on the boundary line so callers can fall back
    /// to composer history recall.
    fn move_composer_line(&mut self, direction: isize) -> bool {
        let text = &self.composer.text;
        let cursor = self.composer.cursor.min(text.len());
        let before = text.get(..cursor).unwrap_or_default();
        let line_start = before.rfind('\n').map_or(0, |index| index + 1);
        let column = before.get(line_start..).unwrap_or_default().chars().count();
        if direction < 0 {
            if line_start == 0 {
                return false;
            }
            let prev_end = line_start - 1;
            let prev_start = text
                .get(..prev_end)
                .unwrap_or_default()
                .rfind('\n')
                .map_or(0, |index| index + 1);
            let prev_line = text.get(prev_start..prev_end).unwrap_or_default();
            let offset = prev_line
                .char_indices()
                .nth(column)
                .map_or(prev_line.len(), |(index, _)| index);
            self.composer.cursor = prev_start + offset;
        } else {
            let after = text.get(cursor..).unwrap_or_default();
            let Some(newline) = after.find('\n') else {
                return false;
            };
            let next_start = cursor + newline + 1;
            let next_tail = text.get(next_start..).unwrap_or_default();
            let next_end = next_tail
                .find('\n')
                .map_or(text.len(), |index| next_start + index);
            let next_line = text.get(next_start..next_end).unwrap_or_default();
            let offset = next_line
                .char_indices()
                .nth(column)
                .map_or(next_line.len(), |(index, _)| index);
            self.composer.cursor = next_start + offset;
        }
        true
    }

    #[cfg(test)]
    pub(super) fn submit_input(&mut self, catalog: &UiCatalog) -> UiAction {
        self.submit_input_with_delivery(catalog, None)
    }

    fn submit_input_with_delivery(
        &mut self,
        catalog: &UiCatalog,
        delivery: Option<ActiveMessageDelivery>,
    ) -> UiAction {
        let had_pastes = !self.composer.pastes.is_empty();
        let line = self.expanded_input();
        if line.len() > MAX_MESSAGE_BYTES {
            self.input_limit_reached = true;
            return UiAction::None;
        }
        if self.approval().is_some() {
            let decision = approval_decision(line.trim())
                .or_else(|| approval_choice(self.approval_selection))
                .unwrap_or(ReviewDecision::Abort);
            return self.decide_approval(decision);
        }
        if self.upload_in_progress {
            self.push(
                "wait for attachment uploads to finish before sending",
                TranscriptTone::Warning,
            );
            return UiAction::None;
        }
        let line = if had_pastes {
            line.as_str()
        } else {
            line.trim()
        };
        if !self.composer.attachments.is_empty() && self.composer_target_turn().is_some() {
            self.push(
                "attachments can be sent when the agent is idle",
                TranscriptTone::Warning,
            );
            return UiAction::None;
        }
        let submitted_input = std::mem::take(&mut self.composer.text);
        let submitted_cursor = self.composer.cursor;
        self.composer.pastes.clear();
        self.composer.cursor = 0;
        self.slash_input_changed();
        let status = self.status();
        if !had_pastes
            && self.composer.attachments.is_empty()
            && let Some(action) = catalog.dispatch(
                line,
                CommandContext {
                    active_turn: self.active_turn(),
                    status: &status,
                },
            )
        {
            return match action {
                CommandAction::Ui(UiAction::Submit(op)) => {
                    match (self.capability_popup_for(&op), *op) {
                        (Some((widgets, action)), Op::CapabilityCommand { capability, .. }) => {
                            self.picker = None;
                            self.preview = None;
                            self.capability_overlay =
                                Some(crate::frontend::dashboard::CapabilityOverlay::from_widgets(
                                    capability.to_string(),
                                    widgets,
                                ));
                            UiAction::submit(action)
                        }
                        (_, op) => UiAction::submit(op),
                    }
                }
                CommandAction::Ui(action) => action,
                CommandAction::ShowMenu => {
                    self.push(catalog.menu(), TranscriptTone::Neutral);
                    UiAction::None
                }
                CommandAction::Print(message) => {
                    let is_status = line.split_whitespace().next() == Some("/status");
                    if !is_status {
                        self.composer.text = submitted_input;
                        self.composer.cursor = submitted_cursor;
                    }
                    let tone = if is_status {
                        TranscriptTone::Neutral
                    } else {
                        TranscriptTone::Warning
                    };
                    self.push(message, tone);
                    UiAction::None
                }
                CommandAction::Attention => {
                    let widgets =
                        self.overlay_widgets(|_, widget| widget.slot == FrontendSlot::Attention);
                    self.capability_overlay = Some(
                        crate::frontend::dashboard::CapabilityOverlay::attention(widgets),
                    );
                    self.sync_attention_approvals();
                    UiAction::None
                }
            };
        }
        if line.is_empty() && self.composer.attachments.is_empty() {
            return UiAction::None;
        }
        if self.disconnected {
            self.composer.text = if had_pastes {
                line.to_owned()
            } else {
                submitted_input
            };
            self.composer.cursor = self.composer.text.len();
            self.push(
                "not sent · the gateway is disconnected; your message stays here until it reconnects",
                TranscriptTone::Warning,
            );
            return UiAction::None;
        }
        let op = Op::Message {
            message: MessageSubmission {
                requested_delivery: self.composer_target_turn().and(delivery),
                target_turn_id: self.composer_target_turn().map(str::to_owned),
                ..MessageSubmission::user(line, std::mem::take(&mut self.composer.attachments))
            },
        };
        UiAction::submit(op)
    }

    /// Returns the keyed menu widgets and the menu action for a bare capability command.
    fn capability_popup_for(&self, op: &Op) -> Option<(KeyedWidgets, Op)> {
        let Op::CapabilityCommand {
            capability,
            command,
            arguments,
            input,
            target,
        } = op
        else {
            return None;
        };
        if !arguments.is_empty() || input.is_some() || target.is_some() {
            return None;
        }
        let (_, menu) = self.widgets.iter().find(|(owner, widget)| {
            owner == capability
                && matches!(
                    widget.slot,
                    FrontendSlot::ChatMenu | FrontendSlot::Navigation
                )
                && matches!(
                    widget.action.as_ref(),
                    Some(Op::CapabilityCommand {
                        capability: action_capability,
                        command: action_command,
                        ..
                    }) if action_capability == capability && action_command == command
                )
        })?;
        let action = menu.action.clone()?; // rules-allow: the menu widget keeps its action while the popup submits a copy
        let widgets = self.overlay_widgets(|owner, widget| {
            owner == &**capability
                && matches!(
                    widget.slot,
                    FrontendSlot::ChatMenu | FrontendSlot::Navigation
                )
                && widget.content.is_some()
        });
        Some((widgets, action))
    }

    fn overlay_widgets(&self, include: impl Fn(&str, &FrontendWidget) -> bool) -> KeyedWidgets {
        self.widgets
            .iter()
            .filter(|(owner, item)| include(owner, item))
            .map(|(owner, item)| (Arc::clone(owner), Arc::clone(item)))
            .collect()
    }

    fn status(&self) -> String {
        format!(
            "{} · {}{}",
            if self.active_turn().is_some() {
                "active"
            } else {
                "idle"
            },
            self.usage.label(),
            widget_status(&self.widgets)
        )
    }

    fn accepts_input(&mut self, inserted: usize, removed: usize) -> bool {
        let bytes = self
            .expanded_input_bytes()
            .saturating_sub(removed)
            .saturating_add(inserted);
        self.input_limit_reached = bytes > MAX_MESSAGE_BYTES;
        !self.input_limit_reached
    }

    fn expanded_input_bytes(&self) -> usize {
        self.composer.text.len().saturating_add(
            self.composer
                .pastes
                .iter()
                .map(|(token, paste)| paste.len().saturating_sub(token.len_utf8()))
                .sum::<usize>(),
        )
    }
}

/// The approval card's choices, in order; a digit key picks one directly.
pub(super) const APPROVAL_CHOICES: [&str; 4] = ["Yes", "Yes, for this chat", "No", "Stop the turn"];

fn approval_choice(index: usize) -> Option<ReviewDecision> {
    Some(match index {
        0 => ReviewDecision::Approved,
        1 => ReviewDecision::ApprovedForSession,
        2 => denied_by_user(),
        3 => ReviewDecision::Abort,
        _ => return None,
    })
}

fn denied_by_user() -> ReviewDecision {
    ReviewDecision::Denied {
        rejection: "denied by user".into(),
    }
}

/// A typed answer: a known word decides, other text denies with itself as the reason, and an
/// empty composer defers to the highlighted choice.
fn approval_decision(input: &str) -> Option<ReviewDecision> {
    Some(match input.to_ascii_lowercase().as_str() {
        "" => return None,
        "y" | "yes" => ReviewDecision::Approved,
        "a" | "always" => ReviewDecision::ApprovedForSession,
        "q" | "abort" => ReviewDecision::Abort,
        "n" | "no" => denied_by_user(),
        _ => ReviewDecision::Denied {
            rejection: input.to_owned(),
        },
    })
}

fn previous_boundary(value: &str, cursor: usize) -> usize {
    value
        .get(..cursor)
        .unwrap_or_default()
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn previous_word_boundary(value: &str, cursor: usize) -> usize {
    let mut characters = value.get(..cursor).unwrap_or_default().char_indices().rev();
    let Some((mut start, last)) =
        characters.find(|(_, character)| character.is_control() || !character.is_whitespace())
    else {
        return 0;
    };
    if last.is_control() {
        return start;
    }
    let separator = is_word_separator(last);
    for (index, character) in characters {
        if character.is_whitespace()
            || character.is_control()
            || is_word_separator(character) != separator
        {
            break;
        }
        start = index;
    }
    start
}

fn is_word_separator(character: char) -> bool {
    WORD_SEPARATORS.contains(character)
}

fn next_boundary(value: &str, cursor: usize) -> usize {
    value
        .get(cursor..)
        .unwrap_or_default()
        .char_indices()
        .nth(1)
        .map_or(value.len(), |(index, _)| cursor + index)
}

fn next_word_boundary(value: &str, cursor: usize) -> usize {
    let tail = value.get(cursor..).unwrap_or_default();
    let mut characters = tail.char_indices();
    let Some((first_offset, first)) =
        characters.find(|(_, character)| character.is_control() || !character.is_whitespace())
    else {
        return value.len();
    };
    if first.is_control() {
        return cursor + first_offset + first.len_utf8();
    }
    let separator = is_word_separator(first);
    for (index, character) in characters {
        if character.is_whitespace()
            || character.is_control()
            || is_word_separator(character) != separator
        {
            return cursor + index;
        }
    }
    value.len()
}
