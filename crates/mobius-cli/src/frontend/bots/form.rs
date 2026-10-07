use std::path::PathBuf;

use mobius::protocol::MAX_MESSAGE_BYTES;
use mobius_gateway::bots::{MAX_BOT_DESCRIPTION_BYTES, MAX_BOT_NAME_BYTES};
use mobius_gateway::wire::{
    AgentComposition, BotRecord, ClientMessage, HookSelector, ReadyPayload, Routine, RoutineAction,
    RoutineBinding, RoutineCommand, RoutineDefinition, RoutineSchedule, RoutineScheduleKind,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{Action, FollowUp, moved, request_action};
use crate::frontend::terminal_text;

pub(super) enum Form {
    Bot(Box<BotForm>),
    Routine(Box<RoutineForm>),
}

pub(super) enum FormFlow {
    Stay,
    Cancel,
    Send(Action),
}

pub(super) struct TextForm {
    pub(super) value: String,
    pub(super) limit: usize,
    pub(super) multiline: bool,
    pub(super) error: Option<String>,
}

impl TextForm {
    pub(super) fn new(value: impl Into<String>, limit: usize) -> Self {
        Self {
            value: value.into(),
            limit,
            multiline: false,
            error: None,
        }
    }

    fn multiline(value: impl Into<String>, limit: usize) -> Self {
        Self {
            value: value.into(),
            limit,
            multiline: true,
            error: None,
        }
    }

    fn push(&mut self, value: &str) {
        self.error = (!push_bounded(&mut self.value, value, self.limit, self.multiline))
            .then(|| format!("input is limited to {} bytes", self.limit));
    }

    fn backspace(&mut self) {
        self.value.pop();
        self.error = None;
    }
}

pub(super) enum BotFormMode {
    Create,
    Update { id: String, revision: u64 },
}

pub(super) struct BotForm {
    pub(super) mode: BotFormMode,
    pub(super) name: TextForm,
    pub(super) description: TextForm,
    pub(super) prompt: Option<TextForm>,
    pub(super) row: usize,
    pub(super) error: Option<String>,
}

impl BotForm {
    pub(super) fn create() -> Self {
        Self {
            mode: BotFormMode::Create,
            name: TextForm::new("", MAX_BOT_NAME_BYTES),
            description: TextForm::new("", MAX_BOT_DESCRIPTION_BYTES),
            prompt: None,
            row: 0,
            error: None,
        }
    }

    pub(super) fn update(bot: &BotRecord) -> Self {
        Self {
            mode: BotFormMode::Update {
                id: bot.id.clone(),
                revision: bot.config.revision,
            },
            name: TextForm::new(&bot.name, MAX_BOT_NAME_BYTES),
            description: TextForm::new(&bot.description, MAX_BOT_DESCRIPTION_BYTES),
            prompt: Some(TextForm::multiline(
                &bot.config.config.system_prompt,
                64 * 1024,
            )),
            row: 0,
            error: None,
        }
    }
}

pub(super) enum RoutineFormMode {
    Create(String),
    Update(String),
}

pub(super) struct RoutineForm {
    pub(super) mode: RoutineFormMode,
    pub(super) workspace: TextForm,
    pub(super) instructions: TextForm,
    pub(super) schedule_kind: RoutineScheduleKind,
    pub(super) schedule_value: TextForm,
    pub(super) time_zone: TextForm,
    pub(super) ends_at: TextForm,
    pub(super) schedule_enabled: bool,
    bindings: Vec<RoutineBinding>,
    timer_binding_id: String,
    pub(super) row: usize,
    pub(super) error: Option<String>,
}

impl RoutineForm {
    pub(super) fn is_update(&self) -> bool {
        matches!(self.mode, RoutineFormMode::Update(_))
    }

    pub(super) fn create(bot_id: String) -> Self {
        Self {
            mode: RoutineFormMode::Create(bot_id),
            workspace: TextForm::new("", MAX_MESSAGE_BYTES),
            instructions: TextForm::multiline("", MAX_MESSAGE_BYTES),
            schedule_kind: RoutineScheduleKind::Interval,
            schedule_value: TextForm::new("3600", MAX_MESSAGE_BYTES),
            time_zone: TextForm::new("UTC", MAX_MESSAGE_BYTES),
            ends_at: TextForm::new("", MAX_MESSAGE_BYTES),
            schedule_enabled: true,
            bindings: Vec::new(),
            timer_binding_id: uuid::Uuid::new_v4().to_string(),
            row: 0,
            error: None,
        }
    }

    pub(super) fn update(routine: &Routine) -> Self {
        let timer = routine.bindings.iter().find(|binding| {
            matches!(binding.on, HookSelector::Schedule { .. })
                && binding.action == RoutineAction::Start
        });
        let default_schedule = RoutineSchedule {
            kind: RoutineScheduleKind::Interval,
            at: None,
            every_seconds: Some(3600),
            expression: None,
            time_zone: None,
        };
        let (schedule, ends_at) = timer
            .and_then(|binding| {
                if let HookSelector::Schedule { schedule, ends_at } = &binding.on {
                    Some((schedule, *ends_at))
                } else {
                    None
                }
            })
            .unwrap_or((&default_schedule, None));
        let schedule_value = match schedule.kind {
            RoutineScheduleKind::Once => schedule.at.map(|value| value.to_string()),
            RoutineScheduleKind::Interval => schedule.every_seconds.map(|value| value.to_string()),
            RoutineScheduleKind::Cron => schedule.expression.clone(),
        }
        .unwrap_or_default();
        Self {
            mode: RoutineFormMode::Update(routine.id.clone()),
            workspace: TextForm::new(routine.workspace.display().to_string(), MAX_MESSAGE_BYTES),
            instructions: TextForm::multiline(&routine.instructions, MAX_MESSAGE_BYTES),
            schedule_kind: schedule.kind,
            schedule_value: TextForm::new(schedule_value, MAX_MESSAGE_BYTES),
            time_zone: TextForm::new(
                schedule.time_zone.as_deref().unwrap_or("UTC"),
                MAX_MESSAGE_BYTES,
            ),
            ends_at: TextForm::new(
                ends_at.map(|value| value.to_string()).unwrap_or_default(),
                MAX_MESSAGE_BYTES,
            ),
            schedule_enabled: timer.is_some(),
            bindings: routine.bindings.clone(),
            timer_binding_id: timer.map_or_else(
                || uuid::Uuid::new_v4().to_string(),
                |binding| binding.id.clone(),
            ),
            row: 0,
            error: None,
        }
    }
}
impl Form {
    pub(super) fn handle_key(&mut self, key: KeyEvent, gateway: &ReadyPayload) -> FormFlow {
        if key.code == KeyCode::Esc {
            return FormFlow::Cancel;
        }
        match self {
            Self::Bot(form) => form.handle_key(key, gateway),
            Self::Routine(form) => form.handle_key(key),
        }
    }

    pub(super) fn paste(&mut self, value: &str) {
        match self {
            Self::Bot(form) => {
                if let Some(field) = form.selected_text_field() {
                    field.push(value);
                }
            }
            Self::Routine(form) => form.paste(value),
        }
    }
}

impl BotForm {
    pub(super) fn save_row(&self) -> usize {
        2 + usize::from(self.prompt.is_some())
    }

    fn selected_text_field(&mut self) -> Option<&mut TextForm> {
        match self.row {
            0 => Some(&mut self.name),
            1 => Some(&mut self.description),
            2 => self.prompt.as_mut(),
            _ => None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent, gateway: &ReadyPayload) -> FormFlow {
        match key.code {
            KeyCode::Up | KeyCode::BackTab => self.row = moved(self.row, self.save_row() + 1, -1),
            KeyCode::Down | KeyCode::Tab => self.row = moved(self.row, self.save_row() + 1, 1),
            KeyCode::Enter if self.row == 2 && key.modifiers.contains(KeyModifiers::SHIFT) => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.push("\n");
                }
            }
            KeyCode::Enter if self.row < self.save_row() => self.row += 1,
            KeyCode::Enter => return self.submit(gateway),
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return self.submit(gateway);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(field) = self.selected_text_field() {
                    field.value.clear();
                    field.error = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(field) = self.selected_text_field() {
                    field.backspace();
                }
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(field) = self.selected_text_field() {
                    field.push(&character.to_string());
                }
            }
            _ => {}
        }
        self.error = None;
        FormFlow::Stay
    }

    pub(super) fn submit(&mut self, gateway: &ReadyPayload) -> FormFlow {
        let name = self.name.value.trim().to_owned();
        let description = self.description.value.trim().to_owned();
        if name.is_empty() || description.is_empty() {
            self.error = Some("Bot name and description are required.".into());
            return FormFlow::Stay;
        }
        match &mut self.mode {
            BotFormMode::Create => {
                FormFlow::Send(request_action("Create Bot", FollowUp::None, |request_id| {
                    ClientMessage::CreateBot {
                        request_id,
                        name,
                        description,
                    }
                }))
            }
            BotFormMode::Update { id, revision } => {
                let Some(bot) = gateway.bots.iter().find(|bot| bot.id == *id) else {
                    self.error = Some("The selected Bot is no longer available.".into());
                    return FormFlow::Stay;
                };
                let current = &bot.config.config;
                let config = AgentComposition {
                    provider: current.provider.clone(),
                    realtime_voice: current.realtime_voice.clone(),
                    middleware: current.middleware.clone(),
                    extensions: current.extensions.clone(),
                    system_prompt: if let Some(prompt) = &mut self.prompt {
                        std::mem::take(&mut prompt.value)
                    } else {
                        current.system_prompt.clone()
                    },
                    max_model_steps: current.max_model_steps,
                };
                FormFlow::Send(request_action(
                    "Update Bot identity and prompt",
                    FollowUp::None,
                    |request_id| ClientMessage::UpdateBot {
                        request_id,
                        id: std::mem::take(id),
                        expected_revision: *revision,
                        name,
                        description,
                        tint: bot.tint,
                        shape: bot.shape,
                        config,
                    },
                ))
            }
        }
    }
}

