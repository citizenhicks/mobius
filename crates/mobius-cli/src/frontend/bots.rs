//! Gateway-scoped Bot and routine management.

use std::sync::Arc;

mod form;
mod render;
mod runtime;
mod state;

pub(in crate::frontend) use self::runtime::run;

use super::setup::SetupMode;
use mobius_gateway::wire::ClientMessage;

enum Action {
    None,
    Exit,
    OpenSession(mobius::SessionId),
    /// Runs setup for the Bot shown on the current `Page::Bot`.
    Setup(SetupMode),
    Send {
        request_id: Arc<str>,
        message: Box<ClientMessage>,
        label: &'static str,
        follow_up: FollowUp,
    },
}

#[derive(Clone)]
enum FollowUp {
    None,
    Routines,
    Runs(Arc<str>),
}

fn request_action(
    label: &'static str,
    follow_up: FollowUp,
    make_message: impl FnOnce(Arc<str>) -> ClientMessage,
) -> Action {
    let request_id = crate::fresh_id();
    let message = make_message(Arc::clone(&request_id));
    Action::Send {
        request_id,
        message: Box::new(message),
        label,
        follow_up,
    }
}

/// The model a Bot listing names: the möbius route's model, or the CLI harness's model.
pub(super) fn bot_model(bot: &mobius_gateway::wire::BotRecord) -> &str {
    use mobius_gateway::wire::HarnessConfig;
    match &bot.config.config {
        HarnessConfig::Mobius(composition) => &composition.provider.model,
        HarnessConfig::Codex(options) | HarnessConfig::Claude(options) => {
            options.model.as_deref().unwrap_or("default model")
        }
    }
}

/// The harness label and model a Bot listing shows.
pub(super) fn bot_runtime(
    bot: &mobius_gateway::wire::BotRecord,
    gateway: &mobius_gateway::wire::ReadyPayload,
) -> String {
    let harness = gateway
        .harnesses
        .iter()
        .find(|harness| harness.kind == bot.harness)
        .map_or("", |harness| harness.label.as_str());
    format!("{harness} · {}", bot_model(bot))
}

