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
use crate::frontend::catalog::{GatewayAction, UiCatalog};
use crate::frontend::extensions;
use crate::frontend::gateway;
use crate::frontend::gateway_actions::{prepare, render_response};
use crate::frontend::setup;
use crate::frontend::terminal::{INPUT_POLL, MAX_INPUT_BATCH, TerminalGuard, poll_event};
use mobius::protocol::{EventMsg, FrontendEvent, ModelInfo, Op, Submission};
use mobius::{Error, Result};
use mobius_gateway::client::{GatewayEvents, GatewaySender};
use mobius_gateway::wire::{
    ClientMessage, ReadyPayload, ServerMessage, SessionActivityState, SessionOutcome,
    SessionReadyPayload, SessionRecord, SwarmRecord,
};
use uuid::Uuid;

const ELAPSED_INTERVAL: Duration = Duration::from_secs(1);
const CLEAR_SCREEN_AND_SCROLLBACK: &str = "\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H";

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

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
    let mut terminal = TuiTerminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    if !ensure_configured(
        &mut terminal,
        &sender,
        &mut events,
        gateway,
        session,
        &mut catalog,
    )
    .await?
    {
        drop(terminal);
        drop(guard);
        return Ok((FrontendExit::Discard, sender, events));
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
                        if !handle_upload_message(
                            &frame.message,
                            &sender,
                            gateway,
                            &mut state,
                            &mut uploads,
                            &session_id,
                        )
                        .await
                            && let Some(next_exit) = handle_server_message(
                                frame.message,
                                gateway,
                                session,
                                &mut catalog,
                                &mut state,
                                &session_id,
                            )
                        {
                            exit = next_exit;
                            break 'ui;
                        }
                        if let Some(request) = state.requested_resume.take() {
                            clear_on_exit = true;
                            exit = FrontendExit::Resume(request.session_id);
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
                for _ in 0..MAX_INPUT_BATCH {
                    let Some(event) = poll_event()? else {
                        break;
                    };
                    let action = terminal_action(event, &mut state, &catalog, &mut dirty);
                    match action {
                        UiAction::None => {}
                        UiAction::PasteClipboard => {
                            paste_clipboard(
                                &catalog,
                                gateway,
                                &sender,
                                &session_id,
                                &mut state,
                                &mut uploads,
                            )
                            .await;
                        }
                        UiAction::Exit => {
                            interrupt_active_turn(&sender, &session_id, &state).await;
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
                            send_and_report(&sender, &session_id, op, &mut state).await;
                        }
                        UiAction::Gateway(action) => {
                            send_gateway_action(&sender, action, &mut state).await;
                        }
                        UiAction::GatewaySettings => {
                            match gateway::run(&mut terminal, &gateway_endpoint).await {
                                Ok(true) => {
                                    exit = FrontendExit::Reconnect;
                                    break 'ui;
                                }
                                Ok(false) => {}
                                Err(error) => state.push(error.to_string(), TranscriptTone::Error),
                            }
                            dirty = true;
                        }
                        UiAction::Extensions => {
                            open_extensions(
                                &mut terminal,
                                &sender,
                                &mut events,
                                gateway,
                                session,
                                &mut catalog,
                                &mut state,
                            )
                            .await;
                            dirty = true;
                        }
                        UiAction::Setup { mode, provider } => {
                            let (result, session_changed) = run_setup(
                                &mut terminal,
                                mode,
                                provider.as_deref(),
                                &sender,
                                &mut events,
                                gateway,
                                session,
                            )
                            .await;
                            if session_changed {
                                exit = FrontendExit::Resume(session.session.session_id.clone());
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

async fn ensure_configured(
    terminal: &mut TuiTerminal,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    session: &mut SessionReadyPayload,
    catalog: &mut UiCatalog,
) -> Result<bool> {
    if gateway.default_config.is_some() && !gateway.models.is_empty() {
        return Ok(true);
    }
    setup::run(
        terminal,
        setup::SetupMode::Login,
        None,
        sender,
        events,
        gateway,
        session,
    )
    .await?;
    if gateway.default_config.is_none() || gateway.models.is_empty() {
        return Ok(false);
    }
    catalog.replace_model_choices(&gateway.models);
    Ok(true)
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
            if state.preview.is_some() {
                render_preview(frame, state);
            } else {
                super::view::render(frame, state, catalog);
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
    catalog: &mut UiCatalog,
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
            sync_gateway_models(state, catalog, gateway);
        }
        ServerMessage::Sessions { sessions, .. } => gateway.sessions = sessions,
        ServerMessage::Swarms { swarms, .. } => gateway.swarms = swarms,
        ServerMessage::SessionChanged { payload }
            if payload.session.session_id == session_id
                && payload.config.revision >= session.config.revision =>
        {
            if payload.workspace.id == session.workspace.id
                && payload.contributions == session.contributions
            {
                refresh_session(state, session, payload, gateway);
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
            if state.preview.is_none() && state.picker.is_none() {
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

async fn open_extensions(
    terminal: &mut TuiTerminal,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
    session: &SessionReadyPayload,
    catalog: &mut UiCatalog,
    state: &mut TuiState,
) {
    let result = extensions::run(terminal, sender, events, gateway).await;
    sync_gateway_models(state, catalog, gateway);
    sync_session(state, session, gateway);
    if let Err(error) = result {
        state.push(error.to_string(), TranscriptTone::Error);
    }
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

fn enrich_resume_picker(
    event: &mut EventMsg,
    sessions: &[SessionRecord],
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
        if let Some(swarm) = swarm_label(session_id, swarms) {
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
            Some(SessionOutcome::Completed) => "done",
            Some(SessionOutcome::Aborted) => "aborted",
            Some(SessionOutcome::Failed) => "failed",
            None if session.execution_stats.run_count == 0 => "new",
            None => "idle",
        },
    }
}

fn swarm_label(session_id: &str, swarms: &[SwarmRecord]) -> Option<String> {
    swarms.iter().find_map(|swarm| {
        let member = swarm
            .members
            .iter()
            .find(|member| member.session_id == session_id)?;
        Some(format!(
            "swarm {} (@{}{})",
            swarm.title,
            member.handle,
            if swarm.leader_session_id == session_id {
                ", leader"
            } else {
                ""
            }
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
        "MÖBIUS\nmodel: {} · {reasoning}\nproviders: {}\nmiddleware: {}\n{}tools: {}\nworkspace: {}",
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
    use mobius::protocol::{Event, EventMsg, FrontendPickerOption, SessionContext};
    use mobius_gateway::wire::{RecordedEvent, SessionActivity, SwarmMemberRecord};

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
        let swarms = [SwarmRecord {
            id: "swarm-a".into(),
            title: "Release".into(),
            leader_session_id: "session-a".into(),
            members: vec![SwarmMemberRecord {
                session_id: "session-a".into(),
                handle: "curie".into(),
            }],
            messages: Vec::new(),
            updated_at_ms: 0,
        }];

        enrich_resume_picker(&mut event, &sessions, &swarms, "workspace-a");

        let EventMsg::Frontend(FrontendEvent::Picker { options, .. }) = event else {
            panic!("resume picker");
        };
        assert_eq!(options[0].label, "Named chat");
        assert!(
            options[0]
                .description
                .starts_with("running · this workspace")
        );
        assert!(
            options[0]
                .description
                .contains("swarm Release (@curie, leader)")
        );
        assert!(options[0].description.contains("started "));
        assert!(!options[0].description.contains("Unix"));
    }
}
