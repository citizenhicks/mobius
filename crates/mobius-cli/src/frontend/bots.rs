//! Gateway-scoped Bot, routine, and Swarm management.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;

use mobius::protocol::MAX_MESSAGE_BYTES;
use mobius::{Error, Result};
use mobius_gateway::client::{GatewayEvents, GatewaySender, MAX_PENDING_FRAMES};
use mobius_gateway::wire::{
    BotRecord, ClientMessage, ReadyPayload, Routine, RoutineRun, RoutineRunPreview,
    RoutineRunStatus, RoutineSchedule, RoutineScheduleKind, ServerFrame, ServerMessage,
    SwarmRecord,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, HighlightSpacing, List, ListState, Paragraph, Wrap};
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use super::setup::{self, SetupMode};
use super::terminal::{INPUT_POLL, MAX_INPUT_BATCH, poll_event};
use super::terminal_text;
use super::theme::{Role, current};

type BotsTerminal = Terminal<CrosstermBackend<io::Stdout>>;

const MAX_BOT_NAME_BYTES: usize = 128;
const MAX_BOT_DESCRIPTION_BYTES: usize = 2 * 1024;
const MAX_SWARM_TITLE_BYTES: usize = 256;

#[derive(Clone)]
enum Page {
    Root,
    Bot(String),
    Conversations(String),
    Routines(String),
    Routine { bot_id: String, routine_id: String },
    Runs { bot_id: String, routine_id: String },
    Run { bot_id: String, routine_id: String },
    Swarm(String),
}

#[derive(Clone)]
enum RootItem {
    Bot(String),
    Swarm(String),
}

#[derive(Clone, Copy)]
enum BotRow {
    Model,
    Capabilities,
    Conversations,
    Routines,
    Swarm,
}

#[derive(Clone, Copy)]
enum RoutineRow {
    Edit,
    Toggle,
    Run,
    History,
}

enum Confirmation {
    DeleteBot {
        id: String,
        revision: u64,
        handle: String,
    },
    DeleteRoutine {
        id: String,
        label: String,
    },
    DeleteRun {
        id: String,
        routine_id: String,
        label: String,
    },
    RemoveMember {
        swarm_id: String,
        bot_id: String,
        handle: String,
    },
    DisbandSwarm {
        id: String,
        title: String,
    },
}

#[derive(Clone)]
enum FollowUp {
    None,
    Routines,
    Runs(String),
}

struct Pending {
    request_id: String,
    label: &'static str,
    follow_up: FollowUp,
}

struct Notice {
    text: String,
    role: Role,
}

enum Form {
    Bot(BotForm),
    CreateSwarm(CreateSwarmForm),
    RenameSwarm { swarm_id: String, title: TextForm },
    AddSwarmMember(AddMemberForm),
    Routine(Box<RoutineForm>),
}

struct TextForm {
    value: String,
    limit: usize,
    multiline: bool,
    error: Option<String>,
}

