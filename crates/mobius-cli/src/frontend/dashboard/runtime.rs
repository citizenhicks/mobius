use std::io;
use std::path::PathBuf;

use mobius::protocol::{EventMsg, FrontendWidgetContent, MAX_MESSAGE_BYTES, Op, Submission};
use mobius::{Error, Result};
use mobius_gateway::client::{GatewayClient, GatewayEvents, GatewaySender};
use mobius_gateway::wire::{
    ClientKind, ClientMessage, ClientStatus, ReadyPayload, ServerMessage, SessionActivityState,
    SessionRecord,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use super::REFRESH_INTERVAL;
use super::state::{ActionInput, CapabilityOverlay, DashboardFocus, DashboardState};
use super::view::{dashboard_areas, render};
use crate::frontend::setup::{self, SetupMode};
use crate::frontend::terminal::{INPUT_POLL, MAX_INPUT_BATCH, poll_event, terminal_text};
use crate::gateway_accounts::{configured_token, dashboard_gateway_endpoint};

pub(super) async fn connect(
    state_dir: PathBuf,
) -> Result<(GatewaySender, GatewayEvents, DashboardState)> {
    let endpoint = dashboard_gateway_endpoint(&state_dir).map_err(gateway_error)?;
    mobius_gateway::command::ensure_background_gateway(state_dir)
        .await
        .map_err(gateway_error)?;
    let token = configured_token(&endpoint)
        .map_err(gateway_error)?
        .ok_or_else(|| {
            Error::Config(format!(
                "this machine is not paired with {endpoint}; pair it before opening the gateway dashboard"
            ))
        })?;
    let client = GatewayClient::connect(&endpoint, token, ClientKind::GatewayDashboard)
        .await
        .map_err(gateway_error)?;
    let (sender, mut events) = client.into_parts();
    let gateway = wait_ready(&mut events).await?;
    Ok((
        sender,
        events,
        DashboardState {
            endpoint: endpoint.to_string(),
            gateway,
            clients: Vec::new(),
            current_client_id: None,
            selected_client_id: None,
            selected_session_id: None,
            device_list: ListState::default(),
            chat_list: ListState::default(),
            focus: DashboardFocus::Devices,
            pending_unpair: None,
            profile: None,
            pending_open: None,
            overlay: None,
            error: None,
        },
    ))
}

pub(super) async fn wait_ready(events: &mut GatewayEvents) -> Result<ReadyPayload> {
    loop {
        let frame = events
            .next()
            .await
            .map_err(gateway_error)?
            .ok_or_else(|| Error::Stopped("gateway disconnected before it was ready".into()))?;
        match frame.message {
            ServerMessage::Ready { payload } => return Ok(payload),
            ServerMessage::Error { message, .. } => return Err(Error::Stopped(message)),
            _ => {}
        }
    }
}

pub(super) async fn dashboard_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    sender: GatewaySender,
    events: &mut GatewayEvents,
    state: &mut DashboardState,
) -> Result<()> {
    let mut input = tokio::time::interval(INPUT_POLL);
    input.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut refresh = tokio::time::interval(REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    sync_chat_selection(state);
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| render(frame, state))?;
            dirty = false;
        }
        tokio::select! {
            _ = refresh.tick() => {
                request_snapshot(&sender).await?;
            }
            _ = input.tick() => {
                for _ in 0..MAX_INPUT_BATCH {
                    let Some(event) = poll_event()? else { break; };
                    dirty = true;
                    match event {
                        Event::Key(key)
                            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                        {
                            if handle_key(terminal, &sender, events, state, key).await? {
                                return Ok(());
                            }
                        }
                        Event::Paste(text) => handle_overlay_paste(state, &text),
                        Event::Mouse(mouse) => {
                            handle_mouse(state, mouse, terminal.size()?.into());
                        }
                        _ => {}
                    }
                }
            }
            frame = events.next() => {
                let frame = frame
                    .map_err(gateway_error)?
                    .ok_or_else(|| Error::Stopped("gateway disconnected".into()))?;
                handle_frame(state, frame.message)?;
                dirty = true;
            }
        }
    }
}

