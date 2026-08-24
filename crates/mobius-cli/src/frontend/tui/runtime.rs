use std::io;
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::SynchronizedUpdate;
use ratatui::crossterm::event::Event as TerminalEvent;
use ratatui::crossterm::event::KeyEventKind;
use ratatui::crossterm::execute;
use ratatui::crossterm::style::Print;
use tokio::time::MissedTickBehavior;

use super::TranscriptTone;
use super::TuiState;
use super::clipboard::ClipboardUploads;
use super::clipboard::read_clipboard;
use super::events::{handle_gateway_event, handle_gateway_history};
use super::input::UiAction;
use super::view::render_preview;
use crate::frontend::FrontendExit;
use crate::frontend::catalog::UiCatalog;
use crate::frontend::extensions;
use crate::frontend::gateway;
use crate::frontend::gateway_actions::{prepare, render_response};
use crate::frontend::setup;
use crate::frontend::terminal::{INPUT_POLL, MAX_INPUT_BATCH, TerminalGuard, poll_event};
use mobius::protocol::{ModelInfo, Op, Submission};
use mobius::{Error, Result};
use mobius_gateway::client::{GatewayEvents, GatewaySender};
use mobius_gateway::wire::{ClientMessage, ReadyPayload, ServerMessage, SessionReadyPayload};
use uuid::Uuid;

const ELAPSED_INTERVAL: Duration = Duration::from_secs(1);
const CLEAR_SCREEN_AND_SCROLLBACK: &str = "\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H";

struct ReplayHydration {
    pending: bool,
}

impl ReplayHydration {
    const fn pending() -> Self {
        Self { pending: true }
    }

    const fn allows_draw(&self) -> bool {
        !self.pending
    }

    fn observe(&mut self, message: &ServerMessage, session_id: &str) {
        if matches!(
            message,
            ServerMessage::SessionReplayComplete {
                session_id: actual,
                ..
            } if actual == session_id
        ) {
            self.pending = false;
        }
    }

    fn finish(&mut self) {
        self.pending = false;
    }
}

