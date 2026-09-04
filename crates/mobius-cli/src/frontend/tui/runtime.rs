use std::io;
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
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
use crate::frontend::bots;
use crate::frontend::catalog::{GatewayAction, UiCatalog};
use crate::frontend::dashboard::render_capability_overlay;
use crate::frontend::extensions;
use crate::frontend::gateway;
use crate::frontend::gateway_actions::{prepare, render_response};
use crate::frontend::setup;
use crate::frontend::terminal::{INPUT_POLL, MAX_INPUT_BATCH, TerminalGuard, poll_event};
use mobius::backend::checkpoint::ExecutionOutcome;
use mobius::protocol::{
    ActiveMessageDelivery, EventMsg, FrontendEvent, FrontendSettingKind, FrontendSettingValue,
    MiddlewareFeature, ModelInfo, Op, Submission,
};
use mobius::{Error, Result};
use mobius_gateway::client::{GatewayEvents, GatewaySender};
use mobius_gateway::wire::{
    BotRecord, ClientMessage, MiddlewareConfig, ReadyPayload, ServerMessage, SessionActivityState,
    SessionReadyPayload, SessionRecord, SwarmRecord, WorkspaceFileScope,
};
use uuid::Uuid;

const ELAPSED_INTERVAL: Duration = Duration::from_secs(1);
const CLEAR_SCREEN_AND_SCROLLBACK: &str = "\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H";

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

