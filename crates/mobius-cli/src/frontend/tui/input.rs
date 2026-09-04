use ratatui::crossterm::event::KeyCode;
use ratatui::crossterm::event::KeyEvent;
use ratatui::crossterm::event::KeyEventKind;
use ratatui::crossterm::event::KeyModifiers;
use ratatui::crossterm::event::MouseEvent;
use ratatui::crossterm::event::MouseEventKind;

use super::PreviewContent;
use super::TranscriptTone;
use super::TuiState;
use super::references::ReferenceToken;
use super::references::active_reference_token;
use super::references::replace_reference_token;
use super::view::widget_status;
use crate::frontend::catalog::CommandAction;
use crate::frontend::catalog::CommandContext;
use crate::frontend::catalog::GatewayAction;
use crate::frontend::catalog::MenuItem;
use crate::frontend::catalog::UiCatalog;
use crate::frontend::dashboard::activate_overlay;
use crate::frontend::dashboard::handle_action_input_key;
use crate::frontend::dashboard::insert_overlay_input;
use crate::frontend::dashboard::move_overlay_action;
use crate::frontend::dashboard::move_overlay_selection;
use crate::frontend::dashboard::prepare_overlay_operation;
use crate::frontend::dashboard::select_overlay_edge;
use crate::frontend::setup::SetupMode;
use crate::frontend::terminal::terminal_text;
use mobius::protocol::ActiveMessageDelivery;
use mobius::protocol::FrontendSlot;
use mobius::protocol::FrontendWidget;
use mobius::protocol::FrontendWidgetContent;
use mobius::protocol::MAX_MESSAGE_BYTES;
use mobius::protocol::MessageAuthor;
use mobius::protocol::MessageSubmission;
use mobius::protocol::Op;
use mobius::protocol::ReviewDecision;
use mobius_gateway::wire::BotRecord;
use std::path::PathBuf;

const COLLAPSED_PASTE_BYTES: usize = 200;
const SCROLL_ROWS: usize = 3;
const WORD_SEPARATORS: &str = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?";

#[derive(Debug, PartialEq)]
pub(super) enum UiAction {
    None,
    PasteClipboard,
    Submit(Op),
    Gateway(GatewayAction),
    GatewaySettings,
    Extensions,
    Bots,
    Setup {
        mode: SetupMode,
        provider: Option<String>,
    },
    Exit,
    ChooseBot {
        workspace: PathBuf,
        clear: bool,
    },
    CreateSession {
        workspace: PathBuf,
        bot_id: String,
        clear: bool,
    },
}

