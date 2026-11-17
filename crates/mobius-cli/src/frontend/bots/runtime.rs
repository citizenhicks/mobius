use std::sync::Arc;

use mobius::{Error, Result};
use mobius_gateway::client::{GatewayEvents, GatewaySender};
use mobius_gateway::wire::{
    ClientMessage, ReadyPayload, RoutineChange, ServerFrame, ServerMessage, apply_session_changes,
};
use ratatui::crossterm::event::Event;
use tokio::time::MissedTickBehavior;

use super::Action;
use super::render::render;
use super::state::{BotsState, Page};
use crate::frontend::setup;
use crate::frontend::terminal::{INPUT_POLL, Screen, input_batch};
use crate::gateway_error;

pub(in crate::frontend) async fn run(
    terminal: &mut Screen,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    preferred_bot_id: Option<&str>,
    protected_bot_id: Option<&str>,
) -> Result<Option<mobius::SessionId>> {
    terminal.clear()?;
    let mut state = BotsState::new(gateway, preferred_bot_id, protected_bot_id);
    request_routines(sender, &mut state).await?;
    let mut tick = tokio::time::interval(INPUT_POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut events = events.scoped();
    let mut events_open = true;
    let mut dirty = true;

    'screen: loop {
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
                if let Some(frame) = frame {
                    if let Some(error) = frame.message.response_error(None) {
                        break 'screen Err(Error::Stopped(error.message.into()));
                    }
                    if let Some(frame) = handle_frame(frame.message, gateway, &mut state) {
                        events.defer(frame).map_err(gateway_error)?;
                    }
                } else {
                    events_open = false;
                    state.fail("Gateway disconnected. Press q to close.");
                }
                state.clamp(gateway);
                dirty = true;
            }
            _ = tick.tick() => {
                for event in input_batch() {
                    let event = event?;
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
                        Action::Exit => break 'screen Ok(None),
                        Action::OpenSession(session_id) => {
                            break 'screen Ok(Some(session_id));
                        }
                        Action::Setup(mode) => {
                            let Page::Bot(bot_id) = &state.page else {
                                continue;
                            };
                            if let Err(error) = setup::run_bot(
                                terminal,
                                mode,
                                None,
                                sender,
                                events.reborrow(),
                                gateway,
                                bot_id,
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
                        } => match sender.send(*message).await {
                            Ok(()) => state.begin(request_id, label),
                            Err(error) => state.fail(error.to_string()),
                        },
                    }
                }
            }
        }
    }
}

pub(super) fn handle_frame(
    message: ServerMessage,
    gateway: &mut ReadyPayload,
    state: &mut BotsState,
) -> Option<ServerFrame> {
    let mut deferred = None;
    match message {
        ServerMessage::RoutineChanged { change } => apply_routine_change(state, change),
        ServerMessage::Ready { payload } => gateway.update(payload),
        ServerMessage::GatewayConfigured {
            request_id,
            payload,
        } => {
            gateway.update(payload.clone()); // rules-allow: this screen applies it and the requesting flow still gets the frame
            deferred = Some(ServerFrame::new(ServerMessage::GatewayConfigured {
                request_id,
                payload,
            }));
        }
        ServerMessage::Sessions {
            request_id,
            sessions,
        } => {
            if request_id.is_some() {
                gateway.sessions = sessions.clone(); // rules-allow: the correlated reply must still reach its waiter
                deferred = Some(ServerFrame::new(ServerMessage::Sessions {
                    request_id,
                    sessions,
                }));
            } else {
                gateway.sessions = sessions;
            }
        }
        ServerMessage::SessionsChanged { sessions, .. } => {
            apply_session_changes(&mut gateway.sessions, sessions);
        }
        ServerMessage::Bots { request_id, bots } => {
            if request_id
                .as_ref()
                .is_some_and(|id| pending_matches(state, id))
            {
                gateway.bots = bots;
                state.complete();
            } else if request_id.is_some() {
                gateway.bots = bots.clone(); // rules-allow: the correlated payload must still reach its actual waiter
                deferred = Some(ServerFrame::new(ServerMessage::Bots { request_id, bots }));
            } else {
                gateway.bots = bots;
            }
        }
        ServerMessage::Routines {
            request_id,
            routines,
        } if pending_matches(state, &request_id) => {
            state.routines = routines;
            state.complete();
        }
        ServerMessage::RoutineHistory { request_id, runs }
            if pending_matches(state, &request_id) =>
        {
            state.runs = runs;
            state.complete();
        }
        ServerMessage::RoutineRunPreview {
            request_id,
            preview,
        } if pending_matches(state, &request_id) => {
            state.preview = Some(preview);
            state.complete();
        }
        ServerMessage::Accepted { request_id } if pending_matches(state, &request_id) => {
            state.complete();
        }
        ServerMessage::Rejected {
            request_id,
            message,
            ..
        } if pending_matches(state, &request_id) => state.fail(&*message),
        message => deferred = Some(ServerFrame::new(message)),
    }
    deferred
}

fn apply_routine_change(state: &mut BotsState, change: RoutineChange) {
    match change {
        RoutineChange::Saved { routine } => upsert(&mut state.routines, routine, |r| &r.id),
        RoutineChange::Ran { routine, run } => {
            let shown = match &state.page {
                Page::Runs { routine_id, .. } | Page::Run { routine_id, .. } => {
                    *routine_id == run.routine_id
                }
                _ => false,
            };
            if shown {
                upsert(&mut state.runs, run, |r| &r.id);
            }
            upsert(&mut state.routines, routine, |r| &r.id);
        }
        RoutineChange::Deleted { routine_id } => {
            state.routines.retain(|routine| routine.id != routine_id);
            state.runs.retain(|run| run.routine_id != routine_id);
        }
        RoutineChange::RunDeleted { run_id } => state.runs.retain(|run| run.id != run_id),
    }
}

fn upsert<T>(held: &mut Vec<T>, record: T, id: impl Fn(&T) -> &Arc<str>) {
    match held
        .iter_mut()
        .find(|candidate| id(candidate) == id(&record))
    {
        Some(existing) => *existing = record,
        None => held.push(record),
    }
}

fn pending_matches(state: &BotsState, request_id: &str) -> bool {
    state
        .pending
        .as_ref()
        .is_some_and(|pending| *pending.request_id == *request_id)
}

async fn request_routines(sender: &GatewaySender, state: &mut BotsState) -> Result<()> {
    let request_id = crate::fresh_id();
    sender
        .send(ClientMessage::ListRoutines {
            request_id: Arc::clone(&request_id),
            bot_id: None,
        })
        .await
        .map_err(gateway_error)?;
    state.begin(request_id, "Load routines");
    Ok(())
}
