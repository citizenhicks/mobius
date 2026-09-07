use std::collections::BTreeSet;
use std::path::PathBuf;

use mobius::protocol::MAX_MESSAGE_BYTES;
use mobius_gateway::wire::{
    BotRecord, ClientMessage, ReadyPayload, Routine, RoutineSchedule, RoutineScheduleKind,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{Action, FollowUp, moved, request_action};
use crate::frontend::terminal_text;

const MAX_BOT_NAME_BYTES: usize = 128;
const MAX_BOT_DESCRIPTION_BYTES: usize = 2 * 1024;
pub(super) const MAX_SWARM_TITLE_BYTES: usize = 256;

pub(super) enum Form {
    Bot(BotForm),
    CreateSwarm(CreateSwarmForm),
    RenameSwarm { swarm_id: String, title: TextForm },
    AddSwarmMember(AddMemberForm),
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
    Update(String),
}

pub(super) struct BotForm {
    pub(super) mode: BotFormMode,
    pub(super) name: TextForm,
    pub(super) description: TextForm,
    pub(super) row: usize,
    pub(super) error: Option<String>,
}

impl BotForm {
    pub(super) fn create() -> Self {
        Self {
            mode: BotFormMode::Create,
            name: TextForm::new("", MAX_BOT_NAME_BYTES),
            description: TextForm::new("", MAX_BOT_DESCRIPTION_BYTES),
            row: 0,
            error: None,
        }
    }

    pub(super) fn update(bot: &BotRecord) -> Self {
        Self {
            mode: BotFormMode::Update(bot.id.clone()),
            name: TextForm::new(&bot.name, MAX_BOT_NAME_BYTES),
            description: TextForm::new(&bot.description, MAX_BOT_DESCRIPTION_BYTES),
            row: 0,
            error: None,
        }
    }
}

pub(super) struct CreateSwarmForm {
    pub(super) title: TextForm,
    pub(super) bot_ids: Vec<String>,
    pub(super) members: BTreeSet<String>,
    pub(super) leader_bot_id: Option<String>,
    pub(super) row: usize,
    pub(super) error: Option<String>,
}

pub(super) struct AddMemberForm {
    pub(super) swarm_id: String,
    pub(super) bot_ids: Vec<String>,
    pub(super) row: usize,
}

pub(super) enum RoutineFormMode {
    Create(String),
    Update(String),
}

pub(super) struct RoutineForm {
    pub(super) mode: RoutineFormMode,
    pub(super) bot_id: String,
    pub(super) workspace: TextForm,
    pub(super) instructions: TextForm,
    pub(super) schedule_kind: RoutineScheduleKind,
    pub(super) schedule_value: TextForm,
    pub(super) time_zone: TextForm,
    pub(super) ends_at: TextForm,
    pub(super) enabled: bool,
    pub(super) row: usize,
    pub(super) error: Option<String>,
}

impl RoutineForm {
    pub(super) fn create(bot_id: String) -> Self {
        Self {
            mode: RoutineFormMode::Create(bot_id.clone()),
            bot_id,
            workspace: TextForm::new("", MAX_MESSAGE_BYTES),
            instructions: TextForm::multiline("", MAX_MESSAGE_BYTES),
            schedule_kind: RoutineScheduleKind::Interval,
            schedule_value: TextForm::new("3600", MAX_MESSAGE_BYTES),
            time_zone: TextForm::new("UTC", MAX_MESSAGE_BYTES),
            ends_at: TextForm::new("", MAX_MESSAGE_BYTES),
            enabled: true,
            row: 0,
            error: None,
        }
    }

    pub(super) fn update(routine: &Routine) -> Self {
        let schedule_value = match routine.schedule.kind {
            RoutineScheduleKind::Once => routine.schedule.at.map(|value| value.to_string()),
            RoutineScheduleKind::Interval => routine
                .schedule
                .every_seconds
                .map(|value| value.to_string()),
            RoutineScheduleKind::Cron => routine.schedule.expression.clone(),
        }
        .unwrap_or_default();
        Self {
            mode: RoutineFormMode::Update(routine.id.clone()),
            bot_id: routine.bot_id.clone(),
            workspace: TextForm::new(routine.workspace.display().to_string(), MAX_MESSAGE_BYTES),
            instructions: TextForm::multiline(&routine.instructions, MAX_MESSAGE_BYTES),
            schedule_kind: routine.schedule.kind,
            schedule_value: TextForm::new(schedule_value, MAX_MESSAGE_BYTES),
            time_zone: TextForm::new(
                routine.schedule.time_zone.as_deref().unwrap_or("UTC"),
                MAX_MESSAGE_BYTES,
            ),
            ends_at: TextForm::new(
                routine
                    .ends_at
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                MAX_MESSAGE_BYTES,
            ),
            enabled: routine.enabled,
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
            Self::CreateSwarm(form) => form.handle_key(key),
            Self::RenameSwarm { swarm_id, title } => match key.code {
                KeyCode::Enter => {
                    let value = title.value.trim();
                    if value.is_empty() {
                        title.error = Some("Swarm title cannot be empty.".into());
                        FormFlow::Stay
                    } else {
                        FormFlow::Send(request_action(
                            "Rename Swarm",
                            FollowUp::None,
                            |request_id| ClientMessage::RenameSwarm {
                                request_id,
                                swarm_id: swarm_id.clone(),
                                title: value.into(),
                            },
                        ))
                    }
                }
                KeyCode::Backspace => {
                    title.backspace();
                    FormFlow::Stay
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    title.push(&character.to_string());
                    FormFlow::Stay
                }
                _ => FormFlow::Stay,
            },
            Self::AddSwarmMember(form) => form.handle_key(key),
            Self::Routine(form) => form.handle_key(key),
        }
    }

    pub(super) fn paste(&mut self, value: &str) {
        match self {
            Self::Bot(form) => match form.row {
                0 => form.name.push(value),
                1 => form.description.push(value),
                _ => {}
            },
            Self::CreateSwarm(form) if form.row == 0 => form.title.push(value),
            Self::RenameSwarm { title, .. } => title.push(value),
            Self::Routine(form) => form.paste(value),
            Self::CreateSwarm(_) | Self::AddSwarmMember(_) => {}
        }
    }
}

impl BotForm {
    fn handle_key(&mut self, key: KeyEvent, gateway: &ReadyPayload) -> FormFlow {
        match key.code {
            KeyCode::Up | KeyCode::BackTab => self.row = moved(self.row, 3, -1),
            KeyCode::Down | KeyCode::Tab => self.row = moved(self.row, 3, 1),
            KeyCode::Enter if self.row < 2 => self.row += 1,
            KeyCode::Enter => return self.submit(gateway),
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return self.submit(gateway);
            }
            KeyCode::Backspace => match self.row {
                0 => self.name.backspace(),
                1 => self.description.backspace(),
                _ => {}
            },
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match self.row {
                    0 => self.name.push(&character.to_string()),
                    1 => self.description.push(&character.to_string()),
                    _ => {}
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
        match &self.mode {
            BotFormMode::Create => {
                FormFlow::Send(request_action("Create Bot", FollowUp::None, |request_id| {
                    ClientMessage::CreateBot {
                        request_id,
                        name,
                        description,
                    }
                }))
            }
            BotFormMode::Update(id) => {
                let Some(bot) = gateway.bots.iter().find(|bot| bot.id == *id) else {
                    self.error = Some("The selected Bot is no longer available.".into());
                    return FormFlow::Stay;
                };
                FormFlow::Send(request_action(
                    "Update Bot identity",
                    FollowUp::None,
                    |request_id| ClientMessage::UpdateBot {
                        request_id,
                        id: id.clone(),
                        expected_revision: bot.config.revision,
                        name,
                        description,
                        tint: bot.tint,
                        config: bot.config.config.clone(),
                    },
                ))
            }
        }
    }
}

impl CreateSwarmForm {
    fn handle_key(&mut self, key: KeyEvent) -> FormFlow {
        let length = self.bot_ids.len() + 2;
        match key.code {
            KeyCode::Up | KeyCode::BackTab => self.row = moved(self.row, length, -1),
            KeyCode::Down | KeyCode::Tab => self.row = moved(self.row, length, 1),
            KeyCode::Enter if self.row == 0 => self.row = 1,
            KeyCode::Enter if self.row == length - 1 => return self.submit(),
            KeyCode::Enter | KeyCode::Char(' ') if self.row > 0 => self.toggle_member(),
            KeyCode::Char('l') if self.row > 0 && self.row < length - 1 => {
                let bot_id = self.bot_ids[self.row - 1].clone();
                self.members.insert(bot_id.clone());
                self.leader_bot_id = Some(bot_id);
            }
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return self.submit();
            }
            KeyCode::Backspace if self.row == 0 => self.title.backspace(),
            KeyCode::Char(character)
                if self.row == 0
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.title.push(&character.to_string());
            }
            _ => {}
        }
        self.error = None;
        FormFlow::Stay
    }

    fn toggle_member(&mut self) {
        if self.row == 0 || self.row > self.bot_ids.len() {
            return;
        }
        let bot_id = self.bot_ids[self.row - 1].clone();
        if !self.members.remove(&bot_id) {
            self.members.insert(bot_id.clone());
            self.leader_bot_id.get_or_insert(bot_id);
        } else if self.leader_bot_id.as_deref() == Some(&bot_id) {
            self.leader_bot_id = None;
        }
    }

    pub(super) fn submit(&mut self) -> FormFlow {
        let title = self.title.value.trim().to_owned();
        if title.is_empty() {
            self.error = Some("Swarm title is required.".into());
            return FormFlow::Stay;
        }
        if self.members.len() < 2 {
            self.error = Some("Select at least two Bots.".into());
            return FormFlow::Stay;
        }
        let Some(leader_bot_id) = self.leader_bot_id.clone() else {
            self.error = Some("Appoint a leader with l.".into());
            return FormFlow::Stay;
        };
        let member_bot_ids = self
            .bot_ids
            .iter()
            .filter(|id| self.members.contains(*id))
            .cloned()
            .collect();
        FormFlow::Send(request_action(
            "Create Swarm",
            FollowUp::None,
            |request_id| ClientMessage::CreateSwarm {
                request_id,
                title,
                leader_bot_id,
                member_bot_ids,
            },
        ))
    }
}

