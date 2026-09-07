use std::io;

use mobius::{Error, Result};
use mobius_gateway::client::{GatewayEvents, GatewaySender, MAX_PENDING_FRAMES};
use mobius_gateway::wire::{ClientMessage, ReadyPayload, ServerFrame, ServerMessage};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::Event;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use super::render::render;
use super::state::BotsState;
use super::{Action, FollowUp};
use crate::frontend::setup;
use crate::frontend::terminal::{INPUT_POLL, MAX_INPUT_BATCH, poll_event};

type BotsTerminal = Terminal<CrosstermBackend<io::Stdout>>;

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

pub(super) fn handle_frame(
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

fn gateway_error(error: impl std::fmt::Display) -> Error {
    Error::Stopped(error.to_string())
}
