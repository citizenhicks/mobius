//! Gateway-scoped Bot and routine management.

mod form;
mod render;
mod runtime;
mod state;

pub(in crate::frontend) use self::runtime::run;

use super::setup::SetupMode;
use mobius_gateway::wire::ClientMessage;
use uuid::Uuid;

enum Action {
    None,
    Exit,
    OpenSession(String),
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

#[derive(Clone)]
enum FollowUp {
    None,
    Routines,
    Runs(String),
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

fn moved(current: usize, length: usize, delta: isize) -> usize {
    if length == 0 {
        0
    } else {
        (current as isize + delta).rem_euclid(length as isize) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::form::{FormFlow, RoutineForm};
    use super::runtime::handle_frame;
    use super::state::{BotsState, Page};
    use super::*;
    use mobius::protocol::SessionFileLimits;
    use mobius_gateway::wire::{
        AgentComposition, BotRecord, ProviderTint, ReadyPayload, Routine, RoutineInteractionPolicy,
        RoutineRun, RoutineRunStatus, RoutineSchedule, RoutineScheduleKind, ServerMessage,
        VersionedAgentConfig,
    };
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn gateway(bots: Vec<BotRecord>) -> ReadyPayload {
        ReadyPayload {
            gateway_version: env!("CARGO_PKG_VERSION").into(),
            machine_name: "test".into(),
            computer_view: mobius_gateway::wire::ComputerView::Unavailable,
            bots,
            sessions: Vec::new(),
            background_approvals: Vec::new(),
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
            revisions: Default::default(),
            omitted: Default::default(),
            image_models: Vec::new(),
            voice_models: Vec::new(),
        }
    }

    fn bot(id: &str) -> BotRecord {
        BotRecord {
            id: id.into(),
            conversation_session_id: "bot-conversation-test".into(),
            handle: id.into(),
            name: id.into(),
            description: "description".into(),
            tint: ProviderTint::Teal,
            shape: mobius_gateway::wire::BotShape::Circle,
            config: VersionedAgentConfig {
                revision: 7,
                config: AgentComposition::default(),
            },
            accepts_file_attachments: false,
            routine_interaction_policy: RoutineInteractionPolicy::Unattended,
        }
    }

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
        state.page = Page::Bot(bot.id.clone());
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
        gateway.bots[0].config.revision += 1;
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
        assert_eq!(name, "Reviewer");
        assert_eq!(description, "Reviews code");
        assert_eq!(expected_revision, 7);
        assert_eq!(tint, bot.tint);
        assert_eq!(shape, bot.shape);
        let mut expected = bot.config.config;
        expected.system_prompt = "Be concise.\nCheck correctness.\nExplain risks.".into();
        assert_eq!(config, expected);
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
                    session_id: "watched".into(),
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
                request_id: "owned".into(),
                runs: vec![run.clone()],
            },
            &mut gateway,
            &mut state,
        );
        assert!(deferred.is_none());
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