pub(in crate::frontend) async fn run(
    sender: GatewaySender,
    mut events: GatewayEvents,
    gateway: &mut ReadyPayload,
    session: &mut SessionReadyPayload,
    mut catalog: UiCatalog,
    local_gateway: bool,
    gateway_endpoint: String,
) -> Result<(FrontendExit, GatewaySender, GatewayEvents)> {
    let mut guard = TerminalGuard::alternate()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    if gateway.default_config.is_none() || gateway.models.is_empty() {
        setup::run(
            &mut terminal,
            setup::SetupMode::Login,
            None,
            &sender,
            &mut events,
            gateway,
            session,
        )
        .await?;
        if gateway.default_config.is_none() || gateway.models.is_empty() {
            drop(terminal);
            drop(guard);
            return Ok((FrontendExit::Discard, sender, events));
        }
        catalog.replace_model_choices(&gateway.models);
    }
    let mut workspace_inventory = catalog.start_workspace_inventory(local_gateway);
    let mut workspace_inventory_pending = true;
    let model = ModelInfo {
        model: session.session.model.model.clone(),
        reasoning_effort: session.session.model.reasoning_effort.clone(),
    };
    let model_route = session.session.model.route.clone();
    let session_id = session.session.session_id.clone();
    let mut state = TuiState::new(
        &catalog,
        catalog.workspace().to_path_buf(),
        model,
        model_route,
        agent_summary(gateway, session),
    );
    let mut uploads = ClipboardUploads::default();
    state.context_limit = session.context_limit_tokens;
    let mut tick = tokio::time::interval(INPUT_POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut elapsed = tokio::time::interval(ELAPSED_INTERVAL);
    elapsed.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut events_open = true;
    let mut dirty = true;
    let mut replay_hydration = ReplayHydration::pending();
    let mut exit = FrontendExit::Exit;
    let mut clear_on_exit = false;

    'ui: loop {
        if dirty && replay_hydration.allows_draw() {
            io::stdout().sync_update(|_| -> Result<()> {
                terminal.draw(|frame| {
                    if state.preview.is_some() {
                        render_preview(frame, &mut state);
                    } else {
                        super::view::render(frame, &mut state, &catalog);
                    }
                })?;
                Ok(())
            })??;
            dirty = false;
        }
        tokio::select! {
            event = events.next(), if events_open => {
                match event {
                    Ok(Some(frame)) => {
                        replay_hydration.observe(&frame.message, &session_id);
                        if let Some(result) = uploads
                            .handle(
                                &frame.message,
                                &session_id,
                                &gateway.session_file_limits,
                            )
                            .await
                        {
                            match result {
                                Ok(advance) => {
                                    if let Some(attachment) = advance.attachment {
                                        state.attachments.push(attachment);
                                    }
                                    if let Some(message) = advance.message
                                        && let Err(error) = sender.send(message).await
                                    {
                                        uploads.abort();
                                        state.push(error.to_string(), TranscriptTone::Error);
                                    }
                                }
                                Err(error) => state.push(error, TranscriptTone::Error),
                            }
                            state.upload_in_progress = uploads.is_active();
                        } else {
                            match frame.message {
                            ServerMessage::AgentEvent {
                                session_id: actual,
                                record,
                            } if actual == session_id => {
                                handle_gateway_event(&mut state, record);
                            }
                            ServerMessage::SessionHistory {
                                session_id: actual,
                                records,
                                ..
                            } if actual == session_id => {
                                handle_gateway_history(&mut state, records);
                            }
                            ServerMessage::SessionOpened { payload, .. } => {
                                *session = payload;
                                exit = FrontendExit::Reload;
                                break 'ui;
                            }
                            ServerMessage::Ready { payload } => {
                                *gateway = payload;
                                sync_gateway_models(&mut state, &mut catalog, gateway);
                            }
                            ServerMessage::Sessions { sessions, .. } => {
                                gateway.sessions = sessions;
                            }
                            ServerMessage::SessionChanged { payload }
                                if payload.session.session_id == session_id
                                    && payload.config.revision >= session.config.revision =>
                            {
                                if payload.workspace.id == session.workspace.id
                                    && payload.contributions == session.contributions
                                {
                                    refresh_session(&mut state, session, payload, gateway);
                                } else {
                                    *session = payload;
                                    exit = FrontendExit::Resume(session_id.clone());
                                    break 'ui;
                                }
                            }
                            message => {
                                if let Some(message) = render_response(
                                    &message,
                                    &gateway.provider_instances,
                                ) {
                                    state.push(message, TranscriptTone::Neutral);
                                }
                            }
                            }
                        }
                        if let Some(request) = state.requested_resume.take() {
                            clear_on_exit = true;
                            exit = FrontendExit::Resume(request.session_id);
                            break 'ui;
                        }
                    }
                    Ok(None) => {
                        uploads.abort();
                        state.upload_in_progress = false;
                        replay_hydration.finish();
                        events_open = false;
                        state.disconnected = true;
                        state.finish_turn();
                        state.push("gateway disconnected · press q to exit", TranscriptTone::Error);
                    }
                    Err(error) => {
                        uploads.abort();
                        state.upload_in_progress = false;
                        replay_hydration.finish();
                        events_open = false;
                        state.disconnected = true;
                        state.finish_turn();
                        state.push(error.to_string(), TranscriptTone::Error);
                    }
                }
                dirty = true;
            }
            _ = tick.tick() => {
                for _ in 0..MAX_INPUT_BATCH {
                    let Some(event) = poll_event()? else {
                        break;
                    };
                    let action = match event {
                        TerminalEvent::Key(key) => {
                            dirty |= matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat);
                            state.handle_key(key, &catalog)
                        }
                        TerminalEvent::Paste(text) => {
                            if state.preview.is_none() && state.picker.is_none() {
                                let before = (state.input.len(), state.input_limit_reached);
                                state.insert_paste(&text);
                                dirty |=
                                    before != (state.input.len(), state.input_limit_reached);
                            }
                            UiAction::None
                        }
                        TerminalEvent::Resize(_, _) => {
                            dirty = true;
                            UiAction::None
                        }
                        TerminalEvent::Mouse(mouse) => {
                            dirty |= state.handle_mouse(mouse);
                            UiAction::None
                        }
                        TerminalEvent::FocusGained
                        | TerminalEvent::FocusLost => UiAction::None,
                    };
                    match action {
                        UiAction::None => {}
                        UiAction::PasteClipboard => {
                            if !catalog.accepts_file_attachments() {
                                state.push(
                                    "file attachments are not enabled for this chat",
                                    TranscriptTone::Warning,
                                );
                            } else if state.active_turn.is_some() {
                                state.push(
                                    "files can be pasted when the agent is idle",
                                    TranscriptTone::Warning,
                                );
                            } else if state.disconnected {
                                state.push("gateway is disconnected", TranscriptTone::Error);
                            } else if uploads.is_active() {
                                state.push(
                                    "an attachment upload is already in progress",
                                    TranscriptTone::Warning,
                                );
                            } else {
                                match read_clipboard(
                                    &state.attachments,
                                    &gateway.session_file_limits,
                                )
                                    .and_then(|candidates| uploads.start(candidates, &session_id))
                                {
                                    Ok(message) => {
                                        state.upload_in_progress = true;
                                        if let Err(error) = sender.send(message).await {
                                            uploads.abort();
                                            state.upload_in_progress = false;
                                            state.push(error.to_string(), TranscriptTone::Error);
                                        }
                                    }
                                    Err(error) => state.push(error, TranscriptTone::Error),
                                }
                            }
                        }
                        UiAction::Exit => {
                            if let Some(turn_id) = state.active_turn.clone() {
                                let _ = send_op(&sender, &session_id, Op::Interrupt { turn_id }).await;
                            }
                            break 'ui;
                        }
                        UiAction::New => {
                            exit = FrontendExit::New;
                            break 'ui;
                        }
                        UiAction::Clear => {
                            clear_on_exit = true;
                            exit = FrontendExit::New;
                            break 'ui;
                        }
                        UiAction::Submit(op) => {
                            if let Err(error) = send_op(&sender, &session_id, op).await {
                                state.push(error.to_string(), TranscriptTone::Error);
                            }
                        }
                        UiAction::Gateway(action) => match prepare(action) {
                            Ok(message) => {
                                if let Err(error) = sender.send(*message).await {
                                    state.push(error.to_string(), TranscriptTone::Error);
                                }
                            }
                            Err(error) => state.push(error.to_string(), TranscriptTone::Error),
                        },
                        UiAction::GatewaySettings => {
                            match gateway::run(&mut terminal, &gateway_endpoint).await {
                                Ok(true) => {
                                    exit = FrontendExit::Reconnect;
                                    break 'ui;
                                }
                                Ok(false) => {}
                                Err(error) => {
                                    state.push(error.to_string(), TranscriptTone::Error);
                                }
                            }
                            dirty = true;
                        }
                        UiAction::Extensions => {
                            let result = extensions::run(
                                &mut terminal,
                                &sender,
                                &mut events,
                                gateway,
                            )
                            .await;
                            sync_gateway_models(&mut state, &mut catalog, gateway);
                            sync_session(&mut state, session, gateway);
                            if let Err(error) = result {
                                state.push(error.to_string(), TranscriptTone::Error);
                            }
                            dirty = true;
                        }
                        UiAction::Setup { mode, provider } => {
                            let workspace = session.workspace.id.clone();
                            let selected = session.session.session_id.clone();
                            let contributions = session.contributions.clone();
                            let result = setup::run(
                                &mut terminal,
                                mode,
                                provider.as_deref(),
                                &sender,
                                &mut events,
                                gateway,
                                session,
                            )
                            .await;
                            if session.workspace.id != workspace
                                || session.session.session_id != selected
                                || session.contributions != contributions
                            {
                                exit = FrontendExit::Resume(
                                    session.session.session_id.clone(),
                                );
                                break 'ui;
                            }
                            sync_gateway_models(&mut state, &mut catalog, gateway);
                            sync_session(&mut state, session, gateway);
                            if let Err(error) = result {
                                state.push(error.to_string(), TranscriptTone::Error);
                            }
                            dirty = true;
                        }
                    }
                }
            }
            _ = elapsed.tick(), if state.active_turn.is_some() => {
                dirty = true;
            }
            result = &mut workspace_inventory, if workspace_inventory_pending => {
                let _ = result;
                workspace_inventory_pending = false;
                state.reference_cache = None;
                dirty = true;
            }
        }
        guard.set_mouse_capture(state.preview.is_none())?;
    }
    drop(terminal);
    drop(guard);
    if clear_on_exit {
        execute!(io::stdout(), Print(CLEAR_SCREEN_AND_SCROLLBACK))?;
    }
    Ok((exit, sender, events))
}

