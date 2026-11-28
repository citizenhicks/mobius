//! Gateway-wide settings for routing the first message in a new chat.

use crate::frontend::terminal::{Screen, TextForm, choice_row, next_input_batch};
use crate::frontend::theme::{Role, brand_header, current};
use crate::frontend::{setup, terminal_text, wrap_step};
use mobius::Result;
use mobius::protocol::{FrontendSettingKind, FrontendSettingValue};
use mobius_gateway::client::{GatewayEvents, GatewaySender};
use mobius_gateway::wire::{ClientMessage, ReadyPayload, ServerMessage};
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use std::ops::ControlFlow;
use std::sync::Arc;

pub(in crate::frontend) async fn new_chats(
    terminal: &mut Screen,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
) -> Result<()> {
    let catalog = &gateway.agent_catalog.auto_routing;
    let fields = &catalog.feature.settings;
    let mut values = catalog.values.clone(); // rules-allow: Cancel discards a local edit without mutating the connected catalog
    let mut row = 0;
    let mut editing: Option<TextForm> = None;
    loop {
        terminal.draw(|frame| {
            let area = crate::frontend::terminal::page_canvas(frame, 82);
            let mut lines = vec![brand_header("New chats"), Line::from("")];
            for (index, setting) in fields.iter().enumerate() {
                let (value, _) = setup::view::setting_value(setting, values.get(&setting.id));
                let text = editing
                    .as_ref()
                    .filter(|_| index == row)
                    .map_or_else(|| value.as_ref(), |form| form.value.as_str());
                choice_row(&mut lines, index, &setting.label, text, index == row);
            }
            choice_row(&mut lines, fields.len(), "Save", "", row == fields.len());
            lines.push(Line::from(if editing.is_some() {
                "  enter apply · esc discard field"
            } else {
                "  ↑↓ select · ←→ adjust · enter edit/save · esc cancel"
            }));
            let scroll = setup::view::selection_scroll(&lines, area);
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .scroll((scroll, 0)),
                area,
            );
        })?;
        for event in next_input_batch().await {
            let event = event?;
            if let Some(form) = editing.as_mut() {
                if let Some(accepted) = form.input(event)
                    && let Some(form) = editing.take()
                    && accepted
                    && let Some(field) = fields.get(row)
                {
                    values.insert(field.id.clone(), FrontendSettingValue::String(form.value)); // rules-allow: the edited map owns its setting keys
                }
                continue;
            }
            let Event::Key(key) = event else { continue };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                KeyCode::Up | KeyCode::Char('k') => row = wrap_step(row, -1, fields.len() + 1),
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    row = wrap_step(row, 1, fields.len() + 1);
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Enter => {
                    let Some(field) = fields.get(row) else {
                        if key.code == KeyCode::Enter {
                            return save_preference(sender, events, gateway, |request_id| {
                                ClientMessage::ConfigureAutoRouting { request_id, values }
                            })
                            .await;
                        }
                        continue;
                    };
                    let current_value = values.get(&field.id);
                    if let FrontendSettingKind::Text { max_bytes } = field.kind {
                        if key.code == KeyCode::Enter {
                            let value = match current_value {
                                Some(FrontendSettingValue::String(text)) => text.as_str(),
                                _ => "",
                            };
                            editing = Some(TextForm::new(value, max_bytes));
                        }
                    } else if let Some(value) = setup::step_setting(
                        &field.kind,
                        current_value,
                        if key.code == KeyCode::Left { -1 } else { 1 },
                    ) {
                        values.insert(field.id.clone(), value); // rules-allow: the edited map owns its setting keys
                    } else {
                        values.remove(&field.id);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Shared gateway preference picker used by the chat and dashboard.
pub(in crate::frontend) async fn dictation(
    terminal: &mut Screen,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
) -> Result<()> {
    let catalog = &gateway.agent_catalog.dictation;
    let choices: Vec<_> = catalog
        .choices
        .iter()
        .map(|choice| (choice, choice.display()))
        .collect();
    let mut index = choices
        .iter()
        .position(|(c, _)| c.target == catalog.selection)
        .unwrap_or_default();
    loop {
        terminal.draw(|frame| {
            let mut lines = vec![brand_header("Dictation"), Line::from("")];
            for (row, (choice, label)) in choices.iter().enumerate() {
                let marker = if choice.target == catalog.selection {
                    '●'
                } else {
                    '○'
                };
                lines.push(Line::styled(
                    format!("  {marker} {}", terminal_text(label)),
                    current().style(if row == index {
                        Role::Selection
                    } else {
                        Role::Text
                    }),
                ));
            }
            lines.push(Line::from("  ↑↓ choose · enter save · esc back"));
            let area = crate::frontend::terminal::page_canvas(frame, 82);
            frame.render_widget(Paragraph::new(lines), area);
        })?;
        for event in next_input_batch().await {
            let Event::Key(key) = event? else {
                continue;
            };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') => {
                    index = wrap_step(index, 1, choices.len());
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    index = wrap_step(index, -1, choices.len());
                }
                KeyCode::Enter => {
                    let Some((choice, _)) = choices.get(index) else {
                        continue;
                    };
                    let target = choice.target.clone(); // rules-allow: the outgoing request owns the selected target
                    return save_preference(sender, events, gateway, |request_id| {
                        ClientMessage::ConfigureDictation { request_id, target }
                    })
                    .await;
                }
                _ => {}
            }
        }
    }
}

async fn save_preference(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    message: impl FnOnce(Arc<str>) -> ClientMessage,
) -> Result<()> {
    let payload = crate::frontend::response::request(
        sender,
        events,
        Some(gateway),
        message,
        |id, message| match message {
            ServerMessage::GatewayConfigured {
                request_id,
                payload,
            } if &*request_id == id => ControlFlow::Break(payload),
            other => ControlFlow::Continue(other),
        },
    )
    .await?;
    gateway.update(payload);
    Ok(())
}
