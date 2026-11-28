//! Edit the gateway's group and model fields using their advertised input types.
use super::state::{SetupState, step_setting};
use super::view::{choice, selection_scroll, setting_value};
use crate::frontend::terminal::{Screen, TextForm, next_input_batch, page_canvas};
use crate::frontend::theme::brand_header;
use crate::frontend::wrap_step;
use mobius::Result;
use mobius::protocol::{FrontendSettingKind, FrontendSettingValue};
use mobius_gateway::wire::{
    ProviderField, ProviderGroupDraft, ProviderModelDraft, ProviderModelGroup,
};
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use std::collections::BTreeMap;

enum Row<'a> {
    Field(Option<usize>, &'a ProviderField),
    Model(usize),
    Add,
}

fn rows_for<'a>(group: &'a ProviderModelGroup, draft: &ProviderGroupDraft) -> Vec<Row<'a>> {
    let mut rows: Vec<_> = group.fields.iter().map(|f| Row::Field(None, f)).collect();
    for (index, model) in draft.models.iter().enumerate() {
        rows.push(Row::Model(index));
        let fields = group
            .models
            .iter()
            .chain(&group.model_presets)
            .find(|m| m.id == model.id)
            .map_or(&group.new_model_fields, |preset| &preset.fields);
        rows.extend(fields.iter().map(|f| Row::Field(Some(index), f)));
    }
    if group.editable {
        rows.push(Row::Add);
    }
    rows
}

fn value<'a>(
    group: &ProviderModelGroup,
    draft: &'a ProviderGroupDraft,
    model: Option<usize>,
    field: &'a ProviderField,
) -> &'a FrontendSettingValue {
    if let Some(value) = draft.display_override(group, field) {
        return value;
    }
    let values = model.map_or(Some(&draft.values), |i| {
        draft.models.get(i).map(|m| &m.values)
    });
    values
        .and_then(|v| v.get(&field.setting.id))
        .unwrap_or(&field.value)
}

fn update(
    draft: &mut ProviderGroupDraft,
    model: Option<usize>,
    field: &ProviderField,
    value: FrontendSettingValue,
) {
    let values = match model {
        Some(i) => {
            let Some(m) = draft.models.get_mut(i) else {
                return;
            };
            &mut m.values
        }
        None => &mut draft.values,
    };
    if let FrontendSettingValue::String(s) = &value
        && let Some(updates) = field.on_change.get(s)
    {
        values.extend(updates.clone()); // rules-allow: the editable draft owns each advertised dependent field
    }
    values.insert(field.setting.id.clone(), value); // rules-allow: each editable value owns the advertised field ID
}

async fn text_input(
    terminal: &mut Screen,
    title: &str,
    value: &str,
    limit: usize,
) -> Result<Option<String>> {
    let mut text = TextForm::new(value, limit);
    loop {
        terminal.draw(|frame| {
            let area = page_canvas(frame, 82);
            frame.render_widget(
                Paragraph::new(vec![
                    brand_header(title),
                    Line::from(text.value.as_str()),
                    Line::from("  enter apply · ctrl+u clear · esc cancel"),
                ]),
                area,
            );
        })?;
        for event in next_input_batch().await {
            if let Some(accepted) = text.input(event?) {
                return Ok(accepted.then_some(text.value));
            }
        }
    }
}