struct ReplayHydration {
    pending: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct PendingSessionCreation {
    request_id: String,
    clear: bool,
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
    gateway_endpoint: String,
) -> Result<(FrontendExit, GatewaySender, GatewayEvents)> {
    let mut guard = TerminalGuard::alternate()?;
    let mut terminal = TuiTerminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let model = ModelInfo {
        model: session.session.model.model.clone(),
        reasoning_effort: session.session.model.reasoning_effort.clone(),
    };
    let model_route = session.session.model.route.clone();
    let session_id = session.session.session_id.clone();
    let bot = session_bot(gateway, session)?;
    let mut state = TuiState::new(
        &catalog,
        catalog.workspace().to_path_buf(),
        model,
        model_route,
        agent_summary(gateway, session, bot),
    );
    state.active_message_delivery = Some(composer_message_delivery(
        &gateway.middleware_features,
        &bot.config.config.middleware,
    ));
    let mut uploads = ClipboardUploads::default();
    state.context_limit = session.context_limit_tokens;
    let mut tick = tokio::time::interval(INPUT_POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut elapsed = tokio::time::interval(ELAPSED_INTERVAL);
    elapsed.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut events_open = true;
    let mut dirty = true;
    let mut replay_hydration = ReplayHydration::pending();
    let exit;
    let mut clear_on_exit = false;
    let mut pending_session_creation = None;
    let mut workspace_reference_open = false;
    request_workspace_inventory(&sender, &session_id, &mut state).await;

    'ui: loop {
        draw_if_dirty(
            &mut terminal,
            &mut state,
            &catalog,
            &mut dirty,
            &replay_hydration,
        )?;
        tokio::select! {
            event = events.next(), if events_open => {
                match event {
                    Ok(Some(frame)) => {
                        replay_hydration.observe(&frame.message, &session_id);
                        let message = match frame.message {
                            ServerMessage::WorkspaceFiles { session_id: actual, files, .. }
                                if actual == session_id => {
                                catalog.set_workspace_paths(files.into_iter().map(|file| file.path));
                                state.reference_cache = None;
                                dirty = true;
                                continue 'ui;
                            }
                            message => message,
                        };
                        if replay_hydration.allows_draw()
                            && refresh_workspace_inventory(&message, &session_id, workspace_reference_open)
                        {
                            request_workspace_inventory(&sender, &session_id, &mut state).await;
                        }
                        if let Some((next_exit, should_clear)) = handle_incoming_message(
                            message,
                            &sender,
                            gateway,
                            session,
                            &mut state,
                            &mut uploads,
                            &mut pending_session_creation,
                        )
                        .await
                        {
                            clear_on_exit |= should_clear;
                            exit = next_exit;
                            break 'ui;
                        }
                    }
                    Ok(None) => {
                        disconnect(
                            &mut state,
                            &mut uploads,
                            &mut replay_hydration,
                            "gateway disconnected · press q to exit",
                        );
                        events_open = false;
                    }
                    Err(error) => {
                        disconnect(
                            &mut state,
                            &mut uploads,
                            &mut replay_hydration,
                            error.to_string(),
                        );
                        events_open = false;
                    }
                }
                dirty = true;
            }
            _ = tick.tick() => {
                if let Some(next_exit) = handle_terminal_input(
                    &mut terminal,
                    &sender,
                    &mut events,
                    gateway,
                    session,
                    &catalog,
                    &gateway_endpoint,
                    &session_id,
                    &mut state,
                    &mut uploads,
                    &mut pending_session_creation,
                    &mut dirty,
                ).await? {
                    exit = next_exit;
                    break 'ui;
                }
                let reference_open = !state.reference_menu_dismissed
                    && super::references::active_reference_token(&state.input, state.cursor, '@').is_some();
                if events_open && reference_open && !workspace_reference_open {
                    request_workspace_inventory(&sender, &session_id, &mut state).await;
                }
                workspace_reference_open = reference_open;
            }
            _ = elapsed.tick(), if state.active_turn.is_some() => {
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

async fn request_workspace_inventory(
    sender: &GatewaySender,
    session_id: &str,
    state: &mut TuiState,
) {
    if let Err(error) = sender
        .send(ClientMessage::ListWorkspaceFiles {
            request_id: Uuid::new_v4().to_string(),
            session_id: session_id.into(),
            scope: WorkspaceFileScope::All,
        })
        .await
    {
        state.push(error.to_string(), TranscriptTone::Error);
    }
}

fn refresh_workspace_inventory(
    message: &ServerMessage,
    session_id: &str,
    reference_open: bool,
) -> bool {
    matches!(message, ServerMessage::AgentEvent { session_id: actual, record }
        if actual == session_id && (matches!(record.event.msg, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_))
            || reference_open && matches!(record.event.msg, EventMsg::ToolCallEnd(_))))
}

#[allow(clippy::too_many_arguments)]
async fn handle_terminal_input(
    terminal: &mut TuiTerminal,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    session: &mut SessionReadyPayload,
    catalog: &UiCatalog,
    gateway_endpoint: &str,
    session_id: &str,
    state: &mut TuiState,
    uploads: &mut ClipboardUploads,
    pending_session_creation: &mut Option<PendingSessionCreation>,
    dirty: &mut bool,
) -> Result<Option<FrontendExit>> {
    for _ in 0..MAX_INPUT_BATCH {
        let Some(event) = poll_event()? else { break };
        let action = terminal_action(event, state, catalog, dirty);
        match action {
            UiAction::None => {}
            UiAction::PasteClipboard => {
                paste_clipboard(catalog, gateway, sender, session_id, state, uploads).await;
            }
            UiAction::Exit => {
                interrupt_active_turn(sender, session_id, state).await;
                return Ok(Some(FrontendExit::Exit));
            }
            UiAction::ChooseBot { workspace, clear } => {
                choose_bot(gateway, session, workspace, clear, state);
                *dirty = true;
            }
            UiAction::CreateSession {
                workspace,
                bot_id,
                clear,
            } => {
                *pending_session_creation =
                    create_session(sender, workspace, bot_id, clear, state).await;
            }
            UiAction::Submit(op) => send_and_report(sender, session_id, op, state).await,
            UiAction::Gateway(action) => send_gateway_action(sender, action, state).await,
            UiAction::GatewaySettings => {
                if open_gateway_settings(terminal, gateway_endpoint, state).await {
                    return Ok(Some(FrontendExit::Reconnect));
                }
                *dirty = true;
            }
            UiAction::Extensions => {
                open_extensions(terminal, sender, events, gateway, session, state).await;
                *dirty = true;
            }
            UiAction::Bots => {
                if open_bots(
                    terminal, sender, events, gateway, session, state, session_id,
                )
                .await
                {
                    return Ok(Some(FrontendExit::Reload));
                }
                *dirty = true;
            }
            UiAction::Setup { mode, provider } => {
                let (result, session_changed) = run_setup(
                    terminal,
                    mode,
                    provider.as_deref(),
                    sender,
                    events,
                    gateway,
                    session,
                )
                .await;
                if session_changed {
                    return Ok(Some(FrontendExit::Resume(
                        session.session.session_id.clone(),
                    )));
                }
                if let Err(error) = sync_session(state, session, gateway) {
                    state.push(error.to_string(), TranscriptTone::Error);
                }
                if let Err(error) = result {
                    state.push(error.to_string(), TranscriptTone::Error);
                }
                *dirty = true;
            }
        }
    }
    Ok(None)
}

async fn handle_incoming_message(
    message: ServerMessage,
    sender: &GatewaySender,
    gateway: &mut ReadyPayload,
    session: &mut SessionReadyPayload,
    state: &mut TuiState,
    uploads: &mut ClipboardUploads,
    pending_session_creation: &mut Option<PendingSessionCreation>,
) -> Option<(FrontendExit, bool)> {
    let session_id = session.session.session_id.clone();
    let should_clear = settle_session_creation(&message, pending_session_creation);
    if !handle_upload_message(&message, sender, gateway, state, uploads, &session_id).await
        && let Some(exit) = handle_server_message(message, gateway, session, state, &session_id)
    {
        return Some((exit, should_clear));
    }
    state
        .requested_resume
        .take()
        .map(|request| (FrontendExit::Resume(request.session_id), true))
}

fn choose_bot(
    gateway: &ReadyPayload,
    session: &SessionReadyPayload,
    workspace: std::path::PathBuf,
    clear: bool,
    state: &mut TuiState,
) {
    if gateway.bots.is_empty() {
        state.push(
            "create a Bot before starting another chat",
            TranscriptTone::Warning,
        );
        return;
    }
    state.open_bot_picker(
        &gateway.bots,
        workspace,
        &session.session.context.bot_id,
        clear,
    );
}

async fn create_session(
    sender: &GatewaySender,
    workspace: std::path::PathBuf,
    bot_id: String,
    clear: bool,
    state: &mut TuiState,
) -> Option<PendingSessionCreation> {
    let request_id = Uuid::new_v4().to_string();
    let result = sender
        .send(ClientMessage::CreateSession {
            request_id: request_id.clone(),
            workspace,
            bot_id,
        })
        .await;
    if let Err(error) = result {
        state.push(error.to_string(), TranscriptTone::Error);
        return None;
    }
    Some(PendingSessionCreation { request_id, clear })
}

fn settle_session_creation(
    message: &ServerMessage,
    pending: &mut Option<PendingSessionCreation>,
) -> bool {
    let Some(request) = pending.as_ref() else {
        return false;
    };
    let clear = match message {
        ServerMessage::SessionOpened { request_id, .. } if request_id == &request.request_id => {
            request.clear
        }
        ServerMessage::Rejected { request_id, .. } if request_id == &request.request_id => false,
        _ => return false,
    };
    *pending = None;
    clear
}

fn draw_if_dirty(
    terminal: &mut TuiTerminal,
    state: &mut TuiState,
    catalog: &UiCatalog,
    dirty: &mut bool,
    replay_hydration: &ReplayHydration,
) -> Result<()> {
    if *dirty && replay_hydration.allows_draw() {
        draw(terminal, state, catalog)?;
        *dirty = false;
    }
    Ok(())
}

fn draw(terminal: &mut TuiTerminal, state: &mut TuiState, catalog: &UiCatalog) -> Result<()> {
    io::stdout().sync_update(|_| -> Result<()> {
        terminal.draw(|frame| {
            super::view::render(frame, state, catalog);
            if state.preview.is_some() {
                render_preview(frame, state);
            } else if let Some(overlay) = state.capability_overlay.as_mut() {
                render_capability_overlay(frame, overlay);
            }
        })?;
        Ok(())
    })??;
    Ok(())
}

async fn handle_upload_message(
    message: &ServerMessage,
    sender: &GatewaySender,
    gateway: &ReadyPayload,
    state: &mut TuiState,
    uploads: &mut ClipboardUploads,
    session_id: &str,
) -> bool {
    let Some(result) = uploads
        .handle(message, session_id, &gateway.session_file_limits)
        .await
    else {
        return false;
    };
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
    true
}

fn handle_server_message(
    message: ServerMessage,
    gateway: &mut ReadyPayload,
    session: &mut SessionReadyPayload,
    state: &mut TuiState,
    session_id: &str,
) -> Option<FrontendExit> {
    match message {
        ServerMessage::AgentEvent {
            session_id: actual,
            mut record,
        } if actual == session_id => {
            enrich_resume_picker(
                &mut record.event.msg,
                &gateway.sessions,
                &gateway.bots,
                &gateway.swarms,
                &session.workspace.id,
            );
            handle_gateway_event(state, record);
        }
        ServerMessage::SessionHistory {
            session_id: actual,
            mut records,
            ..
        } if actual == session_id => {
            for record in &mut records {
                enrich_resume_picker(
                    &mut record.event.msg,
                    &gateway.sessions,
                    &gateway.bots,
                    &gateway.swarms,
                    &session.workspace.id,
                );
            }
            handle_gateway_history(state, records);
        }
        ServerMessage::SessionOpened { payload, .. } => {
            *session = payload;
            return Some(FrontendExit::Reload);
        }
        ServerMessage::Ready { payload } => {
            *gateway = payload;
            if let Err(error) = sync_session(state, session, gateway) {
                state.push(error.to_string(), TranscriptTone::Error);
            }
        }
        ServerMessage::Sessions { sessions, .. } => gateway.sessions = sessions,
        ServerMessage::Bots { bots, .. } => {
            gateway.bots = bots;
            if let Err(error) = sync_session(state, session, gateway) {
                state.push(error.to_string(), TranscriptTone::Error);
            }
        }
        ServerMessage::Swarms { swarms, .. } => gateway.swarms = swarms,
        ServerMessage::SessionChanged { payload } if payload.session.session_id == session_id => {
            if payload.workspace.id == session.workspace.id
                && payload.contributions == session.contributions
            {
                if let Err(error) = refresh_session(state, session, payload, gateway) {
                    state.push(error.to_string(), TranscriptTone::Error);
                }
            } else {
                *session = payload;
                return Some(FrontendExit::Resume(session_id.into()));
            }
        }
        message => {
            if let Some(message) = render_response(&message, &gateway.provider_instances) {
                state.push(message, TranscriptTone::Neutral);
            }
        }
    }
    None
}

fn disconnect(
    state: &mut TuiState,
    uploads: &mut ClipboardUploads,
    replay_hydration: &mut ReplayHydration,
    message: impl AsRef<str>,
) {
    uploads.abort();
    state.upload_in_progress = false;
    replay_hydration.finish();
    state.disconnected = true;
    state.finish_turn();
    state.push(message, TranscriptTone::Error);
}

fn terminal_action(
    event: TerminalEvent,
    state: &mut TuiState,
    catalog: &UiCatalog,
    dirty: &mut bool,
) -> UiAction {
    match event {
        TerminalEvent::Key(key) => {
            *dirty |= matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat);
            state.handle_key(key, catalog)
        }
        TerminalEvent::Paste(text) => {
            if state.capability_overlay.is_some() {
                *dirty |= state.insert_capability_overlay_paste(&text);
            } else if state.preview.is_none() && state.picker.is_none() {
                let before = (state.input.len(), state.input_limit_reached);
                state.insert_paste(&text);
                *dirty |= before != (state.input.len(), state.input_limit_reached);
            }
            UiAction::None
        }
        TerminalEvent::Resize(_, _) => {
            *dirty = true;
            UiAction::None
        }
        TerminalEvent::Mouse(mouse) => {
            *dirty |= state.handle_mouse(mouse);
            UiAction::None
        }
        TerminalEvent::FocusGained | TerminalEvent::FocusLost => UiAction::None,
    }
}

async fn paste_clipboard(
    catalog: &UiCatalog,
    gateway: &ReadyPayload,
    sender: &GatewaySender,
    session_id: &str,
    state: &mut TuiState,
    uploads: &mut ClipboardUploads,
) {
    if !catalog.accepts_file_attachments() {
        state.push(
            "file attachments are not enabled for this chat",
            TranscriptTone::Warning,
        );
        return;
    }
    if state.active_turn.is_some() {
        state.push(
            "files can be pasted when the agent is idle",
            TranscriptTone::Warning,
        );
        return;
    }
    if state.disconnected {
        state.push("gateway is disconnected", TranscriptTone::Error);
        return;
    }
    if uploads.is_active() {
        state.push(
            "an attachment upload is already in progress",
            TranscriptTone::Warning,
        );
        return;
    }
    match read_clipboard(&state.attachments, &gateway.session_file_limits)
        .and_then(|candidates| uploads.start(candidates, session_id))
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

async fn interrupt_active_turn(sender: &GatewaySender, session_id: &str, state: &TuiState) {
    if let Some(turn_id) = state.active_turn.clone() {
        let _ = send_op(sender, session_id, Op::Interrupt { turn_id }).await;
    }
}

async fn send_and_report(sender: &GatewaySender, session_id: &str, op: Op, state: &mut TuiState) {
    if let Err(error) = send_op(sender, session_id, op).await {
        state.push(error.to_string(), TranscriptTone::Error);
    }
}

async fn send_gateway_action(sender: &GatewaySender, action: GatewayAction, state: &mut TuiState) {
    match prepare(action) {
        Ok(message) => {
            if let Err(error) = sender.send(*message).await {
                state.push(error.to_string(), TranscriptTone::Error);
            }
        }
        Err(error) => state.push(error.to_string(), TranscriptTone::Error),
    }
}

async fn open_gateway_settings(
    terminal: &mut TuiTerminal,
    gateway_endpoint: &str,
    state: &mut TuiState,
) -> bool {
    match gateway::run(terminal, gateway_endpoint).await {
        Ok(reconnect) => reconnect,
        Err(error) => {
            state.push(error.to_string(), TranscriptTone::Error);
            false
        }
    }
}

async fn open_extensions(
    terminal: &mut TuiTerminal,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    session: &SessionReadyPayload,
    state: &mut TuiState,
) {
    let result = extensions::run(terminal, sender, events, gateway).await;
    if let Err(error) = sync_session(state, session, gateway) {
        state.push(error.to_string(), TranscriptTone::Error);
    }
    if let Err(error) = result {
        state.push(error.to_string(), TranscriptTone::Error);
    }
}

async fn open_bots(
    terminal: &mut TuiTerminal,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    session: &SessionReadyPayload,
    state: &mut TuiState,
    session_id: &str,
) -> bool {
    let bot_id = session.session.context.bot_id.clone();
    let result = bots::run(
        terminal,
        sender,
        events,
        gateway,
        Some(&bot_id),
        Some(&bot_id),
    )
    .await;
    if !gateway
        .sessions
        .iter()
        .any(|candidate| candidate.session_id == session_id)
    {
        return true;
    }
    if let Err(error) = sync_session(state, session, gateway) {
        state.push(error.to_string(), TranscriptTone::Error);
    }
    if let Err(error) = result {
        state.push(error.to_string(), TranscriptTone::Error);
    }
    false
}

async fn run_setup(
    terminal: &mut TuiTerminal,
    mode: setup::SetupMode,
    provider: Option<&str>,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    session: &mut SessionReadyPayload,
) -> (Result<()>, bool) {
    let workspace = session.workspace.id.clone();
    let selected = session.session.session_id.clone();
    let contributions = session.contributions.clone();
    let result = setup::run(terminal, mode, provider, sender, events, gateway, session).await;
    let changed = session.workspace.id != workspace
        || session.session.session_id != selected
        || session.contributions != contributions;
    (result, changed)
}

fn refresh_session(
    state: &mut TuiState,
    session: &mut SessionReadyPayload,
    payload: SessionReadyPayload,
    gateway: &ReadyPayload,
) -> Result<()> {
    sync_session(state, &payload, gateway)?;
    *session = payload;
    Ok(())
}

fn sync_session(
    state: &mut TuiState,
    session: &SessionReadyPayload,
    gateway: &ReadyPayload,
) -> Result<()> {
    let bot = session_bot(gateway, session)?;
    state.model.model = super::terminal_text(&session.session.model.model);
    state.model.reasoning_effort = session
        .session
        .model
        .reasoning_effort
        .as_deref()
        .map(super::terminal_text);
    state.model_route.clone_from(&session.session.model.route);
    state.agent_summary = agent_summary(gateway, session, bot);
    state.active_message_delivery = Some(composer_message_delivery(
        &gateway.middleware_features,
        &bot.config.config.middleware,
    ));
    state.context_limit = session.context_limit_tokens;
    state.usage.apply_context_limit(state.context_limit);
    Ok(())
}

fn composer_message_delivery(
    features: &[MiddlewareFeature],
    config: &MiddlewareConfig,
) -> ActiveMessageDelivery {
    for feature in features {
        for setting in &feature.settings {
            let FrontendSettingKind::Select { options, .. } = &setting.kind else {
                continue;
            };
            if !setting.composer
                || options.len() != 2
                || !options.iter().any(|option| option.value == "steer")
                || !options.iter().any(|option| option.value == "queue")
            {
                continue;
            }
            let Some(FrontendSettingValue::String(value)) =
                config.setting(&feature.id, &setting.id)
            else {
                continue;
            };
            return match value.as_str() {
                "queue" => ActiveMessageDelivery::Queue,
                _ => ActiveMessageDelivery::Steer,
            };
        }
    }
    ActiveMessageDelivery::Steer
}

fn enrich_resume_picker(
    event: &mut EventMsg,
    sessions: &[SessionRecord],
    bots: &[BotRecord],
    swarms: &[SwarmRecord],
    current_workspace_id: &str,
) {
    let EventMsg::Frontend(FrontendEvent::Picker { options, .. }) = event else {
        return;
    };
    for option in options {
        let Op::ResumeSession { session_id } = &option.op else {
            continue;
        };
        let Some(session) = sessions
            .iter()
            .find(|session| session.session_id == *session_id)
        else {
            continue;
        };
        if let Some(title) = &session.title {
            option.label.clone_from(title);
        }
        let mut details = vec![session_status(session).into()];
        details.push(
            if session.session_context.workspace_id.as_deref() == Some(current_workspace_id) {
                "this workspace"
            } else {
                session
                    .session_context
                    .workspace_label
                    .as_deref()
                    .unwrap_or("other workspace")
            }
            .into(),
        );
        if let Some(origin) = &session.session_context.origin_label {
            details.push(origin.clone());
        }
        let bot_id = &session.session_context.bot_id;
        if let Some(handle) = bot_handle(bot_id, bots) {
            details.push(format!("@{handle}"));
        }
        if let Some(swarm) = swarm_label(bot_id, swarms) {
            details.push(swarm);
        }
        details.push(format!("started {}", human_time(session.created_at)));
        option.description = details.join(" · ");
    }
}

fn session_status(session: &SessionRecord) -> &'static str {
    match session.activity.state {
        SessionActivityState::Running => "running",
        SessionActivityState::AwaitingApproval => "awaiting approval",
        SessionActivityState::Idle => match session.activity.last_outcome {
            Some(ExecutionOutcome::Completed) => "done",
            Some(ExecutionOutcome::Aborted) => "aborted",
            Some(ExecutionOutcome::Failed) => "failed",
            None if session.execution_stats.run_count == 0 => "new",
            None => "idle",
        },
    }
}

fn swarm_label(bot_id: &str, swarms: &[SwarmRecord]) -> Option<String> {
    swarms.iter().find_map(|swarm| {
        if !swarm.members.iter().any(|member| member.bot_id == bot_id) {
            return None;
        }
        Some(format!(
            "swarm {}{}",
            swarm.title,
            if swarm.leader_bot_id == bot_id {
                " (leader)"
            } else {
                ""
            }
        ))
    })
}

fn bot_handle<'a>(bot_id: &str, bots: &'a [BotRecord]) -> Option<&'a str> {
    bots.iter()
        .find(|bot| bot.id == bot_id)
        .map(|bot| bot.handle.as_str())
}

fn session_bot<'a>(
    gateway: &'a ReadyPayload,
    session: &SessionReadyPayload,
) -> Result<&'a BotRecord> {
    gateway
        .bots
        .iter()
        .find(|bot| bot.id == session.session.context.bot_id)
        .ok_or_else(|| {
            Error::Config(format!(
                "session {} references unknown Bot {}",
                session.session.session_id, session.session.context.bot_id
            ))
        })
}

