use mobius::protocol::{
    FrontendActionListItem, FrontendListItemState, FrontendSlot, FrontendTone, FrontendWidget,
    FrontendWidgetContent, Op,
};
use std::sync::Arc;

use mobius::backend::session_files::session_file_limits;
use mobius_gateway::wire::{ReadyPayload, ServerMessage};
use ratatui::widgets::ListState;

use super::runtime::{handle_frame, insert_overlay_input, prepare_overlay_operation};
use super::state::{CapabilityOverlay, DashboardFocus};

#[test]
fn superseded_profile_rejection_does_not_set_dashboard_error() {
    let mut state = dashboard_state();

    handle_frame(
        &mut state,
        ServerMessage::Rejected {
            request_id: "profile-1".into(),
            code: "profile_superseded".into(),
            message: "profile request superseded".into(),
            fatal: false,
        },
    )
    .expect("benign superseded profile rejection");

    assert_eq!(state.error, None);
}

#[test]
fn capability_input_paste_stops_at_the_protocol_capability_limit() {
    let key: (Arc<str>, Arc<str>) = ("capability-a".into(), "view".into());
    let mut overlay = CapabilityOverlay {
        title: "Chat capabilities · session-1".into(),
        session_id: Some(mobius::SessionId::new("session-1").expect("session")),
        slots: vec![FrontendSlot::Navigation],
        widgets: vec![(Arc::clone(&key.0), Arc::new(widget(blocks("Initial"))))],
        widget_list: ListState::default(),
        open: Some(key),
        option_list: ListState::default(),
        action_index: 0,
        input: None,
    };
    assert!(prepare_overlay_operation(&mut overlay, capability_op("edit", Some(""))).is_none());

    insert_overlay_input(
        &mut overlay,
        &"x".repeat(mobius::protocol::MAX_CAPABILITY_INPUT_BYTES + 1),
    );

    assert_eq!(
        overlay.input.expect("action input").text.len(),
        mobius::protocol::MAX_CAPABILITY_INPUT_BYTES
    );
}

fn dashboard_state() -> super::state::DashboardState {
    super::state::DashboardState {
        endpoint: String::new(),
        gateway: ReadyPayload {
            gateway_version: env!("CARGO_PKG_VERSION").into(),
            machine_name: String::new(),
            computer_view: mobius_gateway::wire::ComputerView::Unavailable,
            bots: Vec::new(),
            sessions: Vec::new(),
            background_approvals: Vec::new(),
            providers: Vec::new(),
            provider_instances: Vec::new(),
            agent_catalog: Default::default(),
            max_active_sessions: 0,
            session_file_limits: session_file_limits(),
            revisions: Default::default(),
            omitted: Default::default(),
        },
        clients: Vec::new(),
        current_client_id: None,
        selected_client_id: None,
        selected_session_id: None,
        selected_bot_id: None,
        device_list: ListState::default(),
        chat_list: ListState::default(),
        bot_list: ListState::default(),
        focus: DashboardFocus::Devices,
        pending_unpair: None,
        profile: None,
        pending_open: None,
        overlay: None,
        error: None,
    }
}

fn blocks(text: &str) -> FrontendWidgetContent {
    FrontendWidgetContent::ActionList {
        title: "View".into(),
        items: vec![FrontendActionListItem {
            id: "view".into(),
            text: text.into(),
            state: FrontendListItemState::Plain,
            actions: Vec::new(),
        }],
        actions: Vec::new(),
    }
}

fn capability_op(command: &str, input: Option<&str>) -> Op {
    Op::CapabilityCommand {
        capability: "capability-a".into(),
        command: command.into(),
        arguments: "note-1".into(),
        input: input.map(str::to_owned),
        target: None,
    }
}

fn widget(content: FrontendWidgetContent) -> FrontendWidget {
    FrontendWidget {
        id: "view".into(),
        count: None,
        priority: mobius::protocol::FrontendPriority::Primary,
        slot: FrontendSlot::Navigation,
        text: "Capability view".into(),
        tone: FrontendTone::Neutral,
        symbol: None,
        icon_only: false,
        progress: None,
        content: Some(content),
        action: None,
    }
}
