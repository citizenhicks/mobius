pub(super) use crate::frontend::response::wait_ready;
use std::path::PathBuf;
use std::sync::Arc;

use mobius::protocol::{
    EventMsg, FrontendWidgetContent, MAX_CAPABILITY_INPUT_BYTES, MAX_MESSAGE_BYTES, Op, Submission,
};
use mobius::{Error, Result};
use mobius_gateway::client::{GatewayClient, GatewayEvents, GatewaySender};
use mobius_gateway::wire::{
    ClientCapabilities, ClientMessage, ClientStatus, ServerMessage, SessionActivityState,
    SessionRecord, apply_session_changes,
};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use tokio::time::MissedTickBehavior;

use super::REFRESH_INTERVAL;
use super::state::{ActionInput, CapabilityOverlay, DashboardFocus, DashboardState, Selection};
use super::view::{dashboard_areas, render};
use crate::frontend::terminal::{INPUT_POLL, Screen, input_batch, terminal_text, truncate_bytes};
use crate::frontend::{bots, setup};
use crate::gateway_error;
use mobius_gateway::gateway_accounts::{configured_token, dashboard_gateway_endpoint};

pub(super) async fn connect(
    state_dir: PathBuf,
) -> Result<(GatewaySender, GatewayEvents, DashboardState)> {
    let endpoint = dashboard_gateway_endpoint(&state_dir).map_err(gateway_error)?;
    mobius_gateway::command::ensure_background_gateway(state_dir, configured_token)
        .await
        .map_err(gateway_error)?;
    let token = configured_token(&endpoint)
        .map_err(gateway_error)?
        .ok_or_else(|| {
            Error::Config(format!(
                "this machine is not paired with {endpoint}; pair it before opening the gateway dashboard"
            ))
        })?;
    let client = GatewayClient::connect(&endpoint, &token, ClientCapabilities::default())
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
            devices: Selection::default(),
            chats: Selection::default(),
            bots: Selection::default(),
            focus: DashboardFocus::Devices,
            pending_unpair: None,
            profile: None,
            pending_open: None,
            overlay: None,
            error: None,
        },
    ))
}