fn human_time(timestamp_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(timestamp_ms).map_or_else(
        || "unknown time".into(),
        |time| {
            time.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M %Z")
                .to_string()
        },
    )
}

fn agent_summary(gateway: &ReadyPayload, session: &SessionReadyPayload, bot: &BotRecord) -> String {
    let bot_id = &session.session.context.bot_id;
    let bot_label = format!("@{}", bot.handle);
    let bot_label = swarm_label(bot_id, &gateway.swarms)
        .map_or(bot_label.clone(), |swarm| format!("{bot_label} · {swarm}"));
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
        .filter(|feature| feature.required || bot.config.config.middleware.enabled(&feature.id))
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
        "MÖBIUS · {bot_label}\nmodel: {} · {reasoning}\nproviders: {}\nmiddleware: {}\n{}tools: {}\nworkspace: {}",
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
    use mobius::protocol::{
        Event, EventMsg, FrontendPickerOption, FrontendSetting, FrontendSettingOption,
        FrontendTone, SessionContext,
    };
    use mobius_gateway::wire::{
        RecordedEvent, SessionActivity, SwarmMemberRecord, VersionedAgentConfig,
    };

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
    fn workspace_inventory_refreshes_current_session_changes() {
        use mobius::protocol::{ToolCallEndEvent, TurnAbortedEvent, TurnCompleteEvent};

        let mut message = replay_event(1);
        assert!(!refresh_workspace_inventory(&message, "session-a", true));
        for event in [
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".into(),
            }),
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: "turn-1".into(),
                reason: "cancelled".into(),
            }),
            EventMsg::ToolCallEnd(ToolCallEndEvent {
                turn_id: "turn-1".into(),
                call_id: "call-1".into(),
                name: "write".into(),
                output: String::new(),
                is_error: false,
            }),
        ] {
            let ServerMessage::AgentEvent { record, .. } = &mut message else {
                unreachable!()
            };
            let terminal = !matches!(event, EventMsg::ToolCallEnd(_));
            record.event.msg = event;
            assert!(refresh_workspace_inventory(&message, "session-a", true));
            assert_eq!(
                refresh_workspace_inventory(&message, "session-a", false),
                terminal
            );
            assert!(!refresh_workspace_inventory(&message, "session-b", true));
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

    #[test]
    fn rejected_session_creation_does_not_commit_clear() {
        let mut pending = Some(PendingSessionCreation {
            request_id: "create-1".into(),
            clear: true,
        });
        let rejection = ServerMessage::Rejected {
            request_id: "create-1".into(),
            code: "invalid_bot".into(),
            message: "Bot unavailable".into(),
            fatal: false,
        };

        assert!(!settle_session_creation(&rejection, &mut pending));
        assert_eq!(pending, None);
    }

    #[test]
    fn composer_delivery_uses_the_advertised_session_setting() {
        let features = [MiddlewareFeature {
            id: "messages".into(),
            label: "Messages".into(),
            description: String::new(),
            required: true,
            settings: vec![FrontendSetting {
                id: "delivery".into(),
                label: "Delivery".into(),
                description: String::new(),
                composer: true,
                kind: FrontendSettingKind::Select {
                    options: vec![
                        FrontendSettingOption {
                            value: "steer".into(),
                            label: "Steer".into(),
                            description: String::new(),
                            symbol: None,
                            tone: FrontendTone::Neutral,
                        },
                        FrontendSettingOption {
                            value: "queue".into(),
                            label: "Queue".into(),
                            description: String::new(),
                            symbol: None,
                            tone: FrontendTone::Neutral,
                        },
                    ],
                    unset_label: None,
                },
            }],
        }];
        let mut config: MiddlewareConfig = serde_json::from_value(serde_json::json!({
            "enabled": [],
            "settings": {}
        }))
        .expect("middleware config");

        assert_eq!(
            composer_message_delivery(&features, &config),
            ActiveMessageDelivery::Steer
        );
        config.set_setting(
            "messages",
            "delivery",
            Some(FrontendSettingValue::String("queue".into())),
        );
        assert_eq!(
            composer_message_delivery(&features, &config),
            ActiveMessageDelivery::Queue
        );
    }

    #[test]
    fn resume_picker_uses_live_session_and_swarm_metadata() {
        let mut event = EventMsg::Frontend(FrontendEvent::Picker {
            title: "Resume chat".into(),
            options: vec![FrontendPickerOption {
                label: "old label".into(),
                description: "created at Unix time 1700000000000".into(),
                detail: String::new(),
                symbol: None,
                shows_detail: false,
                op: Op::ResumeSession {
                    session_id: "session-a".into(),
                },
            }],
        });
        let sessions = [SessionRecord {
            session_id: "session-a".into(),
            session_context: SessionContext {
                bot_id: "bot-a".into(),
                workspace_id: Some("workspace-a".into()),
                workspace_label: Some("Project A".into()),
                ..SessionContext::default()
            },
            parent_session_id: None,
            parent_sequence: None,
            sequence: 1,
            first_user_message: Some("First message".into()),
            execution_stats: Default::default(),
            title: Some("Named chat".into()),
            pinned: false,
            activity: SessionActivity {
                state: SessionActivityState::Running,
                ..SessionActivity::default()
            },
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
        }];
        let bots = [BotRecord {
            id: "bot-a".into(),
            handle: "curie".into(),
            name: "Curie".into(),
            description: "Own research.".into(),
            tint: Default::default(),
            config: VersionedAgentConfig {
                revision: 1,
                config: Default::default(),
            },
        }];
        let swarms = [SwarmRecord {
            id: "swarm-a".into(),
            title: "Release".into(),
            leader_bot_id: "bot-a".into(),
            members: vec![SwarmMemberRecord {
                bot_id: "bot-a".into(),
                handle: "curie".into(),
            }],
            messages: Vec::new(),
            updated_at_ms: 0,
        }];

        enrich_resume_picker(&mut event, &sessions, &bots, &swarms, "workspace-a");

        let EventMsg::Frontend(FrontendEvent::Picker { options, .. }) = event else {
            panic!("resume picker");
        };
        assert_eq!(options[0].label, "Named chat");
        assert!(
            options[0]
                .description
                .starts_with("running · this workspace")
        );
        assert!(options[0].description.contains("swarm Release (leader)"));
        assert!(options[0].description.contains("started "));
        assert!(!options[0].description.contains("Unix"));
    }
}
