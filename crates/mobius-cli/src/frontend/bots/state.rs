use mobius_gateway::wire::{
    ClientMessage, ReadyPayload, Routine, RoutineRun, RoutineRunPreview, RoutineRunStatus,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::form::{BotForm, Form, FormFlow, RoutineForm};
use super::{Action, FollowUp, request_action};
use crate::frontend::setup::SetupMode;
use crate::frontend::theme::Role;

#[derive(Clone)]
pub(super) enum Page {
    Root,
    Bot(String),
    Conversations(String),
    Routines(String),
    Routine { bot_id: String, routine_id: String },
    Runs { bot_id: String, routine_id: String },
    Run { bot_id: String, routine_id: String },
}

#[derive(Clone, Copy)]
pub(super) enum BotRow {
    Identity,
    Model,
    Capabilities,
    Conversations,
    Routines,
}

pub(super) const BOT_ROWS: [BotRow; 5] = [
    BotRow::Identity,
    BotRow::Model,
    BotRow::Capabilities,
    BotRow::Conversations,
    BotRow::Routines,
];

#[derive(Clone, Copy)]
pub(super) enum RoutineRow {
    Edit,
    Toggle,
    Run,
    History,
}

pub(super) enum Confirmation {
    Bot {
        id: String,
        revision: u64,
        handle: String,
    },
    Routine {
        id: String,
        label: String,
    },
    Run {
        id: String,
        routine_id: String,
        label: String,
    },
}

pub(super) struct Pending {
    pub(super) request_id: String,
    pub(super) label: &'static str,
    pub(super) follow_up: FollowUp,
}

pub(super) struct Notice {
    pub(super) text: String,
    pub(super) role: Role,
}

pub(super) struct BotsState {
    pub(super) page: Page,
    pub(super) selected: usize,
    pub(super) protected_bot_id: Option<String>,
    pub(super) routines: Vec<Routine>,
    pub(super) runs: Vec<RoutineRun>,
    pub(super) preview: Option<RoutineRunPreview>,
    pub(super) form: Option<Form>,
    pub(super) pending: Option<Pending>,
    pub(super) confirmation: Option<Confirmation>,
    pub(super) notice: Option<Notice>,
}
impl BotsState {
    pub(super) fn new(
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

    fn row_count(&self, gateway: &ReadyPayload) -> usize {
        match &self.page {
            Page::Root => gateway.bots.len(),
            Page::Bot(_) => BOT_ROWS.len(),
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
        }
    }

    pub(super) fn clamp(&mut self, gateway: &ReadyPayload) {
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

    pub(super) fn handle_key(&mut self, key: KeyEvent, gateway: &ReadyPayload) -> Action {
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
        }
    }

    fn handle_root_key(&mut self, key: KeyEvent, gateway: &ReadyPayload) -> Action {
        if key.code == KeyCode::Char('n') {
            self.form = Some(Form::Bot(Box::new(BotForm::create())));
            return Action::None;
        }
        let Some(bot) = gateway.bots.get(self.selected) else {
            return Action::None;
        };
        match key.code {
            KeyCode::Enter => {
                self.page = Page::Bot(bot.id.clone());
                self.selected = 0;
            }
            KeyCode::Char('e') => {
                self.form = Some(Form::Bot(Box::new(BotForm::update(bot))));
            }
            KeyCode::Delete | KeyCode::Char('x') => {
                if self.protected_bot_id.as_deref() == Some(&bot.id) {
                    self.fail(
                        "The Bot owning this open chat cannot be deleted here; use the gateway dashboard.",
                    );
                } else {
                    self.confirmation = Some(Confirmation::Bot {
                        id: bot.id.clone(),
                        revision: bot.config.revision,
                        handle: bot.handle.clone(),
                    });
                }
            }
            _ => {}
        }
        Action::None
    }

    fn handle_bot_key(&mut self, key: KeyEvent, gateway: &ReadyPayload, id: String) -> Action {
        if key.code == KeyCode::Char('e') {
            if let Some(bot) = gateway.bots.iter().find(|bot| bot.id == id) {
                self.form = Some(Form::Bot(Box::new(BotForm::update(bot))));
            }
            return Action::None;
        }
        if key.code != KeyCode::Enter {
            return Action::None;
        }
        let Some(row) = BOT_ROWS.get(self.selected).copied() else {
            return Action::None;
        };
        match row {
            BotRow::Identity => {
                if let Some(bot) = gateway.bots.iter().find(|bot| bot.id == id) {
                    self.form = Some(Form::Bot(Box::new(BotForm::update(bot))));
                }
                Action::None
            }
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
                self.confirmation = Some(Confirmation::Routine {
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
            .into_iter()
            .nth(self.selected)
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
                        id: run.id.clone(),
                        before_sequence: None,
                    }
                })
            }
            KeyCode::Delete | KeyCode::Char('x') => {
                let label = routine_run_label(run);
                self.confirmation = Some(Confirmation::Run {
                    id: run.id.clone(),
                    routine_id,
                    label,
                });
                Action::None
            }
            _ => Action::None,
        }
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
                    Confirmation::Bot { id, revision, .. } => {
                        request_action("Delete Bot and owned data", FollowUp::None, |request_id| {
                            ClientMessage::DeleteBot {
                                request_id,
                                id,
                                expected_revision: revision,
                            }
                        })
                    }
                    Confirmation::Routine { id, .. } => {
                        request_action("Delete routine", FollowUp::Routines, |request_id| {
                            ClientMessage::DeleteRoutine { request_id, id }
                        })
                    }
                    Confirmation::Run { id, routine_id, .. } => request_action(
                        "Delete routine run",
                        FollowUp::Runs(routine_id),
                        |request_id| ClientMessage::DeleteRoutineRun { request_id, id },
                    ),
                }
            }
            _ => Action::None,
        }
    }

    pub(super) fn back(&mut self) -> Action {
        match &self.page {
            Page::Root => Action::Exit,
            Page::Bot(_) => {
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

    pub(super) fn begin(&mut self, request_id: String, label: &'static str, follow_up: FollowUp) {
        self.pending = Some(Pending {
            request_id,
            label,
            follow_up,
        });
        self.notice = None;
    }

    pub(super) fn complete(&mut self) -> Option<FollowUp> {
        let pending = self.pending.take()?;
        self.notice = Some(Notice {
            text: format!("{} complete.", pending.label),
            role: Role::Success,
        });
        Some(pending.follow_up)
    }

    pub(super) fn fail(&mut self, message: impl Into<String>) {
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

    pub(super) fn paste(&mut self, value: &str) {
        if let Some(form) = self.form.as_mut() {
            form.paste(value);
        }
    }
}
pub(super) fn update_routine_action(routine: &Routine, enabled: bool) -> Action {
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

pub(super) fn runs_for_routine<'a>(
    runs: &'a [RoutineRun],
    routine_id: &str,
) -> Vec<&'a RoutineRun> {
    let mut runs = runs
        .iter()
        .filter(|run| run.routine_id == routine_id)
        .collect::<Vec<_>>();
    runs.sort_by_key(|run| std::cmp::Reverse(run.started_at));
    runs
}

pub(super) fn routine_run_label(run: &RoutineRun) -> String {
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
pub(super) fn sessions_for_bot<'a>(
    gateway: &'a ReadyPayload,
    bot_id: &str,
) -> Vec<&'a mobius_gateway::wire::SessionRecord> {
    let mut sessions = gateway
        .sessions
        .iter()
        .filter(|session| {
            session.session_context.bot_id == bot_id
                || session
                    .member_bot_ids
                    .as_ref()
                    .is_some_and(|members| members.iter().any(|id| id == bot_id))
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    sessions
}