fn refresh_session(
    state: &mut TuiState,
    session: &mut SessionReadyPayload,
    payload: SessionReadyPayload,
    gateway: &ReadyPayload,
) {
    sync_session(state, &payload, gateway);
    *session = payload;
}

fn sync_session(state: &mut TuiState, session: &SessionReadyPayload, gateway: &ReadyPayload) {
    state.model.model = super::terminal_text(&session.session.model.model);
    state.model.reasoning_effort = session
        .session
        .model
        .reasoning_effort
        .as_deref()
        .map(super::terminal_text);
    state.model_route.clone_from(&session.session.model.route);
    state.agent_summary = agent_summary(gateway, session);
    state.context_limit = session.context_limit_tokens;
    state.usage.apply_context_limit(state.context_limit);
}

fn agent_summary(gateway: &ReadyPayload, session: &SessionReadyPayload) -> String {
    let providers = gateway
        .provider_instances
        .iter()
        .filter(|entry| entry.configured)
        .map(|entry| entry.label.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let middleware = gateway
        .middleware_features
        .iter()
        .filter(|feature| feature.required || session.config.config.middleware.enabled(&feature.id))
        .map(|feature| feature.label.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let counts = session
        .contributions
        .iter()
        .filter_map(|contribution| {
            contribution.count.map(|count| {
                let label = gateway
                    .middleware_features
                    .iter()
                    .find(|feature| feature.id == contribution.capability)
                    .map_or(contribution.capability.as_str(), |feature| {
                        feature.label.as_str()
                    });
                format!("{label}: {count}")
            })
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let reasoning = session
        .session
        .model
        .reasoning_effort
        .as_deref()
        .unwrap_or("default");
    format!(
        "MÖBIUS AGENT\nmodel: {} · {reasoning}\nproviders: {}\nmiddleware: {}\n{}tools: {}\nworkspace: {}",
        super::terminal_text(&session.session.model.model),
        if providers.is_empty() {
            "none"
        } else {
            &providers
        },
        if middleware.is_empty() {
            "none"
        } else {
            &middleware
        },
        if counts.is_empty() {
            String::new()
        } else {
            format!("{counts} · ")
        },
        session.tool_count,
        super::terminal_text(&session.workspace.path.display().to_string()),
    )
}

fn sync_gateway_models(state: &mut TuiState, catalog: &mut UiCatalog, gateway: &ReadyPayload) {
    state.model_choices.clone_from(&gateway.models);
    catalog.replace_model_choices(&gateway.models);
}

async fn send_op(
    sender: &mobius_gateway::client::GatewaySender,
    session_id: &str,
    op: Op,
) -> Result<()> {
    sender
        .send(ClientMessage::Submit {
            session_id: session_id.into(),
            submission: Submission {
                id: Uuid::new_v4().to_string(),
                op,
            },
        })
        .await
        .map_err(|error| Error::Stopped(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobius::protocol::{Event, EventMsg};
    use mobius_gateway::wire::RecordedEvent;

    fn replay_event(sequence: u64) -> ServerMessage {
        ServerMessage::AgentEvent {
            session_id: "session-a".into(),
            record: RecordedEvent {
                sequence,
                recorded_at_ms: 0,
                event: Event {
                    submission_id: None,
                    msg: EventMsg::ContextCompacted,
                },
                stream_metrics: Vec::new(),
                blocks: Vec::new(),
                preview: None,
            },
        }
    }

    #[test]
    fn replay_hydration_blocks_draw_until_current_session_completes() {
        let mut hydration = ReplayHydration::pending();

        for sequence in 1..=3 {
            hydration.observe(&replay_event(sequence), "session-a");
            assert!(!hydration.allows_draw());
        }
        hydration.observe(
            &ServerMessage::SessionReplayComplete {
                request_id: "other-request".into(),
                session_id: "session-b".into(),
            },
            "session-a",
        );
        assert!(!hydration.allows_draw());

        hydration.observe(
            &ServerMessage::SessionReplayComplete {
                request_id: "open-request".into(),
                session_id: "session-a".into(),
            },
            "session-a",
        );
        assert!(hydration.allows_draw());

        hydration.observe(&replay_event(4), "session-a");
        assert!(hydration.allows_draw());
    }
}