pub(super) async fn handle_key(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    state: &mut DashboardState,
    key: KeyEvent,
) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'd'))
    {
        return Ok(true);
    }
    if state.overlay.is_some() {
        handle_overlay_key(sender, state, key).await?;
        return Ok(false);
    }
    if state.pending_unpair.is_some() {
        match key.code {
            KeyCode::Char('y') => confirm_unpair(sender, state).await?,
            KeyCode::Char('n') | KeyCode::Esc => state.pending_unpair = None,
            _ => {}
        }
        return Ok(false);
    }
    if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
        return Ok(true);
    }
    if handle_navigation_key(state, key, terminal.size()?.into()) {
        return Ok(false);
    }
    if state.focus == DashboardFocus::Devices
        && matches!(key.code, KeyCode::Char('u') | KeyCode::Delete)
    {
        begin_unpair(state);
        return Ok(false);
    }
    if state.focus == DashboardFocus::Chats && key.code == KeyCode::Enter {
        open_selected_session(sender, state).await?;
        return Ok(false);
    }
    let mode = match key.code {
        KeyCode::Char('p') => Some(SetupMode::Login),
        KeyCode::Char('d') => Some(SetupMode::Agent),
        KeyCode::Char('r') => {
            state.error = None;
            request_snapshot(sender).await?;
            None
        }
        _ => None,
    };
    if let Some(mode) = mode {
        state.error = setup::run_gateway(terminal, mode, sender, events, &mut state.gateway)
            .await
            .err()
            .map(|error| error.to_string());
        terminal.clear()?;
        request_snapshot(sender).await?;
    }
    Ok(false)
}

pub(super) async fn open_selected_session(
    sender: &GatewaySender,
    state: &mut DashboardState,
) -> Result<()> {
    let Some(session_id) = state.selected_session_id.clone() else {
        return Ok(());
    };
    let request_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::OpenSession {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            last_sequence: None,
        })
        .await
        .map_err(gateway_error)?;
    state.pending_open = Some((request_id, session_id));
    state.error = None;
    Ok(())
}