pub(super) async fn dashboard_loop(
    terminal: &mut Screen,
    sender: GatewaySender,
    events: &mut GatewayEvents,
    state: &mut DashboardState,
) -> Result<()> {
    let mut input = tokio::time::interval(INPUT_POLL);
    input.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut refresh = tokio::time::interval(REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    sync_chat_selection(state);
    sync_bot_selection(state);
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
                for event in input_batch() {
                    let event = event?;
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
    terminal: &mut Screen,
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
    if state.focus == DashboardFocus::Bots && key.code == KeyCode::Enter {
        match bots::run(
            terminal,
            sender,
            events,
            &mut state.gateway,
            state.bots.id.as_deref(),
            None,
        )
        .await
        {
            Ok(Some(session_id)) => {
                state.chats.id = Some(session_id);
                open_selected_session(sender, state).await?;
            }
            Ok(None) => state.error = None,
            Err(error) => state.error = Some(error.to_string()),
        }
        sync_bot_selection(state);
        sync_chat_selection(state);
        terminal.clear()?;
        request_snapshot(sender).await?;
        return Ok(false);
    }
    match key.code {
        KeyCode::Char('p') => {
            state.error = setup::run_gateway(terminal, sender, events, &mut state.gateway)
                .await
                .err()
                .map(|error| error.to_string());
            terminal.clear()?;
            request_snapshot(sender).await?;
        }
        KeyCode::Char('r') => {
            state.error = None;
            request_snapshot(sender).await?;
        }
        _ => {}
    }
    Ok(false)
}

pub(super) async fn open_selected_session(
    sender: &GatewaySender,
    state: &mut DashboardState,
) -> Result<()> {
    let Some(session_id) = state.chats.id.as_ref().map(mobius::SessionId::share) else {
        return Ok(());
    };
    let request_id = crate::fresh_id();
    sender
        .send(ClientMessage::OpenSession {
            request_id: Arc::clone(&request_id),
            session_id,
            last_sequence: None,
        })
        .await
        .map_err(gateway_error)?;
    state.pending_open = Some(request_id);
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
    match overlay_key(overlay, key) {
        OverlayKey::Handled => {}
        OverlayKey::Close => state.overlay = None,
        OverlayKey::Submit(op) => {
            if let Some(session_id) = overlay.session_id.as_ref() {
                submit_operation(sender, session_id, *op).await?;
            }
        }
    }
    Ok(())
}

/// What a key did to an open capability overlay.
pub(in crate::frontend) enum OverlayKey {
    Handled,
    Close,
    Submit(Box<Op>),
}

/// The one key map of a capability overlay, shared by the dashboard and the chat screen.
pub(in crate::frontend) fn overlay_key(
    overlay: &mut CapabilityOverlay,
    key: KeyEvent,
) -> OverlayKey {
    if overlay.is_editing() {
        return handle_action_input_key(overlay, key)
            .map_or(OverlayKey::Handled, |op| OverlayKey::Submit(Box::new(op)));
    }
    let action_list_open = matches!(
        overlay
            .open_widget()
            .and_then(|widget| widget.content.as_ref()),
        Some(FrontendWidgetContent::ActionList { .. })
    );
    let op = match key.code {
        KeyCode::Char('q') => return OverlayKey::Close,
        KeyCode::Esc if !overlay.can_go_back() => return OverlayKey::Close,
        KeyCode::Esc => {
            overlay.close_widget();
            None
        }
        KeyCode::Left if action_list_open => {
            move_overlay_action(overlay, -1);
            None
        }
        KeyCode::Left => {
            if overlay.can_go_back() {
                overlay.close_widget();
            }
            None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_overlay_selection(overlay, -1);
            None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_overlay_selection(overlay, 1);
            None
        }
        KeyCode::Home => {
            select_overlay_edge(overlay, Edge::First);
            None
        }
        KeyCode::End => {
            select_overlay_edge(overlay, Edge::Last);
            None
        }
        KeyCode::Right if action_list_open => {
            move_overlay_action(overlay, 1);
            None
        }
        KeyCode::Enter | KeyCode::Right => activate_overlay(overlay),
        KeyCode::Char('a') => overlay
            .open_widget()
            .or_else(|| overlay.selected_key().and_then(|key| overlay.widget(&key)))
            .and_then(|widget| widget.action.clone()), // rules-allow: the overlay keeps its widget action while the submission owns one
        _ => None,
    };
    op.and_then(|op| prepare_overlay_operation(overlay, op))
        .map_or(OverlayKey::Handled, |op| OverlayKey::Submit(Box::new(op)))
}

pub(in crate::frontend) fn move_overlay_selection(overlay: &mut CapabilityOverlay, delta: isize) {
    let option_count = overlay
        .open_widget()
        .and_then(|widget| widget.content.as_ref())
        .map(|FrontendWidgetContent::ActionList { items, .. }| items.len());
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

pub(in crate::frontend) fn select_overlay_edge(overlay: &mut CapabilityOverlay, edge: Edge) {
    let option_count = overlay
        .open_widget()
        .and_then(|widget| widget.content.as_ref())
        .map(|FrontendWidgetContent::ActionList { items, .. }| items.len());
    let (list, length) = if let Some(option_count) = option_count {
        (&mut overlay.option_list, option_count)
    } else if overlay.open.is_none() {
        (&mut overlay.widget_list, overlay.widgets.len())
    } else {
        return;
    };
    list.select(length.checked_sub(1).map(|last| edge.index(last)));
    overlay.action_index = 0;
}

pub(in crate::frontend) fn activate_overlay(overlay: &mut CapabilityOverlay) -> Option<Op> {
    if let Some(widget) = overlay.open_widget() {
        return match widget.content.as_ref() {
            Some(FrontendWidgetContent::ActionList { items, .. }) => items
                .get(overlay.option_list.selected().unwrap_or_default())
                .and_then(|item| item.actions.get(overlay.action_index))
                .map(|action| action.clone().into_operation()), // rules-allow: the overlay keeps its action list while the submission owns one
            _ => widget.action.clone(), // rules-allow: the overlay keeps its widget action while the submission owns one
        };
    }
    let key = overlay.selected_key()?;
    let widget = overlay.widget(&key)?;
    let action = widget.action.clone(); // rules-allow: the overlay keeps its widget action while the submission owns one
    if widget.content.is_some() {
        overlay.open = Some(key);
        overlay.sync_selection();
    }
    action
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
    let action = overlay
        .selected_action_list_item()
        .and_then(|item| item.actions.get(overlay.action_index));
    if action.is_some_and(|action| action.editor.is_none()) {
        return Some(op);
    }
    let seed = match &op {
        Op::CapabilityCommand {
            input: Some(input), ..
        } => input.as_str(),
        Op::CapabilityCommand { .. } if action.is_some_and(|action| action.editor.is_some()) => "",
        Op::ExecApproval {
            decision: mobius::protocol::ReviewDecision::Denied { rejection },
            ..
        } => rejection.as_str(),
        _ => return Some(op),
    };
    let mut text = terminal_text(seed);
    truncate_bytes(&mut text, input_limit(&op));
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
            if input.text.trim().is_empty() {
                return None;
            }
            let mut input = overlay.input.take()?;
            match &mut input.op {
                Op::CapabilityCommand { input: value, .. } => *value = Some(input.text),
                Op::ExecApproval {
                    decision: mobius::protocol::ReviewDecision::Denied { rejection },
                    ..
                } => *rejection = input.text,
                _ => {}
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
    let mut value = terminal_text(value);
    truncate_bytes(
        &mut value,
        input_limit(&input.op).saturating_sub(input.text.len()),
    );
    input.text.insert_str(input.cursor, &value);
    input.cursor += value.len();
}

/// Terminal-local memory cap for the overlay editor; the gateway enforces the protocol limit.
fn input_limit(op: &Op) -> usize {
    if matches!(op, Op::CapabilityCommand { .. }) {
        MAX_CAPABILITY_INPUT_BYTES
    } else {
        MAX_MESSAGE_BYTES
    }
}

pub(super) fn previous_boundary(value: &str, cursor: usize) -> usize {
    value
        .get(..cursor)
        .and_then(|before| before.char_indices().next_back())
        .map_or(0, |(index, _)| index)
}

pub(super) fn next_boundary(value: &str, cursor: usize) -> usize {
    value
        .get(cursor..)
        .and_then(|after| after.char_indices().nth(1))
        .map_or(value.len(), |(index, _)| cursor + index)
}

pub(super) async fn submit_operation(
    sender: &GatewaySender,
    session_id: &mobius::SessionId,
    op: Op,
) -> Result<()> {
    sender
        .send(ClientMessage::Submit {
            session_id: session_id.share(),
            submission: Submission {
                id: crate::fresh_id(),
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
        DashboardFocus::Bots => areas.bots.height.saturating_sub(2),
    }
    .max(1);
    let page = isize::try_from(page).unwrap_or(isize::MAX);
    match key.code {
        KeyCode::Tab => state.focus = state.focus.next(),
        KeyCode::Up | KeyCode::Char('k') => move_selection(state, -1),
        KeyCode::Down | KeyCode::Char('j') => move_selection(state, 1),
        KeyCode::PageUp => move_selection(state, -page),
        KeyCode::PageDown => move_selection(state, page),
        KeyCode::Home => select_edge(state, Edge::First),
        KeyCode::End => select_edge(state, Edge::Last),
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
        MouseEventKind::ScrollUp if contains(areas.bots, mouse.column, mouse.row) => {
            (DashboardFocus::Bots, -3)
        }
        MouseEventKind::ScrollDown if contains(areas.bots, mouse.column, mouse.row) => {
            (DashboardFocus::Bots, 3)
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
    update_selection(state, state.focus, |current, length| {
        moved_index(current, length, delta)
    });
}

pub(super) fn select_edge(state: &mut DashboardState, edge: Edge) {
    update_selection(state, state.focus, |_, length| {
        length.checked_sub(1).map(|last| edge.index(last))
    });
}

/// Keeps the selected id when it is still listed, and otherwise selects the first row.
fn sync_selection(state: &mut DashboardState, focus: DashboardFocus) {
    update_selection(state, focus, |current, length| {
        current.or_else(|| (length > 0).then_some(0))
    });
}

fn update_selection(
    state: &mut DashboardState,
    focus: DashboardFocus,
    pick: impl FnOnce(Option<usize>, usize) -> Option<usize>,
) {
    match focus {
        DashboardFocus::Devices => {
            let ids: Vec<&String> = ordered_clients(&state.clients)
                .into_iter()
                .map(|client| &client.client_id)
                .collect();
            state.devices.update(&ids, |id| id.as_str().into(), pick);
        }
        DashboardFocus::Chats => {
            let ids: Vec<&mobius::SessionId> = ordered_sessions(&state.gateway.sessions)
                .into_iter()
                .map(|session| &session.session_id)
                .collect();
            state.chats.update(&ids, mobius::SessionId::share, pick);
        }
        DashboardFocus::Bots => {
            let ids: Vec<&Arc<str>> = state.gateway.bots.iter().map(|bot| &bot.id).collect();
            state.bots.update(&ids, Arc::clone, pick);
        }
    }
}

/// The end of a list Home or End selects.
#[derive(Clone, Copy)]
pub(in crate::frontend) enum Edge {
    First,
    Last,
}

impl Edge {
    const fn index(self, last: usize) -> usize {
        match self {
            Self::First => 0,
            Self::Last => last,
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
        .find(|client| Some(&client.client_id) == state.devices.id.as_ref())
    else {
        return;
    };
    if Some(client.client_id.as_str()) == state.current_client_id.as_deref() {
        state.error = Some("the dashboard cannot unpair its own device".into());
        return;
    }
    state.error = None;
    state.pending_unpair = Some((client.client_id.clone(), client.label.clone())); // rules-allow: the client list keeps its record while the pending unpair owns a copy
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
            request_id: crate::fresh_id(),
            client_id,
        })
        .await
        .map_err(gateway_error)
}

pub(super) async fn request_snapshot(sender: &GatewaySender) -> Result<()> {
    sender
        .send(ClientMessage::ListClients {
            request_id: crate::fresh_id(),
        })
        .await
        .map_err(gateway_error)?;
    sender
        .send(ClientMessage::GetProfile {
            request_id: crate::fresh_id(),
            include_provider_usage: false,
        })
        .await
        .map_err(gateway_error)
}

pub(super) fn handle_frame(state: &mut DashboardState, message: ServerMessage) -> Result<()> {
    match message {
        ServerMessage::Ready { payload } | ServerMessage::GatewayConfigured { payload, .. } => {
            state.gateway.update(payload);
            sync_chat_selection(state);
            sync_bot_selection(state);
        }
        ServerMessage::Sessions { sessions, .. } => {
            state.gateway.sessions = sessions;
            sync_chat_selection(state);
        }
        ServerMessage::SessionsChanged { sessions, .. } => {
            apply_session_changes(&mut state.gateway.sessions, sessions);
            sync_chat_selection(state);
        }
        ServerMessage::Bots { bots, .. } => {
            state.gateway.bots = bots;
            sync_bot_selection(state);
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
            if state.pending_open.as_ref() == Some(&request_id) {
                state.overlay = Some(CapabilityOverlay::from_session(payload));
                state.pending_open = None;
            }
        }
        ServerMessage::AgentEvent {
            session_id, record, ..
        } => {
            if let Some(overlay) = state
                .overlay
                .as_mut()
                .filter(|overlay| overlay.session_id.as_ref() == Some(&session_id))
                && let EventMsg::Frontend(event) = record.event.msg
            {
                overlay.apply(event);
            }
        }
        ServerMessage::Rejected {
            request_id,
            code,
            message,
            fatal,
            ..
        } => {
            if state.pending_open.as_ref() == Some(&request_id) {
                state.pending_open = None;
            }
            if !fatal && code == "profile_superseded" {
                return Ok(());
            }
            if fatal {
                return Err(Error::Stopped(message.to_string()));
            }
            state.error = Some(message.to_string());
        }
        ServerMessage::Error { message, fatal, .. } => {
            if fatal {
                return Err(Error::Stopped(message.to_string()));
            }
            state.error = Some(message.to_string());
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn sync_device_selection(state: &mut DashboardState) {
    sync_selection(state, DashboardFocus::Devices);
}

pub(super) fn sync_chat_selection(state: &mut DashboardState) {
    sync_selection(state, DashboardFocus::Chats);
}

pub(super) fn sync_bot_selection(state: &mut DashboardState) {
    sync_selection(state, DashboardFocus::Bots);
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