impl TuiState {
    pub(super) fn handle_key(&mut self, key: KeyEvent, catalog: &UiCatalog) -> UiAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return UiAction::None;
        }
        if let Some(action) = self.handle_open_surface_key(key) {
            return action;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return UiAction::Exit;
        }
        if self.picker.is_some() {
            return self.handle_picker_key(key);
        }
        match key.code {
            KeyCode::PageUp => {
                let rows = self.transcript_viewport.page_height();
                self.transcript_viewport.scroll_up(rows);
                return UiAction::None;
            }
            KeyCode::PageDown => {
                let rows = self.transcript_viewport.page_height();
                self.transcript_viewport.scroll_down(rows);
                return UiAction::None;
            }
            _ => {}
        }
        if self.approval.is_some()
            && key.kind == KeyEventKind::Press
            && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
            && matches!(
                key.code,
                KeyCode::Char('y' | 'Y' | 'a' | 'A' | 'n' | 'N' | 'q' | 'Q')
            )
        {
            let KeyCode::Char(choice) = key.code else {
                unreachable!();
            };
            let id = self.approval.take().expect("approval checked");
            self.restore_draft();
            return UiAction::Submit(Op::ExecApproval {
                id,
                decision: approval_decision(&choice.to_string()),
            });
        }
        if key.kind == KeyEventKind::Press
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char(character) if character.eq_ignore_ascii_case(&'v'))
        {
            return UiAction::PasteClipboard;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('d') if self.input.is_empty() => UiAction::Exit,
                KeyCode::Char('t') => {
                    self.open_transcript_preview();
                    UiAction::None
                }
                KeyCode::Char('p') => {
                    if !self.move_menu_up(catalog) && self.approval.is_none() {
                        self.composer_history_up();
                    }
                    UiAction::None
                }
                KeyCode::Char('n') => {
                    if !self.move_menu_down(catalog) && self.approval.is_none() {
                        self.composer_history_down();
                    }
                    UiAction::None
                }
                _ => UiAction::None,
            };
        }
        match key.code {
            KeyCode::Enter if self.complete_reference(catalog) => UiAction::None,
            KeyCode::Enter => {
                let delivery = (self.active_turn.is_some()
                    && key.modifiers.contains(KeyModifiers::ALT))
                .then(|| self.alternate_message_delivery());
                self.submit_selected_slash(catalog, delivery)
                    .unwrap_or_else(|| self.submit_input_with_delivery(catalog, delivery))
            }
            KeyCode::Up => {
                if !self.move_menu_up(catalog) && self.approval.is_none() {
                    self.composer_history_up();
                }
                UiAction::None
            }
            KeyCode::Down => {
                if !self.move_menu_down(catalog) && self.approval.is_none() {
                    self.composer_history_down();
                }
                UiAction::None
            }
            KeyCode::Esc if self.approval.is_some() => {
                let id = self.approval.take().expect("approval checked");
                self.restore_draft();
                UiAction::Submit(Op::ExecApproval {
                    id,
                    decision: ReviewDecision::Abort,
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
            KeyCode::Esc if self.input.is_empty() && self.is_working() => {
                self.active_turn.clone().map_or(UiAction::None, |turn_id| {
                    UiAction::Submit(Op::Interrupt { turn_id })
                })
            }
            KeyCode::Esc => UiAction::None,
            KeyCode::Backspace => {
                if self.input.is_empty() {
                    self.attachments.pop();
                    return UiAction::None;
                }
                let previous = if key.modifiers.contains(KeyModifiers::ALT) {
                    previous_word_boundary(&self.input, self.cursor)
                } else {
                    previous_boundary(&self.input, self.cursor)
                };
                self.input.drain(previous..self.cursor);
                self.cursor = previous;
                self.prune_pastes();
                self.slash_input_changed();
                UiAction::None
            }
            KeyCode::Delete => {
                let next = next_boundary(&self.input, self.cursor);
                self.input.drain(self.cursor..next);
                self.prune_pastes();
                self.slash_input_changed();
                UiAction::None
            }
            KeyCode::Left => {
                self.cursor = previous_boundary(&self.input, self.cursor);
                UiAction::None
            }
            KeyCode::Right => {
                self.cursor = next_boundary(&self.input, self.cursor);
                UiAction::None
            }
            KeyCode::Home => {
                self.cursor = 0;
                UiAction::None
            }
            KeyCode::End => {
                self.cursor = self.input.len();
                UiAction::None
            }
            KeyCode::Tab => {
                if !self.complete_reference(catalog) {
                    self.complete_slash(catalog);
                }
                UiAction::None
            }
            KeyCode::Char('q') if self.disconnected && self.input.is_empty() => UiAction::Exit,
            KeyCode::Char(character) => {
                self.insert_text(&character.to_string());
                UiAction::None
            }
            _ => UiAction::None,
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
                    snapshot.next.clone()
                })
                .map_or(UiAction::None, UiAction::Submit);
        }
        let preview = self.preview.as_mut().expect("preview checked");
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

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
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
        viewport.effective_scroll() != before
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> UiAction {
        let Some(picker) = self.picker.as_mut() else {
            return UiAction::None;
        };
        match key.code {
            KeyCode::Esc => {
                self.picker = None;
                UiAction::None
            }
            KeyCode::Up => {
                if !picker.options.is_empty() {
                    picker.selected =
                        (picker.selected + picker.options.len() - 1) % picker.options.len();
                }
                UiAction::None
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !picker.options.is_empty() {
                    picker.selected =
                        (picker.selected + picker.options.len() - 1) % picker.options.len();
                }
                UiAction::None
            }
            KeyCode::Down => {
                if !picker.options.is_empty() {
                    picker.selected = (picker.selected + 1) % picker.options.len();
                }
                UiAction::None
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !picker.options.is_empty() {
                    picker.selected = (picker.selected + 1) % picker.options.len();
                }
                UiAction::None
            }
            KeyCode::Enter => {
                let action = picker
                    .options
                    .get(picker.selected)
                    .map(|option| option.action.clone());
                self.picker = None;
                action.map_or(UiAction::None, |action| match action {
                    super::PickerAction::Submit(op) => UiAction::Submit(op),
                    super::PickerAction::CreateSession {
                        workspace,
                        bot_id,
                        clear,
                    } => UiAction::CreateSession {
                        workspace,
                        bot_id,
                        clear,
                    },
                })
            }
            _ => UiAction::None,
        }
    }

    pub(super) fn open_bot_picker(
        &mut self,
        bots: &[BotRecord],
        workspace: PathBuf,
        current_bot_id: &str,
        clear: bool,
    ) {
        self.preview = None;
        self.capability_overlay = None;
        self.picker = Some(super::PickerState {
            title: "Select Bot".into(),
            selected: bots
                .iter()
                .position(|bot| bot.id == current_bot_id)
                .unwrap_or_default(),
            options: bots
                .iter()
                .map(|bot| super::PickerOption {
                    label: format!("@{}", terminal_text(&bot.handle)),
                    description: terminal_text(&bot.name),
                    detail: terminal_text(&bot.config.config.provider.model),
                    shows_detail: true,
                    action: super::PickerAction::CreateSession {
                        workspace: workspace.clone(),
                        bot_id: bot.id.clone(),
                        clear,
                    },
                })
                .collect(),
        });
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
        let editing = self
            .capability_overlay
            .as_ref()
            .is_some_and(crate::frontend::dashboard::CapabilityOverlay::is_editing);
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
            || key.code == KeyCode::Char('q') && !editing
        {
            self.capability_overlay = None;
            return UiAction::None;
        }
        if key.code == KeyCode::Esc && !editing {
            if self
                .capability_overlay
                .as_ref()
                .is_some_and(crate::frontend::dashboard::CapabilityOverlay::can_go_back)
            {
                self.capability_overlay
                    .as_mut()
                    .expect("overlay checked")
                    .close_widget();
            } else {
                self.capability_overlay = None;
            }
            return UiAction::None;
        }
        let Some(overlay) = self.capability_overlay.as_mut() else {
            return UiAction::None;
        };
        if overlay.is_editing() {
            return handle_action_input_key(overlay, key).map_or(UiAction::None, UiAction::Submit);
        }
        let action_list_open = matches!(
            overlay
                .open_widget()
                .and_then(|widget| widget.content.as_ref()),
            Some(FrontendWidgetContent::ActionList { .. })
        );
        match key.code {
            KeyCode::Left if action_list_open => move_overlay_action(overlay, -1),
            KeyCode::Up | KeyCode::Char('k') => move_overlay_selection(overlay, -1),
            KeyCode::Down | KeyCode::Char('j') => move_overlay_selection(overlay, 1),
            KeyCode::Home => select_overlay_edge(overlay, false),
            KeyCode::End => select_overlay_edge(overlay, true),
            KeyCode::Right if action_list_open => move_overlay_action(overlay, 1),
            KeyCode::Enter | KeyCode::Right => {
                return activate_overlay(overlay)
                    .and_then(|op| prepare_overlay_operation(overlay, op))
                    .map_or(UiAction::None, UiAction::Submit);
            }
            KeyCode::Char('a') => {
                return overlay
                    .open_widget()
                    .and_then(|widget| widget.action.clone())
                    .and_then(|op| prepare_overlay_operation(overlay, op))
                    .map_or(UiAction::None, UiAction::Submit);
            }
            _ => {}
        }
        UiAction::None
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
        self.input.insert_str(self.cursor, &text);
        self.cursor += text.len();
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
            .find(|token| !self.pastes.contains_key(token) && !self.input.contains(*token))
        else {
            self.insert_text(text);
            return;
        };
        self.input.insert(self.cursor, token);
        self.cursor += token.len_utf8();
        self.pastes.insert(token, text.to_owned());
        self.slash_input_changed();
    }

    pub(super) fn visible_input(&self) -> (String, usize) {
        let mut visible = String::new();
        let mut visible_cursor = 0;
        for (index, character) in self.input.char_indices() {
            if index == self.cursor {
                visible_cursor = visible.len();
            }
            if let Some(paste) = self.pastes.get(&character) {
                visible.push_str(&format!("[pasted content {} chars]", paste.chars().count()));
            } else {
                visible.push(character);
            }
        }
        if self.cursor == self.input.len() {
            visible_cursor = visible.len();
        }
        (visible, visible_cursor)
    }

    fn expanded_input(&self) -> String {
        self.input.chars().fold(
            String::with_capacity(self.expanded_input_bytes()),
            |mut expanded, character| {
                if let Some(paste) = self.pastes.get(&character) {
                    expanded.push_str(paste);
                } else {
                    expanded.push(character);
                }
                expanded
            },
        )
    }

    fn prune_pastes(&mut self) {
        self.pastes.retain(|token, _| self.input.contains(*token));
    }

    fn complete_slash(&mut self, catalog: &UiCatalog) {
        let Some(command) = self.selected_slash_command(catalog) else {
            return;
        };
        let token_end = self
            .input
            .find(char::is_whitespace)
            .unwrap_or(self.input.len());
        if !self.accepts_input(command.len(), token_end) {
            return;
        }
        let tail = &self.input[token_end..];
        self.input = if tail.is_empty() {
            format!("{command} ")
        } else {
            format!("{command}{tail}")
        };
        self.cursor = self.input.len();
        self.slash_input_changed();
    }

    fn submit_selected_slash(
        &mut self,
        catalog: &UiCatalog,
        delivery: Option<ActiveMessageDelivery>,
    ) -> Option<UiAction> {
        let command = self.selected_slash_command(catalog)?;
        let token_end = self
            .input
            .find(char::is_whitespace)
            .unwrap_or(self.input.len());
        if !self.accepts_input(command.len(), token_end) {
            return Some(UiAction::None);
        }
        self.input.replace_range(..token_end, &command);
        self.cursor = self.input.len();
        Some(self.submit_input_with_delivery(catalog, delivery))
    }

    fn selected_slash_command(&self, catalog: &UiCatalog) -> Option<String> {
        self.slash_suggestions(catalog)?
            .get(self.slash_selection)
            .map(|item| item.value.clone())
    }

    fn slash_suggestions(&self, catalog: &UiCatalog) -> Option<Vec<MenuItem>> {
        if self.slash_menu_dismissed {
            return None;
        }
        catalog.command_suggestions(&self.input, self.cursor)
    }

    fn move_slash_up(&mut self, catalog: &UiCatalog) -> bool {
        let Some(items) = self.slash_suggestions(catalog) else {
            return false;
        };
        if !items.is_empty() {
            self.slash_selection = (self.slash_selection + items.len() - 1) % items.len();
        }
        true
    }

    fn move_slash_down(&mut self, catalog: &UiCatalog) -> bool {
        let Some(items) = self.slash_suggestions(catalog) else {
            return false;
        };
        if !items.is_empty() {
            self.slash_selection = (self.slash_selection + 1) % items.len();
        }
        true
    }

    pub(super) fn reference_suggestions(
        &mut self,
        catalog: &UiCatalog,
    ) -> Option<(ReferenceToken, Vec<MenuItem>)> {
        if self.reference_menu_dismissed {
            return None;
        }
        let trigger = catalog
            .reference_triggers()
            .find(|trigger| active_reference_token(&self.input, self.cursor, *trigger).is_some())?;
        let token = active_reference_token(&self.input, self.cursor, trigger)?;
        let matches = if let Some((cached_trigger, cached_query, matches)) = &self.reference_cache
            && *cached_trigger == trigger
            && cached_query == &token.query
        {
            matches.clone()
        } else {
            let matches = catalog.reference_suggestions(trigger, &token.query);
            self.reference_cache = Some((trigger, token.query.clone(), matches.clone()));
            matches
        };
        (!matches.is_empty()).then_some((token, matches))
    }

    fn complete_reference(&mut self, catalog: &UiCatalog) -> bool {
        let Some((token, matches)) = self.reference_suggestions(catalog) else {
            return false;
        };
        let Some(item) = matches.get(self.reference_selection) else {
            return false;
        };
        if !self.accepts_input(item.value.len(), token.range.len()) {
            return true;
        }
        let Some(cursor) = replace_reference_token(&mut self.input, &token, &item.value) else {
            return false;
        };
        self.cursor = cursor;
        self.prune_pastes();
        self.slash_input_changed();
        self.reference_menu_dismissed = true;
        true
    }

    fn move_menu_up(&mut self, catalog: &UiCatalog) -> bool {
        if let Some((_, matches)) = self.reference_suggestions(catalog) {
            self.reference_selection =
                (self.reference_selection + matches.len() - 1) % matches.len();
            true
        } else {
            self.move_slash_up(catalog)
        }
    }

    fn move_menu_down(&mut self, catalog: &UiCatalog) -> bool {
        if let Some((_, matches)) = self.reference_suggestions(catalog) {
            self.reference_selection = (self.reference_selection + 1) % matches.len();
            true
        } else {
            self.move_slash_down(catalog)
        }
    }

    fn slash_input_changed(&mut self) {
        self.input_limit_reached = false;
        self.slash_selection = 0;
        self.slash_menu_dismissed = false;
        self.reference_selection = 0;
        self.reference_menu_dismissed = false;
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
        let had_pastes = !self.pastes.is_empty();
        let line = self.expanded_input();
        if line.len() > MAX_MESSAGE_BYTES {
            self.input_limit_reached = true;
            return UiAction::None;
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
        if !self.attachments.is_empty() && self.active_turn.is_some() {
            self.push(
                "attachments can be sent when the agent is idle",
                TranscriptTone::Warning,
            );
            return UiAction::None;
        }
        self.input.clear();
        self.pastes.clear();
        self.cursor = 0;
        self.slash_input_changed();
        let status = self.status();
        if !had_pastes
            && self.attachments.is_empty()
            && let Some(action) = catalog.dispatch(
                line,
                CommandContext {
                    active_turn: self.active_turn.as_deref(),
                    status: &status,
                },
            )
        {
            return match action {
                CommandAction::Submit(op) => {
                    if matches!(op, Op::Interrupt { .. }) {
                        self.clear_approval();
                    }
                    if let Some((capability, widgets, action)) = self.capability_popup_for(&op) {
                        self.picker = None;
                        self.preview = None;
                        self.capability_overlay =
                            Some(crate::frontend::dashboard::CapabilityOverlay::from_widgets(
                                capability, widgets,
                            ));
                        return UiAction::Submit(action);
                    }
                    UiAction::Submit(op)
                }
                CommandAction::Gateway(action) => UiAction::Gateway(action),
                CommandAction::GatewaySettings => UiAction::GatewaySettings,
                CommandAction::Extensions => UiAction::Extensions,
                CommandAction::Bots => UiAction::Bots,
                CommandAction::Setup { mode, provider } => UiAction::Setup { mode, provider },
                CommandAction::ShowMenu => {
                    self.push(catalog.menu(), TranscriptTone::Neutral);
                    UiAction::None
                }
                CommandAction::Print(message) => {
                    self.push(message, TranscriptTone::Warning);
                    UiAction::None
                }
                CommandAction::Exit => UiAction::Exit,
                CommandAction::ChooseBot { workspace, clear } => {
                    UiAction::ChooseBot { workspace, clear }
                }
            };
        }
        if let Some(id) = self.approval.take() {
            let decision = approval_decision(line);
            self.restore_draft();
            return UiAction::Submit(Op::ExecApproval { id, decision });
        }
        if line.is_empty() && self.attachments.is_empty() {
            return UiAction::None;
        }
        let op = Op::Message {
            message: MessageSubmission {
                author: MessageAuthor::User,
                text: line.into(),
                attachments: std::mem::take(&mut self.attachments),
                reply: None,
                requested_delivery: self.active_turn.as_ref().and(delivery),
                target_turn_id: self.active_turn.clone(),
            },
        };
        UiAction::Submit(op)
    }

    fn capability_popup_for(&self, op: &Op) -> Option<(String, Vec<FrontendWidget>, Op)> {
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
        let (_, menu) = self.widgets.iter().find(|((owner, _), widget)| {
            owner == capability
                && widget.slot == FrontendSlot::ChatMenu
                && matches!(
                    widget.action.as_ref(),
                    Some(Op::CapabilityCommand {
                        capability: action_capability,
                        command: action_command,
                        ..
                    }) if action_capability == capability && action_command == command
                )
        })?;
        let action = menu.action.clone()?;
        let widgets = self
            .widgets
            .iter()
            .filter(|((owner, _), widget)| {
                owner == capability
                    && matches!(
                        widget.slot,
                        FrontendSlot::ChatMenu | FrontendSlot::Navigation
                    )
                    && widget.content.is_some()
            })
            .map(|(_, widget)| widget.clone())
            .collect();
        Some((capability.clone(), widgets, action))
    }

    fn status(&self) -> String {
        format!(
            "{} · {}{}",
            if self.active_turn.is_some() {
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
        self.input.len().saturating_add(
            self.pastes
                .iter()
                .map(|(token, paste)| paste.len().saturating_sub(token.len_utf8()))
                .sum::<usize>(),
        )
    }
}

fn approval_decision(input: &str) -> ReviewDecision {
    match input.to_ascii_lowercase().as_str() {
        "y" | "yes" => ReviewDecision::Approved,
        "a" | "always" => ReviewDecision::ApprovedForSession,
        "q" | "abort" => ReviewDecision::Abort,
        "" | "n" | "no" => ReviewDecision::Denied {
            rejection: "denied by user".into(),
        },
        _ => ReviewDecision::Denied {
            rejection: input.to_string(),
        },
    }
}

fn previous_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn previous_word_boundary(value: &str, cursor: usize) -> usize {
    let mut characters = value[..cursor].char_indices().rev();
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
    value[cursor..]
        .char_indices()
        .nth(1)
        .map_or(value.len(), |(index, _)| cursor + index)
}