pub(super) async fn handle_overlay_key(
    sender: &GatewaySender,
    state: &mut DashboardState,
    key: KeyEvent,
) -> Result<()> {
    let Some(overlay) = state.overlay.as_mut() else {
        return Ok(());
    };
    if overlay.input.is_some() {
        if let Some(op) = handle_action_input_key(overlay, key) {
            submit_operation(sender, &overlay.session_id, op).await?;
        }
        return Ok(());
    }
    let action_list_open = matches!(
        overlay
            .open_widget()
            .and_then(|widget| widget.content.as_ref()),
        Some(FrontendWidgetContent::ActionList { .. })
    );
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            if overlay.open.take().is_none() {
                state.overlay = None;
            }
        }
        KeyCode::Left if action_list_open => move_overlay_action(overlay, -1),
        KeyCode::Left => {
            overlay.open = None;
        }
        KeyCode::Up | KeyCode::Char('k') => move_overlay_selection(overlay, -1),
        KeyCode::Down | KeyCode::Char('j') => move_overlay_selection(overlay, 1),
        KeyCode::Home => select_overlay_edge(overlay, false),
        KeyCode::End => select_overlay_edge(overlay, true),
        KeyCode::Right if action_list_open => move_overlay_action(overlay, 1),
        KeyCode::Enter | KeyCode::Right => {
            if let Some(op) = activate_overlay(overlay)
                && let Some(op) = prepare_overlay_operation(overlay, op)
            {
                submit_operation(sender, &overlay.session_id, op).await?;
            }
        }
        KeyCode::Char('a') => {
            if let Some(op) = overlay
                .open_widget()
                .or_else(|| {
                    overlay
                        .selected_key()
                        .as_ref()
                        .and_then(|key| overlay.widget(key))
                })
                .and_then(|widget| widget.action.clone())
                && let Some(op) = prepare_overlay_operation(overlay, op)
            {
                submit_operation(sender, &overlay.session_id, op).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(in crate::frontend) fn move_overlay_selection(overlay: &mut CapabilityOverlay, delta: isize) {
    let option_count = overlay
        .open_widget()
        .and_then(|widget| widget.content.as_ref())
        .and_then(|content| match content {
            FrontendWidgetContent::Picker { options, .. } => Some(options.len()),
            FrontendWidgetContent::ActionList { items, .. } => Some(items.len()),
            FrontendWidgetContent::Blocks { .. } => None,
        });
    if let Some(option_count) = option_count {
        overlay.option_list.select(moved_index(
            overlay.option_list.selected(),
            option_count,
            delta,
        ));
        overlay.action_index = 0;
    } else if overlay.open.is_none() {
        overlay.widget_list.select(moved_index(
            overlay.widget_list.selected(),
            overlay.widgets.len(),
            delta,
        ));
    }
}

pub(in crate::frontend) fn select_overlay_edge(overlay: &mut CapabilityOverlay, last: bool) {
    let option_count = overlay
        .open_widget()
        .and_then(|widget| widget.content.as_ref())
        .and_then(|content| match content {
            FrontendWidgetContent::Picker { options, .. } => Some(options.len()),
            FrontendWidgetContent::ActionList { items, .. } => Some(items.len()),
            FrontendWidgetContent::Blocks { .. } => None,
        });
    let (list, length) = if let Some(option_count) = option_count {
        (&mut overlay.option_list, option_count)
    } else if overlay.open.is_none() {
        (&mut overlay.widget_list, overlay.widgets.len())
    } else {
        return;
    };
    list.select(
        length
            .checked_sub(1)
            .map(|last_index| if last { last_index } else { 0 }),
    );
    overlay.action_index = 0;
}

pub(in crate::frontend) fn activate_overlay(overlay: &mut CapabilityOverlay) -> Option<Op> {
    if let Some(widget) = overlay.open_widget() {
        return match widget.content.as_ref() {
            Some(FrontendWidgetContent::Picker { options, .. }) => options
                .get(overlay.option_list.selected().unwrap_or_default())
                .map(|option| option.op.clone()),
            Some(FrontendWidgetContent::ActionList { items, .. }) => items
                .get(overlay.option_list.selected().unwrap_or_default())
                .and_then(|item| item.actions.get(overlay.action_index))
                .map(|action| action.op.clone()),
            _ => widget.action.clone(),
        };
    }
    let key = overlay.selected_key()?;
    let widget = overlay.widget(&key)?;
    let action = widget.action.clone();
    if widget.content.is_some() {
        overlay.open = Some(key);
        overlay.sync_selection();
        action
    } else {
        action
    }
}

pub(in crate::frontend) fn move_overlay_action(overlay: &mut CapabilityOverlay, delta: isize) {
    let Some(length) = overlay
        .selected_action_list_item()
        .map(|item| item.actions.len())
    else {
        return;
    };
    overlay.action_index =
        moved_index(Some(overlay.action_index), length, delta).unwrap_or_default();
}

pub(in crate::frontend) fn prepare_overlay_operation(
    overlay: &mut CapabilityOverlay,
    op: Op,
) -> Option<Op> {
    let seed = match &op {
        Op::CapabilityCommand {
            input: Some(input), ..
        } => input.clone(),
        _ => return Some(op),
    };
    let text = truncate_input(terminal_text(&seed));
    overlay.input = Some(ActionInput {
        cursor: text.len(),
        text,
        op,
    });
    None
}

pub(in crate::frontend) fn handle_action_input_key(
    overlay: &mut CapabilityOverlay,
    key: KeyEvent,
) -> Option<Op> {
    let input = overlay.input.as_mut()?;
    match key.code {
        KeyCode::Esc => {
            overlay.input = None;
            None
        }
        KeyCode::Enter => {
            let mut input = overlay.input.take().expect("action input checked");
            if let Op::CapabilityCommand { input: value, .. } = &mut input.op {
                *value = Some(input.text);
            }
            Some(input.op)
        }
        KeyCode::Backspace => {
            let previous = previous_boundary(&input.text, input.cursor);
            input.text.drain(previous..input.cursor);
            input.cursor = previous;
            None
        }
        KeyCode::Delete => {
            let next = next_boundary(&input.text, input.cursor);
            input.text.drain(input.cursor..next);
            None
        }
        KeyCode::Left => {
            input.cursor = previous_boundary(&input.text, input.cursor);
            None
        }
        KeyCode::Right => {
            input.cursor = next_boundary(&input.text, input.cursor);
            None
        }
        KeyCode::Home => {
            input.cursor = 0;
            None
        }
        KeyCode::End => {
            input.cursor = input.text.len();
            None
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            insert_action_input(input, &character.to_string());
            None
        }
        _ => None,
    }
}

pub(super) fn handle_overlay_paste(state: &mut DashboardState, value: &str) {
    if let Some(overlay) = state.overlay.as_mut() {
        insert_overlay_input(overlay, value);
    }
}

pub(in crate::frontend) fn insert_overlay_input(
    overlay: &mut CapabilityOverlay,
    value: &str,
) -> bool {
    let Some(input) = overlay.input.as_mut() else {
        return false;
    };
    let before = (input.text.len(), input.cursor);
    insert_action_input(input, value);
    before != (input.text.len(), input.cursor)
}

pub(super) fn insert_action_input(input: &mut ActionInput, value: &str) {
    let value = terminal_text(value);
    let available = MAX_MESSAGE_BYTES.saturating_sub(input.text.len());
    let value = truncate_to_bytes(&value, available);
    input.text.insert_str(input.cursor, value);
    input.cursor += value.len();
}

pub(super) fn truncate_input(mut value: String) -> String {
    value.truncate(truncate_to_bytes(&value, MAX_MESSAGE_BYTES).len());
    value
}

pub(super) fn truncate_to_bytes(value: &str, limit: usize) -> &str {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub(super) fn previous_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

pub(super) fn next_boundary(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .char_indices()
        .nth(1)
        .map_or(value.len(), |(index, _)| cursor + index)
}

pub(super) async fn submit_operation(
    sender: &GatewaySender,
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
        .map_err(gateway_error)
}

pub(super) fn handle_navigation_key(state: &mut DashboardState, key: KeyEvent, area: Rect) -> bool {
    let areas = dashboard_areas(area);
    let page = match state.focus {
        DashboardFocus::Devices => areas.devices.height.saturating_sub(2),
        DashboardFocus::Chats => areas.chats.height.saturating_sub(2),
    }
    .max(1);
    let page = isize::try_from(page).unwrap_or(isize::MAX);
    match key.code {
        KeyCode::Tab => {
            state.focus = match state.focus {
                DashboardFocus::Devices => DashboardFocus::Chats,
                DashboardFocus::Chats => DashboardFocus::Devices,
            }
        }
        KeyCode::Up | KeyCode::Char('k') => move_selection(state, -1),
        KeyCode::Down | KeyCode::Char('j') => move_selection(state, 1),
        KeyCode::PageUp => move_selection(state, -page),
        KeyCode::PageDown => move_selection(state, page),
        KeyCode::Home => select_edge(state, false),
        KeyCode::End => select_edge(state, true),
        _ => return false,
    }
    true
}

pub(super) fn handle_mouse(state: &mut DashboardState, mouse: MouseEvent, area: Rect) {
    let areas = dashboard_areas(area);
    let (focus, delta) = match mouse.kind {
        MouseEventKind::ScrollUp if contains(areas.devices, mouse.column, mouse.row) => {
            (DashboardFocus::Devices, -3)
        }
        MouseEventKind::ScrollDown if contains(areas.devices, mouse.column, mouse.row) => {
            (DashboardFocus::Devices, 3)
        }
        MouseEventKind::ScrollUp if contains(areas.chats, mouse.column, mouse.row) => {
            (DashboardFocus::Chats, -3)
        }
        MouseEventKind::ScrollDown if contains(areas.chats, mouse.column, mouse.row) => {
            (DashboardFocus::Chats, 3)
        }
        _ => return,
    };
    state.focus = focus;
    move_selection(state, delta);
}

pub(super) fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

pub(super) fn move_selection(state: &mut DashboardState, delta: isize) {
    match state.focus {
        DashboardFocus::Devices => {
            let ordered = ordered_clients(&state.clients);
            let current = state
                .selected_client_id
                .as_deref()
                .and_then(|id| ordered.iter().position(|client| client.client_id == id));
            let selected = moved_index(current, ordered.len(), delta);
            state.selected_client_id = selected.map(|index| ordered[index].client_id.clone());
            state.device_list.select(selected);
        }
        DashboardFocus::Chats => {
            let ordered = ordered_sessions(&state.gateway.sessions);
            let current = state
                .selected_session_id
                .as_deref()
                .and_then(|id| ordered.iter().position(|session| session.session_id == id));
            let selected = moved_index(current, ordered.len(), delta);
            state.selected_session_id = selected.map(|index| ordered[index].session_id.clone());
            state.chat_list.select(selected);
        }
    }
}

pub(super) fn select_edge(state: &mut DashboardState, last: bool) {
    let length = match state.focus {
        DashboardFocus::Devices => state.clients.len(),
        DashboardFocus::Chats => state.gateway.sessions.len(),
    };
    let Some(selected) = length
        .checked_sub(1)
        .map(|last_index| if last { last_index } else { 0 })
    else {
        return;
    };
    match state.focus {
        DashboardFocus::Devices => {
            let ordered = ordered_clients(&state.clients);
            state.selected_client_id = Some(ordered[selected].client_id.clone());
            state.device_list.select(Some(selected));
        }
        DashboardFocus::Chats => {
            let ordered = ordered_sessions(&state.gateway.sessions);
            state.selected_session_id = Some(ordered[selected].session_id.clone());
            state.chat_list.select(Some(selected));
        }
    }
}

pub(super) fn moved_index(current: Option<usize>, length: usize, delta: isize) -> Option<usize> {
    let last = length.checked_sub(1)?;
    Some(
        current
            .unwrap_or_default()
            .saturating_add_signed(delta)
            .min(last),
    )
}

pub(super) fn begin_unpair(state: &mut DashboardState) {
    let Some(client) = ordered_clients(&state.clients)
        .into_iter()
        .find(|client| Some(client.client_id.as_str()) == state.selected_client_id.as_deref())
    else {
        return;
    };
    if Some(client.client_id.as_str()) == state.current_client_id.as_deref() {
        state.error = Some("the dashboard cannot unpair its own device".into());
        return;
    }
    state.error = None;
    state.pending_unpair = Some((client.client_id.clone(), client.label.clone()));
}

pub(super) async fn confirm_unpair(
    sender: &GatewaySender,
    state: &mut DashboardState,
) -> Result<()> {
    let Some((client_id, _)) = state.pending_unpair.take() else {
        return Ok(());
    };
    state.error = None;
    sender
        .send(ClientMessage::UnpairClient {
            request_id: Uuid::new_v4().to_string(),
            client_id,
        })
        .await
        .map_err(gateway_error)
}

pub(super) async fn request_snapshot(sender: &GatewaySender) -> Result<()> {
    sender
        .send(ClientMessage::ListClients {
            request_id: Uuid::new_v4().to_string(),
        })
        .await
        .map_err(gateway_error)?;
    sender
        .send(ClientMessage::GetProfile {
            request_id: Uuid::new_v4().to_string(),
        })
        .await
        .map_err(gateway_error)
}

pub(super) fn handle_frame(state: &mut DashboardState, message: ServerMessage) -> Result<()> {
    match message {
        ServerMessage::Ready { payload } | ServerMessage::GatewayConfigured { payload, .. } => {
            state.gateway = payload;
            sync_chat_selection(state);
        }
        ServerMessage::Sessions { sessions, .. } => {
            state.gateway.sessions = sessions;
            sync_chat_selection(state);
        }
        ServerMessage::Clients {
            current_client_id,
            clients,
            ..
        } => {
            state.current_client_id = Some(current_client_id);
            state.clients = clients;
            sync_device_selection(state);
        }
        ServerMessage::Profile { profile, .. } => state.profile = Some(profile),
        ServerMessage::SessionOpened {
            request_id,
            payload,
        } => {
            let Some((pending_request, expected_session)) = state.pending_open.as_ref() else {
                return Ok(());
            };
            if request_id != *pending_request {
                return Ok(());
            }
            if payload.session.session_id != *expected_session {
                state.error = Some("gateway opened a different chat than requested".into());
            } else {
                state.overlay = Some(CapabilityOverlay::from_session(payload));
            }
            state.pending_open = None;
        }
        ServerMessage::AgentEvent { session_id, record } => {
            if let Some(overlay) = state
                .overlay
                .as_mut()
                .filter(|overlay| overlay.session_id == session_id)
                && let EventMsg::Frontend(event) = record.event.msg
            {
                overlay.apply(event);
            }
        }
        ServerMessage::Rejected {
            request_id,
            message,
            fatal,
            ..
        } => {
            if state
                .pending_open
                .as_ref()
                .is_some_and(|(pending, _)| pending == &request_id)
            {
                state.pending_open = None;
            }
            if fatal {
                return Err(Error::Stopped(message));
            }
            state.error = Some(message);
        }
        ServerMessage::Error { message, fatal, .. } => {
            if fatal {
                return Err(Error::Stopped(message));
            }
            state.error = Some(message);
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn sync_device_selection(state: &mut DashboardState) {
    let ordered = ordered_clients(&state.clients);
    let selected = state
        .selected_client_id
        .as_deref()
        .and_then(|id| ordered.iter().position(|client| client.client_id == id))
        .or_else(|| (!ordered.is_empty()).then_some(0));
    state.selected_client_id = selected.map(|index| ordered[index].client_id.clone());
    state.device_list.select(selected);
}

pub(super) fn sync_chat_selection(state: &mut DashboardState) {
    let ordered = ordered_sessions(&state.gateway.sessions);
    let selected = state
        .selected_session_id
        .as_deref()
        .and_then(|id| ordered.iter().position(|session| session.session_id == id))
        .or_else(|| (!ordered.is_empty()).then_some(0));
    state.selected_session_id = selected.map(|index| ordered[index].session_id.clone());
    state.chat_list.select(selected);
}

pub(super) fn ordered_clients(clients: &[ClientStatus]) -> Vec<&ClientStatus> {
    let mut clients = clients.iter().collect::<Vec<_>>();
    clients.sort_by(|left, right| {
        (left.connections == 0)
            .cmp(&(right.connections == 0))
            .then_with(|| left.label.cmp(&right.label))
    });
    clients
}

pub(super) fn ordered_sessions(sessions: &[SessionRecord]) -> Vec<&SessionRecord> {
    let mut sessions = sessions.iter().collect::<Vec<_>>();
    sessions.sort_by_key(|session| session.activity.state == SessionActivityState::Idle);
    sessions
}

pub(super) fn gateway_error(error: mobius_gateway::Error) -> Error {
    Error::Stopped(error.to_string())
}
