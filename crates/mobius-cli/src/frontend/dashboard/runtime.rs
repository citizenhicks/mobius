pub(super) use crate::frontend::response::wait_ready;
use std::path::PathBuf;
use std::sync::Arc;

use mobius::protocol::{
    EventMsg, FrontendWidgetContent, MAX_CAPABILITY_INPUT_BYTES, MAX_MESSAGE_BYTES, Op,
};
use mobius::{Error, Result};
use mobius_gateway::client::{GatewayClient, GatewayEvents, GatewaySender};
use mobius_gateway::wire::{
    ClientCapabilities, ClientMessage, ServerMessage, apply_session_changes,
};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use tokio::time::MissedTickBehavior;

use super::REFRESH_INTERVAL;
use super::state::{
    ActionInput, CapabilityOverlay, DashboardFocus, DashboardState, RowMove, Selection,
};
use super::view::{dashboard_areas, render};
use crate::frontend::terminal::{
    Screen, input_batch, input_interval, terminal_text, truncate_bytes,
};
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
            Error::Invalid(format!(
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
    let mut input = input_interval();
    let mut refresh = tokio::time::interval(REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    state.sync_selection(DashboardFocus::Chats);
    state.sync_selection(DashboardFocus::Bots);
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
    if crate::frontend::terminal::exit_key(key) {
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
        state.sync_selection(DashboardFocus::Bots);
        state.sync_selection(DashboardFocus::Chats);
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
    if let Some(movement) = row_move(key.code) {
        overlay.move_selection(movement);
        return OverlayKey::Handled;
    }
    let action_list_open = overlay.action_items().is_some();
    let op = match key.code {
        KeyCode::Char('q') => return OverlayKey::Close,
        KeyCode::Esc if !overlay.can_go_back() => return OverlayKey::Close,
        KeyCode::Esc => {
            overlay.close_widget();
            None
        }
        KeyCode::Left if action_list_open => {
            overlay.move_action(-1);
            None
        }
        KeyCode::Left => {
            if overlay.can_go_back() {
                overlay.close_widget();
            }
            None
        }
        KeyCode::Right if action_list_open => {
            overlay.move_action(1);
            None
        }
        KeyCode::Enter | KeyCode::Right => activate_overlay(overlay),
        KeyCode::Char('a') => overlay
            .open_widget()
            .or_else(|| overlay.selected_widget().map(|(_, widget)| widget.as_ref()))
            .and_then(|widget| widget.action.clone()), // rules-allow: the overlay keeps its widget action while the submission owns one
        _ => None,
    };
    op.and_then(|op| prepare_overlay_operation(overlay, op))
        .map_or(OverlayKey::Handled, |op| OverlayKey::Submit(Box::new(op)))
}

pub(in crate::frontend) fn activate_overlay(overlay: &mut CapabilityOverlay) -> Option<Op> {
    if let Some(widget) = overlay.open_widget() {
        return match widget.content.as_ref() {
            Some(FrontendWidgetContent::ActionList { .. }) => overlay
                .selected_action()
                .map(mobius::protocol::FrontendAction::operation),
            _ => widget.action.clone(), // rules-allow: the overlay keeps its widget action while the submission owns one
        };
    }
    let (capability, widget) = overlay.selected_widget()?;
    let action = widget.action.clone(); // rules-allow: the overlay keeps its widget action while the submission owns one
    if widget.content.is_some() {
        overlay.open = Some((Arc::clone(capability), Arc::clone(&widget.id)));
        overlay.sync_selection();
    }
    action
}

pub(in crate::frontend) fn prepare_overlay_operation(
    overlay: &mut CapabilityOverlay,
    op: Op,
) -> Option<Op> {
    let action = overlay.selected_action();
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
    crate::frontend::submit(sender, session_id, crate::fresh_id(), op).await
}

fn row_move(key: KeyCode) -> Option<RowMove> {
    match key {
        KeyCode::Up | KeyCode::Char('k') => Some(RowMove::By(-1)),
        KeyCode::Down | KeyCode::Char('j') => Some(RowMove::By(1)),
        KeyCode::Home => Some(RowMove::First),
        KeyCode::End => Some(RowMove::Last),
        _ => None,
    }
}

pub(super) fn handle_navigation_key(state: &mut DashboardState, key: KeyEvent, area: Rect) -> bool {
    match key.code {
        KeyCode::Tab => state.focus = state.focus.next(),
        KeyCode::PageUp | KeyCode::PageDown => {
            let areas = dashboard_areas(area);
            let panel = match state.focus {
                DashboardFocus::Devices => areas.devices,
                DashboardFocus::Chats => areas.chats,
                DashboardFocus::Bots => areas.bots,
            };
            let page = isize::try_from(panel.height.saturating_sub(2).max(1)).unwrap_or(isize::MAX);
            state.move_selection(RowMove::By(if key.code == KeyCode::PageUp {
                -page
            } else {
                page
            }));
        }
        key => match row_move(key) {
            Some(movement) => state.move_selection(movement),
            None => return false,
        },
    }
    true
}

pub(super) fn handle_mouse(state: &mut DashboardState, mouse: MouseEvent, area: Rect) {
    let delta = match mouse.kind {
        MouseEventKind::ScrollUp => -3,
        MouseEventKind::ScrollDown => 3,
        _ => return,
    };
    let areas = dashboard_areas(area);
    let panels = [
        (areas.devices, DashboardFocus::Devices),
        (areas.chats, DashboardFocus::Chats),
        (areas.bots, DashboardFocus::Bots),
    ];
    let Some((_, focus)) = panels
        .into_iter()
        .find(|(rect, _)| contains(*rect, mouse.column, mouse.row))
    else {
        return;
    };
    state.focus = focus;
    state.move_selection(RowMove::By(delta));
}

pub(super) fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

pub(super) fn begin_unpair(state: &mut DashboardState) {
    let Some(client) = state
        .clients
        .iter()
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
            parts: mobius_gateway::wire::ProfileParts {
                provider_usage: false,
                run_stats: true,
            },
        })
        .await
        .map_err(gateway_error)
}

pub(super) fn handle_frame(state: &mut DashboardState, message: ServerMessage) -> Result<()> {
    match message {
        ServerMessage::Ready { payload } | ServerMessage::GatewayConfigured { payload, .. } => {
            state.gateway.update(payload);
            state.sync_selection(DashboardFocus::Chats);
            state.sync_selection(DashboardFocus::Bots);
        }
        ServerMessage::Sessions { sessions, .. } => {
            state.gateway.sessions = sessions;
            state.sync_selection(DashboardFocus::Chats);
        }
        ServerMessage::SessionsChanged { sessions, .. } => {
            apply_session_changes(&mut state.gateway.sessions, sessions);
            state.sync_selection(DashboardFocus::Chats);
        }
        ServerMessage::Bots { bots, .. } => {
            state.gateway.bots = bots;
            state.sync_selection(DashboardFocus::Bots);
        }
        ServerMessage::Clients {
            current_client_id,
            clients,
            ..
        } => {
            state.current_client_id = Some(current_client_id);
            state.clients = clients;
            state.sync_devices();
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
                crate::frontend::dashboard::apply_widget_event(event, None, Some(overlay));
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