impl TextForm {
    fn new(value: impl Into<String>, limit: usize) -> Self {
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

enum BotFormMode {
    Create,
    Update(String),
}

struct BotForm {
    mode: BotFormMode,
    name: TextForm,
    description: TextForm,
    row: usize,
    error: Option<String>,
}

impl BotForm {
    fn create() -> Self {
        Self {
            mode: BotFormMode::Create,
            name: TextForm::new("", MAX_BOT_NAME_BYTES),
            description: TextForm::new("", MAX_BOT_DESCRIPTION_BYTES),
            row: 0,
            error: None,
        }
    }

    fn update(bot: &BotRecord) -> Self {
        Self {
            mode: BotFormMode::Update(bot.id.clone()),
            name: TextForm::new(&bot.name, MAX_BOT_NAME_BYTES),
            description: TextForm::new(&bot.description, MAX_BOT_DESCRIPTION_BYTES),
            row: 0,
            error: None,
        }
    }
}

struct CreateSwarmForm {
    title: TextForm,
    bot_ids: Vec<String>,
    members: BTreeSet<String>,
    leader_bot_id: Option<String>,
    row: usize,
    error: Option<String>,
}

struct AddMemberForm {
    swarm_id: String,
    bot_ids: Vec<String>,
    row: usize,
}

enum RoutineFormMode {
    Create(String),
    Update(String),
}

struct RoutineForm {
    mode: RoutineFormMode,
    bot_id: String,
    workspace: TextForm,
    instructions: TextForm,
    schedule_kind: RoutineScheduleKind,
    schedule_value: TextForm,
    time_zone: TextForm,
    ends_at: TextForm,
    enabled: bool,
    row: usize,
    error: Option<String>,
}

impl RoutineForm {
    fn create(bot_id: String) -> Self {
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

    fn update(routine: &Routine) -> Self {
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

struct BotsState {
    page: Page,
    selected: usize,
    protected_bot_id: Option<String>,
    routines: Vec<Routine>,
    runs: Vec<RoutineRun>,
    preview: Option<RoutineRunPreview>,
    form: Option<Form>,
    pending: Option<Pending>,
    confirmation: Option<Confirmation>,
    notice: Option<Notice>,
}

enum Action {
    None,
    Exit,
    Setup {
        bot_id: String,
        mode: SetupMode,
    },
    Send {
        request_id: String,
        message: Box<ClientMessage>,
        label: &'static str,
        follow_up: FollowUp,
    },
}

enum FormFlow {
    Stay,
    Cancel,
    Send(Action),
}

impl BotsState {
    fn new(
        gateway: &ReadyPayload,
        preferred_bot_id: Option<&str>,
        protected_bot_id: Option<&str>,
    ) -> Self {
        let page = preferred_bot_id
            .filter(|id| gateway.bots.iter().any(|bot| bot.id == *id))
            .map_or(Page::Root, |id| Page::Bot(id.into()));
        Self {
            page,
            selected: 0,
            protected_bot_id: protected_bot_id.map(str::to_owned),
            routines: Vec::new(),
            runs: Vec::new(),
            preview: None,
            form: None,
            pending: None,
            confirmation: None,
            notice: None,
        }
    }

    fn root_items(&self, gateway: &ReadyPayload) -> Vec<RootItem> {
        gateway
            .bots
            .iter()
            .map(|bot| RootItem::Bot(bot.id.clone()))
            .chain(
                gateway
                    .swarms
                    .iter()
                    .map(|swarm| RootItem::Swarm(swarm.id.clone())),
            )
            .collect()
    }

    fn bot_rows(&self, gateway: &ReadyPayload, bot_id: &str) -> Vec<BotRow> {
        let mut rows = vec![
            BotRow::Model,
            BotRow::Capabilities,
            BotRow::Conversations,
            BotRow::Routines,
        ];
        if swarm_for_bot(gateway, bot_id).is_some() {
            rows.push(BotRow::Swarm);
        }
        rows
    }

    fn row_count(&self, gateway: &ReadyPayload) -> usize {
        match &self.page {
            Page::Root => self.root_items(gateway).len(),
            Page::Bot(id) => self.bot_rows(gateway, id).len(),
            Page::Conversations(id) => sessions_for_bot(gateway, id).len(),
            Page::Routines(id) => self
                .routines
                .iter()
                .filter(|routine| routine.bot_id == *id)
                .count(),
            Page::Routine { .. } => 4,
            Page::Runs { routine_id, .. } => self
                .runs
                .iter()
                .filter(|run| run.routine_id == *routine_id)
                .count(),
            Page::Run { .. } => 0,
            Page::Swarm(id) => gateway
                .swarms
                .iter()
                .find(|swarm| swarm.id == *id)
                .map_or(0, |swarm| swarm.members.len()),
        }
    }

    fn clamp(&mut self, gateway: &ReadyPayload) {
        if let Page::Bot(id)
        | Page::Conversations(id)
        | Page::Routines(id)
        | Page::Routine { bot_id: id, .. }
        | Page::Runs { bot_id: id, .. }
        | Page::Run { bot_id: id, .. } = &self.page
            && !gateway.bots.iter().any(|bot| bot.id == *id)
        {
            self.page = Page::Root;
            self.selected = 0;
        }
        if let Page::Swarm(id) = &self.page
            && !gateway.swarms.iter().any(|swarm| swarm.id == *id)
        {
            self.page = Page::Root;
            self.selected = 0;
        }
        if let Page::Routine { bot_id, routine_id }
        | Page::Runs { bot_id, routine_id }
        | Page::Run { bot_id, routine_id } = &self.page
            && !self
                .routines
                .iter()
                .any(|routine| routine.id == *routine_id)
        {
            self.page = Page::Routines(bot_id.clone());
            self.selected = 0;
        }
        self.selected = self.selected.min(self.row_count(gateway).saturating_sub(1));
    }

    fn move_selection(&mut self, gateway: &ReadyPayload, delta: isize) {
        let length = self.row_count(gateway);
        if length == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(length as isize) as usize;
        self.notice = None;
    }

    fn handle_key(&mut self, key: KeyEvent, gateway: &ReadyPayload) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return Action::Exit;
        }
        if self.pending.is_some() {
            return matches!(key.code, KeyCode::Char('q'))
                .then_some(Action::Exit)
                .unwrap_or(Action::None);
        }
        if self.form.is_some() {
            return self.handle_form_key(key, gateway);
        }
        if self.confirmation.is_some() {
            return self.handle_confirmation(key);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return self.back(),
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                self.move_selection(gateway, -1);
                return Action::None;
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                self.move_selection(gateway, 1);
                return Action::None;
            }
            KeyCode::Home => {
                self.selected = 0;
                return Action::None;
            }
            KeyCode::End if self.row_count(gateway) > 0 => {
                self.selected = self.row_count(gateway) - 1;
                return Action::None;
            }
            _ => {}
        }
        match self.page.clone() {
            Page::Root => self.handle_root_key(key, gateway),
            Page::Bot(id) => self.handle_bot_key(key, gateway, id),
            Page::Conversations(_) => Action::None,
            Page::Routines(id) => self.handle_routine_key(key, id),
            Page::Routine { bot_id, routine_id } => {
                self.handle_routine_detail_key(key, bot_id, routine_id)
            }
            Page::Runs { bot_id, routine_id } => self.handle_runs_key(key, bot_id, routine_id),
            Page::Run { .. } => Action::None,
            Page::Swarm(id) => self.handle_swarm_key(key, gateway, id),
        }
    }

    fn handle_root_key(&mut self, key: KeyEvent, gateway: &ReadyPayload) -> Action {
        match key.code {
            KeyCode::Char('n') => {
                self.form = Some(Form::Bot(BotForm::create()));
                return Action::None;
            }
            KeyCode::Char('s') => {
                let bot_ids = available_bot_ids(gateway);
                if bot_ids.len() < 2 {
                    self.fail("A Swarm needs at least two Bots that are not already in a Swarm.");
                } else {
                    self.form = Some(Form::CreateSwarm(CreateSwarmForm {
                        title: TextForm::new("", MAX_SWARM_TITLE_BYTES),
                        bot_ids,
                        members: BTreeSet::new(),
                        leader_bot_id: None,
                        row: 0,
                        error: None,
                    }));
                }
                return Action::None;
            }
            _ => {}
        }
        let Some(item) = self.root_items(gateway).get(self.selected).cloned() else {
            return Action::None;
        };
        match key.code {
            KeyCode::Enter => {
                self.page = match item {
                    RootItem::Bot(id) => Page::Bot(id),
                    RootItem::Swarm(id) => Page::Swarm(id),
                };
                self.selected = 0;
                Action::None
            }
            KeyCode::Char('e') => {
                match item {
                    RootItem::Bot(id) => {
                        if let Some(bot) = gateway.bots.iter().find(|bot| bot.id == id) {
                            self.form = Some(Form::Bot(BotForm::update(bot)));
                        }
                    }
                    RootItem::Swarm(id) => {
                        if let Some(swarm) = gateway.swarms.iter().find(|swarm| swarm.id == id) {
                            self.form = Some(Form::RenameSwarm {
                                swarm_id: id,
                                title: TextForm::new(&swarm.title, MAX_SWARM_TITLE_BYTES),
                            });
                        }
                    }
                }
                Action::None
            }
            KeyCode::Delete | KeyCode::Char('x') => {
                if let RootItem::Bot(id) = &item
                    && self.protected_bot_id.as_deref() == Some(id)
                {
                    self.fail(
                        "The Bot owning this open chat cannot be deleted here; use the gateway dashboard.",
                    );
                    return Action::None;
                }
                self.confirmation = match item {
                    RootItem::Bot(id) => gateway.bots.iter().find(|bot| bot.id == id).map(|bot| {
                        Confirmation::DeleteBot {
                            id,
                            revision: bot.config.revision,
                            handle: bot.handle.clone(),
                        }
                    }),
                    RootItem::Swarm(id) => {
                        gateway
                            .swarms
                            .iter()
                            .find(|swarm| swarm.id == id)
                            .map(|swarm| Confirmation::DisbandSwarm {
                                id,
                                title: swarm.title.clone(),
                            })
                    }
                };
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_bot_key(&mut self, key: KeyEvent, gateway: &ReadyPayload, id: String) -> Action {
        if key.code == KeyCode::Char('e') {
            if let Some(bot) = gateway.bots.iter().find(|bot| bot.id == id) {
                self.form = Some(Form::Bot(BotForm::update(bot)));
            }
            return Action::None;
        }
        if key.code != KeyCode::Enter {
            return Action::None;
        }
        let Some(row) = self.bot_rows(gateway, &id).get(self.selected).copied() else {
            return Action::None;
        };
        match row {
            BotRow::Model => Action::Setup {
                bot_id: id,
                mode: SetupMode::BotModel,
            },
            BotRow::Capabilities => Action::Setup {
                bot_id: id,
                mode: SetupMode::Bot,
            },
            BotRow::Conversations => {
                self.page = Page::Conversations(id);
                self.selected = 0;
                Action::None
            }
            BotRow::Routines => {
                self.page = Page::Routines(id);
                self.selected = 0;
                Action::None
            }
            BotRow::Swarm => {
                if let Some(swarm) = swarm_for_bot(gateway, &id) {
                    self.page = Page::Swarm(swarm.id.clone());
                    self.selected = 0;
                }
                Action::None
            }
        }
    }

    fn handle_routine_key(&mut self, key: KeyEvent, bot_id: String) -> Action {
        if key.code == KeyCode::Char('n') {
            self.form = Some(Form::Routine(Box::new(RoutineForm::create(bot_id))));
            return Action::None;
        }
        let Some(routine) = self
            .routines
            .iter()
            .filter(|routine| routine.bot_id == bot_id)
            .nth(self.selected)
            .cloned()
        else {
            return Action::None;
        };
        match key.code {
            KeyCode::Enter => {
                self.page = Page::Routine {
                    bot_id,
                    routine_id: routine.id,
                };
                self.selected = 0;
                Action::None
            }
            KeyCode::Char('e') => {
                self.form = Some(Form::Routine(Box::new(RoutineForm::update(&routine))));
                Action::None
            }
            KeyCode::Char(' ') => update_routine_action(&routine, !routine.enabled),
            KeyCode::Char('r') => request_action("Run routine", FollowUp::Routines, |request_id| {
                ClientMessage::RunRoutine {
                    request_id,
                    id: routine.id,
                }
            }),
            KeyCode::Delete | KeyCode::Char('x') => {
                self.confirmation = Some(Confirmation::DeleteRoutine {
                    id: routine.id,
                    label: routine.instructions,
                });
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_routine_detail_key(
        &mut self,
        key: KeyEvent,
        bot_id: String,
        routine_id: String,
    ) -> Action {
        let Some(routine) = self
            .routines
            .iter()
            .find(|routine| routine.id == routine_id)
            .cloned()
        else {
            return Action::None;
        };
        let row = [
            RoutineRow::Edit,
            RoutineRow::Toggle,
            RoutineRow::Run,
            RoutineRow::History,
        ][self.selected];
        if key.code == KeyCode::Char('e') {
            self.form = Some(Form::Routine(Box::new(RoutineForm::update(&routine))));
            return Action::None;
        }
        if key.code == KeyCode::Char(' ') {
            return update_routine_action(&routine, !routine.enabled);
        }
        if key.code == KeyCode::Char('r') {
            return request_action("Run routine", FollowUp::Routines, |request_id| {
                ClientMessage::RunRoutine {
                    request_id,
                    id: routine.id,
                }
            });
        }
        if key.code != KeyCode::Enter {
            return Action::None;
        }
        match row {
            RoutineRow::Edit => {
                self.form = Some(Form::Routine(Box::new(RoutineForm::update(&routine))));
                Action::None
            }
            RoutineRow::Toggle => update_routine_action(&routine, !routine.enabled),
            RoutineRow::Run => request_action("Run routine", FollowUp::Routines, |request_id| {
                ClientMessage::RunRoutine {
                    request_id,
                    id: routine.id,
                }
            }),
            RoutineRow::History => {
                self.page = Page::Runs {
                    bot_id,
                    routine_id: routine.id.clone(),
                };
                self.selected = 0;
                request_action("Load run history", FollowUp::None, |request_id| {
                    ClientMessage::ListRoutineHistory {
                        request_id,
                        id: Some(routine.id),
                    }
                })
            }
        }
    }

    fn handle_runs_key(&mut self, key: KeyEvent, bot_id: String, routine_id: String) -> Action {
        let Some(run) = runs_for_routine(&self.runs, &routine_id)
            .get(self.selected)
            .copied()
            .cloned()
        else {
            return Action::None;
        };
        match key.code {
            KeyCode::Enter => {
                self.preview = None;
                self.page = Page::Run { bot_id, routine_id };
                request_action("Load run", FollowUp::None, |request_id| {
                    ClientMessage::GetRoutineRunPreview {
                        request_id,
                        id: run.id,
                        before_sequence: None,
                    }
                })
            }
            KeyCode::Delete | KeyCode::Char('x') => {
                let label = routine_run_label(&run);
                self.confirmation = Some(Confirmation::DeleteRun {
                    id: run.id,
                    routine_id,
                    label,
                });
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_swarm_key(
        &mut self,
        key: KeyEvent,
        gateway: &ReadyPayload,
        swarm_id: String,
    ) -> Action {
        let Some(swarm) = gateway.swarms.iter().find(|swarm| swarm.id == swarm_id) else {
            return Action::None;
        };
        match key.code {
            KeyCode::Char('e') => {
                self.form = Some(Form::RenameSwarm {
                    swarm_id: swarm.id.clone(),
                    title: TextForm::new(&swarm.title, MAX_SWARM_TITLE_BYTES),
                });
            }
            KeyCode::Char('a') => {
                let bot_ids = available_bot_ids(gateway);
                if bot_ids.is_empty() {
                    self.fail("Every Bot already belongs to a Swarm.");
                } else {
                    self.form = Some(Form::AddSwarmMember(AddMemberForm {
                        swarm_id: swarm.id.clone(),
                        bot_ids,
                        row: 0,
                    }));
                }
            }
            KeyCode::Delete => {
                self.confirmation = Some(Confirmation::DisbandSwarm {
                    id: swarm.id.clone(),
                    title: swarm.title.clone(),
                });
            }
            KeyCode::Char('x') => {
                if let Some(member) = swarm.members.get(self.selected) {
                    if member.bot_id == swarm.leader_bot_id {
                        self.fail("The leader cannot leave; disband the Swarm instead.");
                    } else {
                        self.confirmation = Some(Confirmation::RemoveMember {
                            swarm_id: swarm.id.clone(),
                            bot_id: member.bot_id.clone(),
                            handle: member.handle.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    fn handle_confirmation(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                self.confirmation = None;
                Action::None
            }
            KeyCode::Char('y' | 'Y') => {
                let Some(confirmation) = self.confirmation.take() else {
                    return Action::None;
                };
                match confirmation {
                    Confirmation::DeleteBot { id, revision, .. } => {
                        request_action("Delete Bot and owned data", FollowUp::None, |request_id| {
                            ClientMessage::DeleteBot {
                                request_id,
                                id,
                                expected_revision: revision,
                            }
                        })
                    }
                    Confirmation::DeleteRoutine { id, .. } => {
                        request_action("Delete routine", FollowUp::Routines, |request_id| {
                            ClientMessage::DeleteRoutine { request_id, id }
                        })
                    }
                    Confirmation::DeleteRun { id, routine_id, .. } => request_action(
                        "Delete routine run",
                        FollowUp::Runs(routine_id),
                        |request_id| ClientMessage::DeleteRoutineRun { request_id, id },
                    ),
                    Confirmation::RemoveMember {
                        swarm_id, bot_id, ..
                    } => request_action("Remove Swarm member", FollowUp::None, |request_id| {
                        ClientMessage::LeaveSwarm {
                            request_id,
                            swarm_id,
                            bot_id,
                        }
                    }),
                    Confirmation::DisbandSwarm { id, .. } => {
                        request_action("Disband Swarm", FollowUp::None, |request_id| {
                            ClientMessage::DisbandSwarm {
                                request_id,
                                swarm_id: id,
                            }
                        })
                    }
                }
            }
            _ => Action::None,
        }
    }

    fn back(&mut self) -> Action {
        match &self.page {
            Page::Root => Action::Exit,
            Page::Bot(_) | Page::Swarm(_) => {
                self.page = Page::Root;
                self.selected = 0;
                Action::None
            }
            Page::Conversations(id) | Page::Routines(id) => {
                self.page = Page::Bot(id.clone());
                self.selected = 0;
                Action::None
            }
            Page::Routine { bot_id, .. } => {
                self.page = Page::Routines(bot_id.clone());
                self.selected = 0;
                Action::None
            }
            Page::Runs { bot_id, routine_id } => {
                self.page = Page::Routine {
                    bot_id: bot_id.clone(),
                    routine_id: routine_id.clone(),
                };
                self.selected = 0;
                Action::None
            }
            Page::Run { bot_id, routine_id } => {
                self.page = Page::Runs {
                    bot_id: bot_id.clone(),
                    routine_id: routine_id.clone(),
                };
                self.selected = 0;
                Action::None
            }
        }
    }

    fn begin(&mut self, request_id: String, label: &'static str, follow_up: FollowUp) {
        self.pending = Some(Pending {
            request_id,
            label,
            follow_up,
        });
        self.notice = None;
    }

    fn complete(&mut self) -> Option<FollowUp> {
        let pending = self.pending.take()?;
        self.notice = Some(Notice {
            text: format!("{} complete.", pending.label),
            role: Role::Success,
        });
        Some(pending.follow_up)
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.pending = None;
        self.notice = Some(Notice {
            text: message.into(),
            role: Role::Error,
        });
    }

    fn handle_form_key(&mut self, key: KeyEvent, gateway: &ReadyPayload) -> Action {
        let Some(mut form) = self.form.take() else {
            return Action::None;
        };
        match form.handle_key(key, gateway) {
            FormFlow::Stay => {
                self.form = Some(form);
                Action::None
            }
            FormFlow::Cancel => Action::None,
            FormFlow::Send(action) => action,
        }
    }

    fn paste(&mut self, value: &str) {
        if let Some(form) = self.form.as_mut() {
            form.paste(value);
        }
    }
}

impl Form {
    fn handle_key(&mut self, key: KeyEvent, gateway: &ReadyPayload) -> FormFlow {
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

    fn paste(&mut self, value: &str) {
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

    fn submit(&mut self, gateway: &ReadyPayload) -> FormFlow {
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

    fn submit(&mut self) -> FormFlow {
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
    fn handle_key(&mut self, key: KeyEvent) -> FormFlow {
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
    fn is_update(&self) -> bool {
        matches!(self.mode, RoutineFormMode::Update(_))
    }

    fn row_count(&self) -> usize {
        if self.is_update() { 8 } else { 7 }
    }

    fn save_row(&self) -> usize {
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

    fn action(&self) -> std::result::Result<Action, String> {
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

pub(in crate::frontend) async fn run(
    terminal: &mut BotsTerminal,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    preferred_bot_id: Option<&str>,
    protected_bot_id: Option<&str>,
) -> Result<()> {
    terminal.clear()?;
    let mut state = BotsState::new(gateway, preferred_bot_id, protected_bot_id);
    request_routines(sender, &mut state).await?;
    let mut tick = tokio::time::interval(INPUT_POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut deferred = Vec::new();
    let mut events_open = true;
    let mut dirty = true;

    let result = 'screen: loop {
        if dirty {
            terminal.draw(|frame| render(frame, &state, gateway))?;
            dirty = false;
        }
        tokio::select! {
            frame = events.next(), if events_open => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => break 'screen Err(gateway_error(error)),
                };
                match frame {
                    Some(frame) => {
                        let follow_up =
                            handle_frame(frame.message, gateway, &mut state, &mut deferred)?;
                        request_follow_up(sender, &mut state, follow_up).await?;
                    }
                    None => {
                        events_open = false;
                        state.fail("Gateway disconnected. Press q to close.");
                    }
                }
                state.clamp(gateway);
                dirty = true;
            }
            _ = tick.tick() => {
                for _ in 0..MAX_INPUT_BATCH {
                    let Some(event) = poll_event()? else { break; };
                    dirty = true;
                    let action = match event {
                        Event::Key(key) => state.handle_key(key, gateway),
                        Event::Paste(value) => {
                            state.paste(&value);
                            Action::None
                        }
                        Event::Resize(_, _)
                        | Event::FocusGained
                        | Event::FocusLost
                        | Event::Mouse(_) => Action::None,
                    };
                    match action {
                        Action::None => {}
                        Action::Exit => break 'screen Ok(()),
                        Action::Setup { bot_id, mode } => {
                            if let Err(error) = setup::run_bot(
                                terminal,
                                mode,
                                None,
                                sender,
                                events,
                                gateway,
                                &bot_id,
                            ).await {
                                state.fail(error.to_string());
                            }
                            terminal.clear()?;
                            state.clamp(gateway);
                        }
                        Action::Send {
                            request_id,
                            message,
                            label,
                            follow_up,
                        } => match sender.send(*message).await {
                            Ok(()) => state.begin(request_id, label, follow_up),
                            Err(error) => state.fail(error.to_string()),
                        },
                    }
                }
            }
        }
    };
    events.prepend(deferred).map_err(gateway_error)?;
    result
}

fn handle_frame(
    message: ServerMessage,
    gateway: &mut ReadyPayload,
    state: &mut BotsState,
    deferred: &mut Vec<ServerFrame>,
) -> Result<FollowUp> {
    let mut follow_up = FollowUp::None;
    match message {
        ServerMessage::Ready { payload } => *gateway = payload,
        ServerMessage::GatewayConfigured {
            request_id,
            payload,
        } => {
            *gateway = payload.clone();
            defer(
                ServerMessage::GatewayConfigured {
                    request_id,
                    payload,
                },
                deferred,
            )?;
        }
        ServerMessage::Sessions {
            request_id,
            sessions,
        } => {
            gateway.sessions = sessions.clone();
            if request_id.is_some() {
                defer(
                    ServerMessage::Sessions {
                        request_id,
                        sessions,
                    },
                    deferred,
                )?;
            }
        }
        ServerMessage::Bots { request_id, bots } => {
            gateway.bots = bots.clone();
            if request_id
                .as_ref()
                .is_some_and(|id| pending_matches(state, id))
            {
                follow_up = state.complete().unwrap_or(FollowUp::None);
            } else if request_id.is_some() {
                defer(ServerMessage::Bots { request_id, bots }, deferred)?;
            }
        }
        ServerMessage::Swarms { request_id, swarms } => {
            gateway.swarms = swarms.clone();
            if request_id
                .as_ref()
                .is_some_and(|id| pending_matches(state, id))
            {
                follow_up = state.complete().unwrap_or(FollowUp::None);
            } else if request_id.is_some() {
                defer(ServerMessage::Swarms { request_id, swarms }, deferred)?;
            }
        }
        ServerMessage::Routines {
            request_id,
            routines,
        } if pending_matches(state, &request_id) => {
            state.routines = routines;
            follow_up = state.complete().unwrap_or(FollowUp::None);
        }
        ServerMessage::RoutineHistory { request_id, runs }
            if pending_matches(state, &request_id) =>
        {
            state.runs = runs;
            follow_up = state.complete().unwrap_or(FollowUp::None);
        }
        ServerMessage::RoutineRunPreview {
            request_id,
            preview,
        } if pending_matches(state, &request_id) => {
            state.preview = Some(preview);
            follow_up = state.complete().unwrap_or(FollowUp::None);
        }
        ServerMessage::Accepted { request_id } if pending_matches(state, &request_id) => {
            follow_up = state.complete().unwrap_or(FollowUp::None);
        }
        ServerMessage::Rejected {
            request_id,
            message,
            ..
        } if pending_matches(state, &request_id) => state.fail(message),
        message => defer(message, deferred)?,
    }
    Ok(follow_up)
}

fn defer(message: ServerMessage, deferred: &mut Vec<ServerFrame>) -> Result<()> {
    if deferred.len() == MAX_PENDING_FRAMES {
        return Err(Error::Stopped(format!(
            "gateway event backlog exceeds {MAX_PENDING_FRAMES} frames while managing Bots: {message:?}"
        )));
    }
    deferred.push(ServerFrame::new(message));
    Ok(())
}

fn pending_matches(state: &BotsState, request_id: &str) -> bool {
    state
        .pending
        .as_ref()
        .is_some_and(|pending| pending.request_id == request_id)
}

async fn request_routines(sender: &GatewaySender, state: &mut BotsState) -> Result<()> {
    let request_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::ListRoutines {
            request_id: request_id.clone(),
            bot_id: None,
        })
        .await
        .map_err(gateway_error)?;
    state.begin(request_id, "Load routines", FollowUp::None);
    Ok(())
}

async fn request_runs(
    sender: &GatewaySender,
    state: &mut BotsState,
    routine_id: String,
) -> Result<()> {
    let request_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::ListRoutineHistory {
            request_id: request_id.clone(),
            id: Some(routine_id),
        })
        .await
        .map_err(gateway_error)?;
    state.begin(request_id, "Load run history", FollowUp::None);
    Ok(())
}

async fn request_follow_up(
    sender: &GatewaySender,
    state: &mut BotsState,
    follow_up: FollowUp,
) -> Result<()> {
    match follow_up {
        FollowUp::None => Ok(()),
        FollowUp::Routines => request_routines(sender, state).await,
        FollowUp::Runs(routine_id) => request_runs(sender, state, routine_id).await,
    }
}

fn request_action(
    label: &'static str,
    follow_up: FollowUp,
    make_message: impl FnOnce(String) -> ClientMessage,
) -> Action {
    let request_id = Uuid::new_v4().to_string();
    let message = make_message(request_id.clone());
    Action::Send {
        request_id,
        message: Box::new(message),
        label,
        follow_up,
    }
}

fn update_routine_action(routine: &Routine, enabled: bool) -> Action {
    let id = routine.id.clone();
    let bot_id = routine.bot_id.clone();
    let workspace = routine.workspace.clone();
    let instructions = routine.instructions.clone();
    let schedule = routine.schedule.clone();
    let ends_at = routine.ends_at;
    request_action("Update routine", FollowUp::Routines, |request_id| {
        ClientMessage::UpdateRoutine {
            request_id,
            id,
            bot_id,
            workspace,
            instructions,
            schedule,
            ends_at,
            enabled,
        }
    })
}

fn available_bot_ids(gateway: &ReadyPayload) -> Vec<String> {
    gateway
        .bots
        .iter()
        .filter(|bot| swarm_for_bot(gateway, &bot.id).is_none())
        .map(|bot| bot.id.clone())
        .collect()
}

fn runs_for_routine<'a>(runs: &'a [RoutineRun], routine_id: &str) -> Vec<&'a RoutineRun> {
    let mut runs = runs
        .iter()
        .filter(|run| run.routine_id == routine_id)
        .collect::<Vec<_>>();
    runs.sort_by_key(|run| std::cmp::Reverse(run.started_at));
    runs
}

fn routine_run_label(run: &RoutineRun) -> String {
    format!("{} · {}", run_status(run.status), run.started_at)
}

const fn run_status(status: RoutineRunStatus) -> &'static str {
    match status {
        RoutineRunStatus::Running => "running",
        RoutineRunStatus::Succeeded => "succeeded",
        RoutineRunStatus::Failed => "failed",
        RoutineRunStatus::Skipped => "skipped",
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

fn moved(current: usize, length: usize, delta: isize) -> usize {
    if length == 0 {
        0
    } else {
        (current as isize + delta).rem_euclid(length as isize) as usize
    }
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

fn render(frame: &mut ratatui::Frame<'_>, state: &BotsState, gateway: &ReadyPayload) {
    let theme = current();
    frame.render_widget(
        Block::default().style(theme.style(Role::Canvas)),
        frame.area(),
    );
    let area = content_area(frame.area());
    let [header, body, notice, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(8),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("◉ ", theme.style(Role::AccentStrong)),
                Span::styled(
                    "MÖBIUS",
                    theme.style(Role::Accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" Bots", theme.style(Role::Muted)),
            ]),
            Line::styled(
                format!("  {}", terminal_text(&gateway.machine_name)),
                theme.style(Role::Muted),
            ),
        ]),
        header,
    );
    if let Some(form) = &state.form {
        render_form(frame, body, form, gateway);
    } else {
        match &state.page {
            Page::Root => render_root(frame, body, state, gateway),
            Page::Bot(id) => render_bot(frame, body, state, gateway, id),
            Page::Conversations(id) => render_conversations(frame, body, state, gateway, id),
            Page::Routines(id) => render_routines(frame, body, state, gateway, id),
            Page::Routine { routine_id, .. } => render_routine(frame, body, state, routine_id),
            Page::Runs { routine_id, .. } => render_runs(frame, body, state, routine_id),
            Page::Run { .. } => render_run(frame, body, state),
            Page::Swarm(id) => render_swarm(frame, body, state, gateway, id),
        }
    }
    render_notice(frame, notice, state);
    frame.render_widget(
        Paragraph::new(footer_text(state)).style(theme.style(Role::Muted)),
        footer,
    );
}

fn render_form(frame: &mut ratatui::Frame<'_>, area: Rect, form: &Form, gateway: &ReadyPayload) {
    let (title, mut lines, error) = match form {
        Form::Bot(form) => {
            let title = match form.mode {
                BotFormMode::Create => "Create Bot",
                BotFormMode::Update(_) => "Edit Bot",
            };
            (
                title,
                vec![
                    form_line("Name", &form.name.value, form.row == 0),
                    form_line("Description", &form.description.value, form.row == 1),
                    choice_line("Save", form.row == 2),
                ],
                form.error
                    .as_deref()
                    .or(form.name.error.as_deref())
                    .or(form.description.error.as_deref()),
            )
        }
        Form::CreateSwarm(form) => {
            let mut lines = vec![form_line("Title", &form.title.value, form.row == 0)];
            for (index, bot_id) in form.bot_ids.iter().enumerate() {
                let label = gateway
                    .bots
                    .iter()
                    .find(|bot| bot.id == *bot_id)
                    .map_or(bot_id.as_str(), |bot| bot.handle.as_str());
                let marker = if form.members.contains(bot_id) {
                    "[x]"
                } else {
                    "[ ]"
                };
                let leader = if form.leader_bot_id.as_deref() == Some(bot_id.as_str()) {
                    " · leader"
                } else {
                    ""
                };
                lines.push(choice_line(
                    &format!("{marker} @{label}{leader}"),
                    form.row == index + 1,
                ));
            }
            lines.push(choice_line(
                "Save Swarm",
                form.row == form.bot_ids.len() + 1,
            ));
            (
                "Create Swarm",
                lines,
                form.error.as_deref().or(form.title.error.as_deref()),
            )
        }
        Form::RenameSwarm { title, .. } => (
            "Rename Swarm",
            vec![form_line("Title", &title.value, true)],
            title.error.as_deref(),
        ),
        Form::AddSwarmMember(form) => (
            "Add Swarm member",
            form.bot_ids
                .iter()
                .enumerate()
                .map(|(index, bot_id)| {
                    let label = gateway
                        .bots
                        .iter()
                        .find(|bot| bot.id == *bot_id)
                        .map_or(bot_id.as_str(), |bot| bot.handle.as_str());
                    choice_line(&format!("@{label}"), form.row == index)
                })
                .collect(),
            None,
        ),
        Form::Routine(form) => {
            let save_row = form.save_row();
            let mut lines = vec![
                form_line("Workspace", &form.workspace.value, form.row == 0),
                form_line("Instructions", &form.instructions.value, form.row == 1),
                choice_line(
                    &format!("Schedule · {}", schedule_kind_label(form.schedule_kind)),
                    form.row == 2,
                ),
                form_line(
                    schedule_value_label(form.schedule_kind),
                    &form.schedule_value.value,
                    form.row == 3,
                ),
                form_line("Time zone (cron)", &form.time_zone.value, form.row == 4),
                form_line(
                    "Ends at (Unix, optional)",
                    &form.ends_at.value,
                    form.row == 5,
                ),
            ];
            if form.is_update() {
                lines.push(choice_line(
                    if form.enabled {
                        "[x] Enabled"
                    } else {
                        "[ ] Enabled"
                    },
                    form.row == 6,
                ));
            }
            lines.push(choice_line("Save routine", form.row == save_row));
            let field_error = [
                &form.workspace,
                &form.instructions,
                &form.schedule_value,
                &form.time_zone,
                &form.ends_at,
            ]
            .into_iter()
            .find_map(|field| field.error.as_deref());
            (
                if form.is_update() {
                    "Edit routine"
                } else {
                    "Create routine"
                },
                lines,
                form.error.as_deref().or(field_error),
            )
        }
    };
    if let Some(error) = error {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            terminal_text(error),
            current().style(Role::Error),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn form_line(label: &str, value: &str, focused: bool) -> Line<'static> {
    let role = if focused { Role::Selection } else { Role::Text };
    let value = terminal_text(value).replace(['\n', '\t'], " ");
    Line::styled(
        format!("{} {label}: {value}", if focused { "›" } else { " " }),
        current().style(role),
    )
}

fn choice_line(label: &str, focused: bool) -> Line<'static> {
    Line::styled(
        format!(
            "{} {}",
            if focused { "›" } else { " " },
            terminal_text(label)
        ),
        current().style(if focused { Role::Selection } else { Role::Text }),
    )
}

const fn schedule_kind_label(kind: RoutineScheduleKind) -> &'static str {
    match kind {
        RoutineScheduleKind::Once => "once",
        RoutineScheduleKind::Interval => "interval",
        RoutineScheduleKind::Cron => "cron",
    }
}

const fn schedule_value_label(kind: RoutineScheduleKind) -> &'static str {
    match kind {
        RoutineScheduleKind::Once => "Run at (Unix)",
        RoutineScheduleKind::Interval => "Every seconds",
        RoutineScheduleKind::Cron => "Cron expression",
    }
}

fn render_root(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &BotsState,
    gateway: &ReadyPayload,
) {
    let [catalog, details] =
        Layout::vertical([Constraint::Percentage(45), Constraint::Min(5)]).areas(area);
    let theme = current();
    let items = state.root_items(gateway);
    let rows = items.iter().map(|item| match item {
        RootItem::Bot(id) => {
            gateway
                .bots
                .iter()
                .find(|bot| bot.id == *id)
                .map_or_else(Line::default, |bot| {
                    Line::from(format!(
                        " BOT    @{} · {}",
                        terminal_text(&bot.handle),
                        terminal_text(&bot.name)
                    ))
                })
        }
        RootItem::Swarm(id) => gateway
            .swarms
            .iter()
            .find(|swarm| swarm.id == *id)
            .map_or_else(Line::default, |swarm| {
                Line::from(format!(
                    " SWARM  {} · {} Bots",
                    terminal_text(&swarm.title),
                    swarm.members.len()
                ))
            }),
    });
    let mut list_state =
        ListState::default().with_selected((!items.is_empty()).then_some(state.selected));
    frame.render_stateful_widget(
        List::new(rows)
            .block(panel("Bots & Swarms"))
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::default().add_modifier(Modifier::BOLD)),
        catalog,
        &mut list_state,
    );
    let lines = items.get(state.selected).map_or_else(
        || vec![Line::styled("No Bots or Swarms", theme.style(Role::Muted))],
        |item| root_details(item, state, gateway),
    );
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel("Details")),
        details,
    );
}

fn root_details(item: &RootItem, state: &BotsState, gateway: &ReadyPayload) -> Vec<Line<'static>> {
    match item {
        RootItem::Bot(id) => gateway
            .bots
            .iter()
            .find(|bot| bot.id == *id)
            .map_or_else(Vec::new, |bot| bot_details(bot, state, gateway)),
        RootItem::Swarm(id) => gateway
            .swarms
            .iter()
            .find(|swarm| swarm.id == *id)
            .map_or_else(Vec::new, |swarm| swarm_details(swarm, gateway)),
    }
}

fn bot_details(bot: &BotRecord, state: &BotsState, gateway: &ReadyPayload) -> Vec<Line<'static>> {
    let theme = current();
    let chats = sessions_for_bot(gateway, &bot.id).len();
    let routines = state
        .routines
        .iter()
        .filter(|routine| routine.bot_id == bot.id)
        .count();
    let swarm = swarm_for_bot(gateway, &bot.id)
        .map(|swarm| terminal_text(&swarm.title))
        .unwrap_or_else(|| "none".into());
    vec![
        Line::styled(
            format!(
                "{} · @{}",
                terminal_text(&bot.name),
                terminal_text(&bot.handle)
            ),
            theme.style(Role::AccentStrong).add_modifier(Modifier::BOLD),
        ),
        Line::styled(terminal_text(&bot.description), theme.style(Role::Text)),
        Line::styled(
            format!(
                "model: {}",
                terminal_text(&bot.config.config.provider.model)
            ),
            theme.style(Role::Muted),
        ),
        Line::styled(
            format!("{chats} conversations · {routines} routines · swarm {swarm}"),
            theme.style(Role::Muted),
        ),
    ]
}

fn swarm_details(swarm: &SwarmRecord, gateway: &ReadyPayload) -> Vec<Line<'static>> {
    let theme = current();
    let leader = gateway
        .bots
        .iter()
        .find(|bot| bot.id == swarm.leader_bot_id)
        .map(|bot| format!("@{}", terminal_text(&bot.handle)))
        .unwrap_or_else(|| "unavailable".into());
    vec![
        Line::styled(
            terminal_text(&swarm.title),
            theme.style(Role::AccentStrong).add_modifier(Modifier::BOLD),
        ),
        Line::styled(format!("leader: {leader}"), theme.style(Role::Muted)),
        Line::styled(
            format!(
                "{} Bots · {} board posts",
                swarm.members.len(),
                swarm.messages.len()
            ),
            theme.style(Role::Muted),
        ),
    ]
}

fn render_bot(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &BotsState,
    gateway: &ReadyPayload,
    bot_id: &str,
) {
    let Some(bot) = gateway.bots.iter().find(|bot| bot.id == bot_id) else {
        return;
    };
    let rows = state.bot_rows(gateway, bot_id);
    let chats = sessions_for_bot(gateway, bot_id).len();
    let routines = state
        .routines
        .iter()
        .filter(|routine| routine.bot_id == bot_id)
        .count();
    let lines = rows.iter().map(|row| match row {
        BotRow::Model => Line::from(" Model & reasoning"),
        BotRow::Capabilities => Line::from(" Capabilities"),
        BotRow::Conversations => Line::from(format!(" Conversations · {chats}")),
        BotRow::Routines => Line::from(format!(" Routines · {routines}")),
        BotRow::Swarm => Line::from(format!(
            " Swarm · {}",
            swarm_for_bot(gateway, bot_id).map_or("—", |swarm| swarm.title.as_str())
        )),
    });
    render_list_page(
        frame,
        area,
        &format!(
            "{} · @{}",
            terminal_text(&bot.name),
            terminal_text(&bot.handle)
        ),
        lines,
        rows.len(),
        state.selected,
    );
}

fn render_conversations(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &BotsState,
    gateway: &ReadyPayload,
    bot_id: &str,
) {
    let sessions = sessions_for_bot(gateway, bot_id);
    let rows = sessions.iter().map(|session| {
        let title = session
            .title
            .as_deref()
            .or(session.first_user_message.as_deref())
            .unwrap_or(&session.session_id);
        Line::from(format!(
            " {} · {}",
            terminal_text(title),
            terminal_text(
                session
                    .session_context
                    .workspace_label
                    .as_deref()
                    .unwrap_or("workspace unavailable")
            )
        ))
    });
    render_list_page(
        frame,
        area,
        "Conversations",
        rows,
        sessions.len(),
        state.selected,
    );
}

fn render_routines(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &BotsState,
    _gateway: &ReadyPayload,
    bot_id: &str,
) {
    let routines = state
        .routines
        .iter()
        .filter(|routine| routine.bot_id == bot_id)
        .collect::<Vec<_>>();
    let rows = routines.iter().map(|routine| {
        Line::from(format!(
            " {} · {}",
            if routine.enabled && !routine.finished {
                "●"
            } else {
                "○"
            },
            terminal_text(&routine.instructions)
        ))
    });
    render_list_page(
        frame,
        area,
        "Routines",
        rows,
        routines.len(),
        state.selected,
    );
}

fn render_routine(frame: &mut ratatui::Frame<'_>, area: Rect, state: &BotsState, routine_id: &str) {
    let Some(routine) = state
        .routines
        .iter()
        .find(|routine| routine.id == routine_id)
    else {
        return;
    };
    let runs = state
        .runs
        .iter()
        .filter(|run| run.routine_id == routine_id)
        .count();
    let rows = [
        Line::from(" Edit settings"),
        Line::from(if routine.enabled {
            " Disable"
        } else {
            " Enable"
        }),
        Line::from(" Run now"),
        Line::from(format!(" Run history · {runs}")),
    ];
    let length = rows.len();
    render_list_page(
        frame,
        area,
        &terminal_text(&routine.instructions),
        rows.into_iter(),
        length,
        state.selected,
    );
}

fn render_runs(frame: &mut ratatui::Frame<'_>, area: Rect, state: &BotsState, routine_id: &str) {
    let runs = runs_for_routine(&state.runs, routine_id);
    let rows = runs.iter().map(|run| {
        let message = run
            .message
            .as_deref()
            .map(|message| format!(" · {}", terminal_text(message)))
            .unwrap_or_default();
        Line::from(format!(" {}{message}", routine_run_label(run)))
    });
    render_list_page(
        frame,
        area,
        "Routine runs",
        rows,
        runs.len(),
        state.selected,
    );
}

fn render_run(frame: &mut ratatui::Frame<'_>, area: Rect, state: &BotsState) {
    let Some(preview) = &state.preview else {
        frame.render_widget(
            Paragraph::new("Loading run…").block(panel("Routine run")),
            area,
        );
        return;
    };
    let mut lines = vec![
        Line::styled(
            routine_run_label(&preview.run),
            current().style(Role::AccentStrong),
        ),
        Line::styled(
            terminal_text(&preview.routine.workspace.display().to_string()),
            current().style(Role::Muted),
        ),
        Line::from(""),
    ];
    for record in &preview.records {
        for rendered in &record.blocks {
            let text = super::block_text(&rendered.block);
            lines.extend(
                terminal_text(&text)
                    .lines()
                    .map(|line| Line::from(line.to_owned())),
            );
        }
    }
    if preview.records.is_empty() {
        lines.push(Line::styled(
            "No transcript records",
            current().style(Role::Muted),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("Routine run"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_swarm(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &BotsState,
    gateway: &ReadyPayload,
    swarm_id: &str,
) {
    let Some(swarm) = gateway.swarms.iter().find(|swarm| swarm.id == swarm_id) else {
        return;
    };
    let rows = swarm.members.iter().map(|member| {
        Line::from(format!(
            " @{}{}",
            terminal_text(&member.handle),
            if member.bot_id == swarm.leader_bot_id {
                " · leader"
            } else {
                ""
            }
        ))
    });
    render_list_page(
        frame,
        area,
        &format!("Swarm · {}", terminal_text(&swarm.title)),
        rows,
        swarm.members.len(),
        state.selected,
    );
}

fn render_list_page<'a>(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    rows: impl Iterator<Item = Line<'a>>,
    length: usize,
    selected: usize,
) {
    let mut list_state = ListState::default().with_selected((length > 0).then_some(selected));
    frame.render_stateful_widget(
        List::new(rows)
            .block(panel(title))
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::default().add_modifier(Modifier::BOLD))
            .scroll_padding(1),
        area,
        &mut list_state,
    );
}

fn render_notice(frame: &mut ratatui::Frame<'_>, area: Rect, state: &BotsState) {
    let theme = current();
    let (text, role) = if let Some(confirmation) = &state.confirmation {
        (confirmation_text(confirmation), Role::Warning)
    } else if let Some(pending) = &state.pending {
        (format!("{}…", pending.label), Role::Muted)
    } else if let Some(notice) = &state.notice {
        (notice.text.clone(), notice.role)
    } else {
        (String::new(), Role::Muted)
    };
    frame.render_widget(Paragraph::new(text).style(theme.style(role)), area);
}

fn confirmation_text(confirmation: &Confirmation) -> String {
    match confirmation {
        Confirmation::DeleteBot { handle, .. } => format!(
            "Delete @{handle} plus every owned conversation, routine, run, and Swarm membership? y/n"
        ),
        Confirmation::DeleteRoutine { label, .. } => {
            format!("Delete routine `{}`? y/n", terminal_text(label))
        }
        Confirmation::DeleteRun { label, .. } => {
            format!(
                "Delete routine run `{}` and its transcript? y/n",
                terminal_text(label)
            )
        }
        Confirmation::RemoveMember { handle, .. } => {
            format!("Remove @{handle} from this Swarm? y/n")
        }
        Confirmation::DisbandSwarm { title, .. } => format!(
            "Disband `{}` and delete its board and collective scratchpad? y/n",
            terminal_text(title)
        ),
    }
}

fn footer_text(state: &BotsState) -> &'static str {
    if state.form.is_some() {
        return "tab/↑↓ select · type edit · enter continue/save · esc cancel";
    }
    if state.confirmation.is_some() {
        return "y confirm · n cancel";
    }
    match state.page {
        Page::Root => "↑↓ select · n new Bot · s new Swarm · e edit · x delete · q close",
        Page::Bot(_) => "↑↓ select · enter open · e edit identity · esc back",
        Page::Conversations(_) => "↑↓ inspect · esc back",
        Page::Routines(_) => {
            "↑↓ select · n new · enter open · e edit · space enable · r run · x delete"
        }
        Page::Routine { .. } => "↑↓ select · enter open · e edit · space enable · r run · esc back",
        Page::Runs { .. } => "↑↓ select · enter inspect · x delete · esc back",
        Page::Run { .. } => "esc back",
        Page::Swarm(_) => "↑↓ select · a add · e rename · x remove · delete disband · esc back",
    }
}

fn panel(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .title(format!(" {} ", title.into()))
        .border_style(current().style(Role::Border))
}

fn content_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(92);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y.saturating_add(1),
        width,
        area.height.saturating_sub(2),
    )
}

fn sessions_for_bot<'a>(
    gateway: &'a ReadyPayload,
    bot_id: &str,
) -> Vec<&'a mobius_gateway::wire::SessionRecord> {
    let mut sessions = gateway
        .sessions
        .iter()
        .filter(|session| session.session_context.bot_id == bot_id)
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    sessions
}

fn swarm_for_bot<'a>(gateway: &'a ReadyPayload, bot_id: &str) -> Option<&'a SwarmRecord> {
    gateway
        .swarms
        .iter()
        .find(|swarm| swarm.members.iter().any(|member| member.bot_id == bot_id))
}

fn gateway_error(error: impl std::fmt::Display) -> Error {
    Error::Stopped(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobius::protocol::SessionFileLimits;
    use mobius_gateway::wire::{AgentComposition, ProviderTint, VersionedAgentConfig};

    fn gateway(bots: Vec<BotRecord>) -> ReadyPayload {
        ReadyPayload {
            machine_name: "test".into(),
            bots,
            sessions: Vec::new(),
            swarms: Vec::new(),
            providers: Vec::new(),
            provider_instances: Vec::new(),
            bot_defaults: None,
            models: Vec::new(),
            model_providers: Default::default(),
            middleware_features: Vec::new(),
            extensions: Vec::new(),
            contributions: Vec::new(),
            max_active_sessions: 1,
            session_file_limits: SessionFileLimits {
                max_attachment_references: 0,
                max_file_bytes: 0,
                max_session_files: 0,
                max_session_bytes: 0,
                max_upload_chunk_bytes: 0,
            },
        }
    }

    fn bot(id: &str) -> BotRecord {
        BotRecord {
            id: id.into(),
            handle: id.into(),
            name: id.into(),
            description: "description".into(),
            tint: ProviderTint::Teal,
            config: VersionedAgentConfig {
                revision: 7,
                config: AgentComposition::default(),
            },
        }
    }

    fn routine(id: &str) -> Routine {
        Routine {
            id: id.into(),
            bot_id: "bot-a".into(),
            workspace: "/srv/project".into(),
            instructions: "inspect the project".into(),
            schedule: RoutineSchedule {
                kind: RoutineScheduleKind::Interval,
                at: None,
                every_seconds: Some(600),
                expression: None,
                time_zone: None,
            },
            ends_at: None,
            enabled: true,
            finished: false,
            next_run_at: Some(1),
        }
    }

    fn message(action: Action) -> ClientMessage {
        let Action::Send { message, .. } = action else {
            panic!("expected gateway request");
        };
        *message
    }

    #[test]
    fn back_keeps_nested_bot_pages_inside_the_manager() {
        let mut state = BotsState::new(&gateway(vec![bot("bot-a")]), None, None);
        state.page = Page::Routines("bot-a".into());
        state.selected = 3;
        assert!(matches!(state.back(), Action::None));
        assert!(matches!(state.page, Page::Bot(ref id) if id == "bot-a"));
        assert_eq!(state.selected, 0);
        assert!(matches!(state.back(), Action::None));
        assert!(matches!(state.page, Page::Root));
        assert!(matches!(state.back(), Action::Exit));
    }

    #[test]
    fn bot_forms_create_and_update_identity_without_changing_configuration() {
        let bot = bot("bot-a");
        let gateway = gateway(vec![bot.clone()]);
        let mut create = BotForm::create();
        create.name.value = "Reviewer".into();
        create.description.value = "Review focused changes".into();
        assert!(matches!(
            message(match create.submit(&gateway) {
                FormFlow::Send(action) => action,
                _ => panic!("expected create request"),
            }),
            ClientMessage::CreateBot { name, .. } if name == "Reviewer"
        ));

        let mut update = BotForm::update(&bot);
        update.name.value = "Renamed".into();
        assert!(matches!(
            message(match update.submit(&gateway) {
                FormFlow::Send(action) => action,
                _ => panic!("expected update request"),
            }),
            ClientMessage::UpdateBot {
                expected_revision: 7,
                name,
                tint: ProviderTint::Teal,
                config,
                ..
            } if name == "Renamed" && config == bot.config.config
        ));
    }

    #[test]
    fn swarm_forms_create_rename_and_add_members() {
        let mut create = CreateSwarmForm {
            title: TextForm::new("Pair", MAX_SWARM_TITLE_BYTES),
            bot_ids: vec!["bot-a".into(), "bot-b".into()],
            members: BTreeSet::from(["bot-a".into(), "bot-b".into()]),
            leader_bot_id: Some("bot-a".into()),
            row: 3,
            error: None,
        };
        assert!(matches!(
            message(match create.submit() {
                FormFlow::Send(action) => action,
                _ => panic!("expected create request"),
            }),
            ClientMessage::CreateSwarm { leader_bot_id, member_bot_ids, .. }
                if leader_bot_id == "bot-a" && member_bot_ids.len() == 2
        ));

        let mut rename = Form::RenameSwarm {
            swarm_id: "swarm-a".into(),
            title: TextForm::new("Renamed", MAX_SWARM_TITLE_BYTES),
        };
        assert!(matches!(
            rename.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &gateway(Vec::new()),
            ),
            FormFlow::Send(Action::Send { message, .. })
                if matches!(*message, ClientMessage::RenameSwarm { ref title, .. } if title == "Renamed")
        ));

        let mut add = AddMemberForm {
            swarm_id: "swarm-a".into(),
            bot_ids: vec!["bot-c".into()],
            row: 0,
        };
        assert!(matches!(
            add.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            FormFlow::Send(Action::Send { message, .. })
                if matches!(*message, ClientMessage::AddSwarmMember { ref bot_id, .. } if bot_id == "bot-c")
        ));
    }

    #[test]
    fn routine_forms_create_edit_and_toggle_enabled_state() {
        let mut create = RoutineForm::create("bot-a".into());
        create.workspace.value = "/srv/project".into();
        create.instructions.value = "build it".into();
        assert!(matches!(
            message(create.action().expect("valid create")),
            ClientMessage::CreateRoutine { bot_id, schedule, .. }
                if bot_id == "bot-a" && schedule.every_seconds == Some(3600)
        ));

        let routine = routine("routine-a");
        let update = RoutineForm::update(&routine);
        assert!(matches!(
            message(update.action().expect("valid update")),
            ClientMessage::UpdateRoutine { id, enabled: true, .. } if id == "routine-a"
        ));
        assert!(matches!(
            message(update_routine_action(&routine, false)),
            ClientMessage::UpdateRoutine { enabled: false, .. }
        ));
    }

    #[test]
    fn run_requests_are_correlated_and_unrelated_history_is_deferred() {
        let mut gateway = gateway(vec![bot("bot-a")]);
        let mut state = BotsState::new(&gateway, None, None);
        state.begin("owned".into(), "Load run history", FollowUp::None);
        let run = RoutineRun {
            id: "run-a".into(),
            routine_id: "routine-a".into(),
            bot_id: "bot-a".into(),
            started_at: 1,
            finished_at: Some(2),
            status: RoutineRunStatus::Succeeded,
            session_id: Some("session-a".into()),
            message: None,
        };
        let mut deferred = Vec::new();
        handle_frame(
            ServerMessage::RoutineHistory {
                request_id: "other".into(),
                runs: vec![run.clone()],
            },
            &mut gateway,
            &mut state,
            &mut deferred,
        )
        .expect("defer unrelated response");
        assert_eq!(deferred.len(), 1);

        handle_frame(
            ServerMessage::RoutineHistory {
                request_id: "owned".into(),
                runs: vec![run.clone()],
            },
            &mut gateway,
            &mut state,
            &mut deferred,
        )
        .expect("accept correlated response");
        assert_eq!(state.runs, vec![run.clone()]);
        assert!(matches!(
            message(request_action("Load run", FollowUp::None, |request_id| {
                ClientMessage::GetRoutineRunPreview {
                    request_id,
                    id: run.id.clone(),
                    before_sequence: None,
                }
            })),
            ClientMessage::GetRoutineRunPreview { id, .. } if id == "run-a"
        ));
        assert!(matches!(
            message(request_action("Delete run", FollowUp::None, |request_id| {
                ClientMessage::DeleteRoutineRun { request_id, id: run.id }
            })),
            ClientMessage::DeleteRoutineRun { id, .. } if id == "run-a"
        ));
    }
}