impl RoutineForm {
    fn row_count(&self) -> usize {
        7
    }

    pub(super) fn save_row(&self) -> usize {
        self.row_count() - 1
    }

    fn handle_key(&mut self, key: KeyEvent) -> FormFlow {
        let length = self.row_count();
        match key.code {
            KeyCode::Up | KeyCode::BackTab => self.row = moved(self.row, length, -1),
            KeyCode::Down | KeyCode::Tab => self.row = moved(self.row, length, 1),
            KeyCode::Left if self.row == 2 => self.change_schedule(-1),
            KeyCode::Right if self.row == 2 => self.change_schedule(1),
            KeyCode::Char(' ') if self.row == 2 => self.change_schedule(1),
            KeyCode::Enter if self.row == self.save_row() => return self.submit(),
            KeyCode::Enter if self.row == 2 => self.change_schedule(1),
            KeyCode::Enter => self.row += 1,
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return self.submit();
            }
            KeyCode::Backspace => {
                if let Some(field) = self.selected_text_field() {
                    field.backspace();
                }
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(field) = self.selected_text_field() {
                    field.push(&character.to_string());
                }
            }
            _ => {}
        }
        self.error = None;
        FormFlow::Stay
    }

    fn paste(&mut self, value: &str) {
        if let Some(field) = self.selected_text_field() {
            field.push(value);
        }
    }

    fn selected_text_field(&mut self) -> Option<&mut TextForm> {
        if matches!(self.row, 3..=5) {
            self.schedule_enabled = true;
        }
        match self.row {
            0 => Some(&mut self.workspace),
            1 => Some(&mut self.instructions),
            3 => Some(&mut self.schedule_value),
            4 => Some(&mut self.time_zone),
            5 => Some(&mut self.ends_at),
            _ => None,
        }
    }

    fn change_schedule(&mut self, delta: isize) {
        self.schedule_enabled = true;
        self.schedule_kind = match (schedule_index(self.schedule_kind) + delta).rem_euclid(3) {
            0 => RoutineScheduleKind::Once,
            1 => RoutineScheduleKind::Interval,
            _ => RoutineScheduleKind::Cron,
        };
        self.schedule_value.value = match self.schedule_kind {
            RoutineScheduleKind::Once => String::new(),
            RoutineScheduleKind::Interval => "3600".into(),
            RoutineScheduleKind::Cron => "* * * * *".into(),
        };
    }

    fn submit(&mut self) -> FormFlow {
        match self.action() {
            Ok(action) => FormFlow::Send(action),
            Err(message) => {
                self.error = Some(message);
                FormFlow::Stay
            }
        }
    }

    pub(super) fn action(&mut self) -> std::result::Result<Action, String> {
        let workspace = self.workspace.value.trim();
        if workspace.is_empty() {
            return Err("Routine workspace is required.".into());
        }
        if self.instructions.value.trim().is_empty() {
            return Err("Routine instructions are required.".into());
        }
        let timer = if self.schedule_enabled {
            let schedule = self.schedule()?;
            let ends_at = optional_i64(&self.ends_at.value, "end time")?;
            if ends_at.is_some_and(|value| value <= 0) {
                return Err("Routine end time must be a positive Unix timestamp.".into());
            }
            Some((schedule, ends_at))
        } else {
            None
        };
        // Validation above preserves the editable form on failure; a successful submission consumes it.
        let mut bindings = std::mem::take(&mut self.bindings);
        if let Some((schedule, ends_at)) = timer {
            let binding = RoutineBinding {
                id: std::mem::take(&mut self.timer_binding_id),
                on: HookSelector::Schedule { schedule, ends_at },
                action: RoutineAction::Start,
            };
            if let Some(existing) = bindings
                .iter_mut()
                .find(|existing| existing.id == binding.id)
            {
                *existing = binding;
            } else {
                bindings.push(binding);
            }
        }
        let definition = RoutineDefinition {
            workspace: PathBuf::from(workspace),
            instructions: std::mem::take(&mut self.instructions.value),
            bindings,
        };
        match &mut self.mode {
            RoutineFormMode::Create(bot_id) => Ok(request_action(
                "Create routine",
                FollowUp::Routines,
                |request_id| ClientMessage::CreateRoutine {
                    request_id,
                    bot_id: std::mem::take(bot_id),
                    definition,
                },
            )),
            RoutineFormMode::Update(id) => Ok(request_action(
                "Update routine",
                FollowUp::Routines,
                |request_id| ClientMessage::RoutineCommand {
                    request_id,
                    command: RoutineCommand {
                        routine_id: std::mem::take(id),
                        action: RoutineAction::Update { definition },
                    },
                },
            )),
        }
    }

    fn schedule(&self) -> std::result::Result<RoutineSchedule, String> {
        let value = self.schedule_value.value.trim();
        match self.schedule_kind {
            RoutineScheduleKind::Once => Ok(RoutineSchedule {
                kind: self.schedule_kind,
                at: Some(parse_i64(value, "run time")?),
                every_seconds: None,
                expression: None,
                time_zone: None,
            }),
            RoutineScheduleKind::Interval => {
                let seconds = value
                    .parse::<u64>()
                    .map_err(|_| "Interval must be a whole number of seconds.".to_owned())?;
                if seconds < 60 {
                    return Err("Interval must be at least 60 seconds.".into());
                }
                Ok(RoutineSchedule {
                    kind: self.schedule_kind,
                    at: None,
                    every_seconds: Some(seconds),
                    expression: None,
                    time_zone: None,
                })
            }
            RoutineScheduleKind::Cron => {
                let time_zone = self.time_zone.value.trim();
                if value.is_empty() || time_zone.is_empty() {
                    return Err("Cron expression and time zone are required.".into());
                }
                Ok(RoutineSchedule {
                    kind: self.schedule_kind,
                    at: None,
                    every_seconds: None,
                    expression: Some(value.into()),
                    time_zone: Some(time_zone.into()),
                })
            }
        }
    }
}
const fn schedule_index(kind: RoutineScheduleKind) -> isize {
    match kind {
        RoutineScheduleKind::Once => 0,
        RoutineScheduleKind::Interval => 1,
        RoutineScheduleKind::Cron => 2,
    }
}

fn optional_i64(value: &str, label: &str) -> std::result::Result<Option<i64>, String> {
    let value = value.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        parse_i64(value, label).map(Some)
    }
}

fn parse_i64(value: &str, label: &str) -> std::result::Result<i64, String> {
    value
        .parse()
        .map_err(|_| format!("Routine {label} must be a Unix timestamp."))
}
fn push_bounded(target: &mut String, value: &str, limit: usize, multiline: bool) -> bool {
    let value = terminal_text(value);
    for character in value
        .chars()
        .filter(|character| multiline || !matches!(character, '\n' | '\t'))
    {
        if target.len() + character.len_utf8() > limit {
            return false;
        }
        target.push(character);
    }
    true
}