impl AddMemberForm {
    pub(super) fn handle_key(&mut self, key: KeyEvent) -> FormFlow {
        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                self.row = moved(self.row, self.bot_ids.len(), -1);
                FormFlow::Stay
            }
            KeyCode::Down | KeyCode::Tab => {
                self.row = moved(self.row, self.bot_ids.len(), 1);
                FormFlow::Stay
            }
            KeyCode::Enter => {
                let Some(bot_id) = self.bot_ids.get(self.row).cloned() else {
                    return FormFlow::Stay;
                };
                FormFlow::Send(request_action(
                    "Add Swarm member",
                    FollowUp::None,
                    |request_id| ClientMessage::AddSwarmMember {
                        request_id,
                        swarm_id: self.swarm_id.clone(),
                        bot_id,
                    },
                ))
            }
            _ => FormFlow::Stay,
        }
    }
}

impl RoutineForm {
    pub(super) fn is_update(&self) -> bool {
        matches!(self.mode, RoutineFormMode::Update(_))
    }

    fn row_count(&self) -> usize {
        if self.is_update() { 8 } else { 7 }
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
            KeyCode::Char(' ') if self.is_update() && self.row == 6 => {
                self.enabled = !self.enabled;
            }
            KeyCode::Enter if self.row == self.save_row() => return self.submit(),
            KeyCode::Enter if self.row == 2 => self.change_schedule(1),
            KeyCode::Enter if self.is_update() && self.row == 6 => {
                self.enabled = !self.enabled;
            }
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

    pub(super) fn action(&self) -> std::result::Result<Action, String> {
        let workspace = self.workspace.value.trim();
        if workspace.is_empty() {
            return Err("Routine workspace is required.".into());
        }
        if self.instructions.value.trim().is_empty() {
            return Err("Routine instructions are required.".into());
        }
        let schedule = self.schedule()?;
        let ends_at = optional_i64(&self.ends_at.value, "end time")?;
        if ends_at.is_some_and(|value| value <= 0) {
            return Err("Routine end time must be a positive Unix timestamp.".into());
        }
        let bot_id = self.bot_id.clone();
        let workspace = PathBuf::from(workspace);
        let instructions = self.instructions.value.clone();
        match &self.mode {
            RoutineFormMode::Create(form_bot_id) => {
                debug_assert_eq!(&bot_id, form_bot_id);
                Ok(request_action(
                    "Create routine",
                    FollowUp::Routines,
                    |request_id| ClientMessage::CreateRoutine {
                        request_id,
                        bot_id,
                        workspace,
                        instructions,
                        schedule,
                        ends_at,
                    },
                ))
            }
            RoutineFormMode::Update(id) => Ok(request_action(
                "Update routine",
                FollowUp::Routines,
                |request_id| ClientMessage::UpdateRoutine {
                    request_id,
                    id: id.clone(),
                    bot_id,
                    workspace,
                    instructions,
                    schedule,
                    ends_at,
                    enabled: self.enabled,
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
