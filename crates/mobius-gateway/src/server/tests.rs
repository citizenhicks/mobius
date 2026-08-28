use futures_util::SinkExt as _;
use mobius::protocol::{Event, EventMsg, Op, SessionFileReference, Submission};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::tungstenite::protocol::Role;
use uuid::Uuid;

use crate::client::{Endpoint, GatewayClient, GatewayEvents, GatewaySender};
use crate::wire::{SessionActivity, SessionActivityState};

use super::*;

async fn wait_gateway_ready(events: &mut GatewayEvents) {
    loop {
        if matches!(
            next_gateway_message(events).await,
            ServerMessage::Ready { .. }
        ) {
            return;
        }
    }
}

async fn next_gateway_message(events: &mut GatewayEvents) -> ServerMessage {
    tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("gateway response timeout")
        .expect("gateway frame")
        .expect("gateway open")
        .message
}

async fn create_chat(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    workspace: &Path,
) -> String {
    let request_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::CreateSession {
            request_id: request_id.clone(),
            workspace: workspace.into(),
        })
        .await
        .expect("create chat");
    loop {
        if let ServerMessage::SessionOpened {
            request_id: actual,
            payload,
        } = next_gateway_message(events).await
            && actual == request_id
        {
            return payload.session.session_id;
        }
    }
}

async fn open_chat(sender: &GatewaySender, events: &mut GatewayEvents, session_id: &str) {
    let request_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::OpenSession {
            request_id: request_id.clone(),
            session_id: session_id.into(),
            last_sequence: None,
        })
        .await
        .expect("open chat");
    loop {
        let frame = events
            .next()
            .await
            .expect("chat frame")
            .expect("gateway open");
        if matches!(
            frame.message,
            ServerMessage::SessionOpened { request_id: actual, .. } if actual == request_id
        ) {
            return;
        }
    }
}

async fn wait_submission(events: &mut GatewayEvents, submission_id: &str) {
    loop {
        let frame = events
            .next()
            .await
            .expect("agent frame")
            .expect("gateway open");
        if matches!(
            frame.message,
            ServerMessage::AgentEvent { record, .. }
                if record.event.submission_id.as_deref() == Some(submission_id)
        ) {
            return;
        }
    }
}

async fn wait_session_activity(
    events: &mut GatewayEvents,
    session_id: &str,
    state: SessionActivityState,
) -> SessionActivity {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), events.next())
            .await
            .expect("session activity timeout")
            .expect("gateway frame")
            .expect("gateway open");
        match frame.message {
            ServerMessage::AgentEvent {
                session_id: actual, ..
            } if actual == session_id => {
                panic!("a nonselected chat event crossed the gateway-wide stream")
            }
            ServerMessage::Sessions { sessions, .. } => {
                if let Some(activity) = sessions
                    .into_iter()
                    .find(|session| session.session_id == session_id)
                    .map(|session| session.activity)
                    .filter(|activity| activity.state == state)
                {
                    return activity;
                }
            }
            _ => {}
        }
    }
}

async fn drain_ready_replay(events: &mut GatewayEvents) {
    while matches!(
        tokio::time::timeout(Duration::from_millis(10), events.next()).await,
        Ok(Ok(Some(_)))
    ) {}
}

fn run_git(workspace: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .env("LC_ALL", "C")
        .current_dir(workspace)
        .output()
        .expect("run Git");
    assert!(
        output.status.success(),
        "Git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

mod protocol;
mod sessions;
mod swarms;
mod transport;
