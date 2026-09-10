//! Gateway-scoped Bot, routine, and Swarm management.

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
    use super::form::MAX_SWARM_TITLE_BYTES;
    use super::form::{
        AddMemberForm, BotForm, CreateSwarmForm, Form, FormFlow, RoutineForm, TextForm,
    };
    use super::runtime::handle_frame;
    use super::state::{BotsState, Page, available_bot_ids, update_routine_action};
    use super::*;
    use mobius::protocol::SessionFileLimits;
    use mobius_gateway::wire::{
        AgentComposition, BotRecord, ProviderTint, ReadyPayload, Routine, RoutineRun,
        RoutineRunStatus, RoutineSchedule, RoutineScheduleKind, ServerMessage,
        VersionedAgentConfig,
    };
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::BTreeSet;

    fn gateway(bots: Vec<BotRecord>) -> ReadyPayload {
        ReadyPayload {
            gateway_version: env!("CARGO_PKG_VERSION").into(),
            machine_name: "test".into(),
            bots,
            sessions: Vec::new(),
            background_approvals: Vec::new(),
            swarm_attentions: Vec::new(),
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
    fn swarm_picker_offers_only_bots_that_enabled_collaboration() {
        let independent = bot("independent");
        let mut peer = bot("peer");
        peer.config.config.middleware.set_setting(
            "bots",
            "collaboration",
            Some(mobius::protocol::FrontendSettingValue::String(
                "swarm".into(),
            )),
        );
        let gateway = gateway(vec![independent, peer]);
        assert_eq!(available_bot_ids(&gateway), ["peer"]);
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
        let mut expected = bot.config.config;
        expected.system_prompt = "Be concise.\nCheck correctness.\nExplain risks.".into();
        assert_eq!(config, expected);
    }

    #[test]
    fn long_bot_prompt_keeps_the_editing_end_and_save_visible() {
        use ratatui::{Terminal, backend::TestBackend};
        let bot = bot("bot-a");
        let gateway = gateway(vec![bot.clone()]);
        let mut state = BotsState::new(&gateway, None, None);
        let mut form = BotForm::update(&bot);
        form.prompt.as_mut().unwrap().value = format!("{}\nPROMPT END", "long prompt ".repeat(200));
        form.row = 2;
        state.form = Some(Form::Bot(form));
        let mut terminal = Terminal::new(TestBackend::new(50, 15)).expect("terminal");
        terminal
            .draw(|frame| super::render::render(frame, &state, &gateway))
            .expect("draw");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("PROMPT END") && screen.contains("Save"));
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