fn render(
    frame: &mut ratatui::Frame<'_>,
    group: &ProviderModelGroup,
    draft: &ProviderGroupDraft,
    rows: &[Row<'_>],
    focus: usize,
    message: Option<&'static str>,
) {
    let area = page_canvas(frame, 82);
    let mut lines = vec![brand_header(&group.label), Line::from("")];
    for (i, row) in rows.iter().enumerate() {
        let selected = focus == i;
        match row {
            Row::Field(index, field) => {
                let (display, _) =
                    setting_value(&field.setting, Some(value(group, draft, *index, field)));
                choice(
                    &mut lines,
                    &field.setting.label,
                    &display,
                    selected,
                    if field.editable { "" } else { "·" },
                );
            }
            Row::Model(index) => {
                if let Some(m) = draft.models.get(*index) {
                    choice(&mut lines, &m.id, "Model", selected, "");
                }
            }
            Row::Add => choice(&mut lines, "Add model", "", selected, "+"),
        }
    }
    if let Some(message) = message {
        lines.push(Line::from(message));
    }
    lines.push(Line::from(
        "  ↑↓ move · enter edit · ←→ select · delete remove model · esc done",
    ));
    let scroll = selection_scroll(&lines, area);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
}

async fn edit_value(
    terminal: &mut Screen,
    field: &ProviderField,
    current: &FrontendSettingValue,
    key: KeyCode,
    message: &mut Option<&'static str>,
) -> Result<Option<FrontendSettingValue>> {
    Ok(match (&field.setting.kind, key) {
        (FrontendSettingKind::Text { max_bytes }, KeyCode::Enter) => {
            let FrontendSettingValue::String(s) = current else {
                return Ok(None);
            };
            text_input(terminal, &field.setting.label, s, *max_bytes)
                .await?
                .map(FrontendSettingValue::String)
        }
        (FrontendSettingKind::Integer { .. }, KeyCode::Enter) => {
            let Some(input) = text_input(
                terminal,
                &field.setting.label,
                &setting_value(&field.setting, Some(current)).0,
                20,
            )
            .await?
            else {
                return Ok(None);
            };
            if let Ok(number) = input.parse() {
                Some(FrontendSettingValue::Integer(number))
            } else {
                *message = Some("Enter a whole number.");
                None
            }
        }
        _ => step_setting(
            &field.setting.kind,
            Some(current),
            if key == KeyCode::Left { -1 } else { 1 },
        ),
    })
}
pub(super) async fn edit(terminal: &mut Screen, state: &mut SetupState) -> Result<()> {
    let Some(kind) = state.active_group().map(|g| g.kind) else {
        return Ok(());
    };
    let previous = state.drafts.remove(&kind);
    let Some(group) = state.active_group() else {
        return Ok(());
    };
    let mut dirty = previous.is_some();
    let mut draft = previous.unwrap_or_else(|| ProviderGroupDraft {
        models: group
            .models
            .iter()
            .map(|m| ProviderModelDraft {
                id: m.id.clone(), // rules-allow: the editable draft owns the model IDs
                values: BTreeMap::new(),
            })
            .collect(),
        values: BTreeMap::new(),
    });
    let mut focus = 0;
    let mut message = None;
    'edit: loop {
        let rows = rows_for(group, &draft);
        focus = focus.min(rows.len().saturating_sub(1));
        terminal.draw(|frame| render(frame, group, &draft, &rows, focus, message))?;
        for event in next_input_batch().await {
            let Event::Key(key) = event? else { continue };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            message = None;
            let rows = rows_for(group, &draft);
            match key.code {
                KeyCode::Esc => break 'edit,
                KeyCode::Up | KeyCode::Down => {
                    focus = wrap_step(
                        focus,
                        if key.code == KeyCode::Up { -1 } else { 1 },
                        rows.len().max(1),
                    );
                }
                KeyCode::Delete if group.editable => {
                    if let Some(Row::Model(index)) = rows.get(focus) {
                        draft.models.remove(*index);
                        dirty = true;
                    }
                }
                KeyCode::Enter if matches!(rows.get(focus), Some(Row::Add)) => {
                    if let Some(id) = text_input(terminal, "Model ID", "", 256).await?
                        && !id.is_empty()
                    {
                        if draft.models.iter().any(|m| m.id == id) {
                            message = Some("This model already exists.");
                        } else {
                            dirty = true;
                            draft.models.push(ProviderModelDraft {
                                id,
                                values: BTreeMap::new(),
                            });
                        }
                    }
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Left => {
                    let Some(Row::Field(index, field)) = rows.get(focus) else {
                        continue;
                    };
                    if !field.editable {
                        continue;
                    }
                    let current = value(group, &draft, *index, field);
                    let next = edit_value(terminal, field, current, key.code, &mut message).await?;
                    if let Some(v) = next
                        && &v != current
                    {
                        dirty = true;
                        update(&mut draft, *index, field, v);
                    }
                }
                _ => {}
            }
        }
    }
    if dirty {
        state.drafts.insert(kind, draft);
        state.test_result = None;
    }
    Ok(())
}