#[cfg(test)]
mod tests {
    use super::form::{FormFlow, RoutineForm};
    use super::runtime::handle_frame;
    use super::state::{BotsState, Page};
    use super::*;
    use crate::frontend::fixtures::{bot, ready as gateway};
    use mobius_gateway::wire::{
        Routine, RoutineRun, RoutineRunStatus, RoutineSchedule, RoutineScheduleKind, ServerMessage,
    };
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn routine(id: &str) -> Routine {
        Routine {
            id: id.into(),
            bot_id: "bot-a".into(),
            workspace: "/srv/project".into(),
            instructions: "inspect the project".into(),
            bindings: vec![mobius_gateway::wire::RoutineBinding {
                id: "timer-a".into(),
                on: mobius_gateway::wire::HookSelector::Schedule {
                    schedule: RoutineSchedule {
                        kind: RoutineScheduleKind::Interval,
                        at: None,
                        every_seconds: Some(600),
                        expression: None,
                        time_zone: None,
                    },
                    ends_at: None,
                },
                action: mobius_gateway::wire::RoutineAction::Start,
            }],
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
    fn bot_menu_edits_identity_and_multiline_prompt_with_revision_protection() {
        let bot = bot("bot-a");
        let mut gateway = gateway(vec![bot.clone()]);
        let mut state = BotsState::new(&gateway, None, None);
        state.page = Page::Bot(Arc::clone(&bot.id));
        state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &gateway);
        let form = state
            .form
            .as_mut()
            .expect("visible identity row opens editor");
        form.handle_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &gateway,
        );
        form.paste("Reviewer");
        form.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &gateway);
        form.handle_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &gateway,
        );
        form.paste("Reviews code");
        form.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &gateway);
        form.handle_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &gateway,
        );
        form.paste("Be concise.");
        form.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), &gateway);
        form.paste("Check correctness.\nExplain risks.");
        Arc::make_mut(&mut gateway.bots[0].config).revision += 1;
        let FormFlow::Send(action) = form.handle_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &gateway,
        ) else {
            panic!("expected save");
        };
        let ClientMessage::UpdateBot {
            name,
            description,
            expected_revision,
            tint,
            shape,
            config,
            ..
        } = message(action)
        else {
            panic!("expected update");
        };
        assert_eq!(&*name, "Reviewer");
        assert_eq!(&*description, "Reviews code");
        assert_eq!(expected_revision, 7);
        assert_eq!(tint, bot.tint);
        assert_eq!(shape, bot.shape);
        let mut expected = (**bot.config.config.mobius().unwrap()).clone();
        expected.system_prompt = "Be concise.\nCheck correctness.\nExplain risks.".into();
        assert_eq!(
            config,
            mobius_gateway::wire::HarnessConfig::Mobius(Arc::new(expected))
        );
    }

    #[test]
    fn editing_routine_content_preserves_event_bindings_and_other_timers() {
        use mobius_gateway::wire::{
            HookKind, HookSelector, HookSource, RoutineAction, RoutineBinding,
        };
        let event_binding = RoutineBinding {
            id: "event-hook".into(),
            on: HookSelector::Event {
                source: HookSource::Session {
                    session_id: mobius::SessionId::new("watched").expect("session"),
                },
                kind: HookKind::SessionTurnFinished,
                routine_outcome: None,
                session_outcome: None,
                custom_name: None,
            },
            action: RoutineAction::Start,
        };
        let mut routine = routine("routine-a");
        let mut other_timer = routine.bindings[0].clone();
        other_timer.id = "other-timer".into();
        other_timer.action = RoutineAction::Pause;
        routine
            .bindings
            .extend([event_binding.clone(), other_timer]);
        let mut form = RoutineForm::update(&routine);
        form.instructions.value = "updated content".into();
        let ClientMessage::RoutineCommand { command, .. } = message(form.action().expect("update"))
        else {
            panic!("command")
        };
        let RoutineAction::Update { definition } = command.action else {
            panic!("update")
        };
        assert_eq!(definition.bindings, routine.bindings);
        assert_eq!(definition.instructions, "updated content");
        routine.bindings = vec![event_binding];
        let mut form = RoutineForm::update(&routine);
        form.instructions.value = "event-only content".into();
        let ClientMessage::RoutineCommand { command, .. } =
            message(form.action().expect("event-only update"))
        else {
            panic!("command")
        };
        let RoutineAction::Update { definition } = command.action else {
            panic!("update")
        };
        assert_eq!(definition.bindings, routine.bindings);
    }

    #[test]
    fn run_requests_are_correlated_and_unrelated_history_is_deferred() {
        let mut gateway = gateway(vec![bot("bot-a")]);
        let mut state = BotsState::new(&gateway, None, None);
        let owned = crate::fresh_id();
        state.begin(Arc::clone(&owned), "Load run history", FollowUp::None);
        let run = RoutineRun {
            id: "run-a".into(),
            routine_id: "routine-a".into(),
            bot_id: "bot-a".into(),
            started_at: 1,
            finished_at: Some(2),
            status: RoutineRunStatus::Succeeded,
            session_id: Some(mobius::SessionId::new("session-a").expect("session")),
            message: None,
        };
        let (_, deferred) = handle_frame(
            ServerMessage::RoutineHistory {
                request_id: "other".into(),
                runs: vec![run.clone()],
            },
            &mut gateway,
            &mut state,
        );
        assert!(deferred.is_some());

        let (_, deferred) = handle_frame(
            ServerMessage::RoutineHistory {
                request_id: owned,
                runs: vec![run.clone()],
            },
            &mut gateway,
            &mut state,
        );
        assert!(deferred.is_none());
        assert_eq!(state.runs, vec![run]);
    }
}
