use futures_util::SinkExt as _;
use mobius::backend::checkpoint::{Checkpoint, CheckpointStore as _, sqlite::SqliteCheckpoint};
use mobius::protocol::{
    Event, EventMsg, MessageAuthor, MessageSubmission, Op, SessionFileReference, Submission,
};
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

async fn configured_test_server(state_dir: PathBuf) -> (GatewayServer, PairingGrant) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let listen = listener.local_addr().expect("listen address");
    let (store, config) =
        ConfigStore::initialize(state_dir, listen, None).expect("initialize gateway");
    let config = config
        .registering_provider(
            crate::wire::AgentComposition::default().provider,
            "Test".into(),
            Default::default(),
            Vec::new(),
            Vec::new(),
        )
        .expect("register provider");
    store.save(&config).expect("save provider");
    let (_, grant) = AuthStore::initialize(store.auth_path()).expect("initialize auth");
    let server = GatewayServer::assemble(store, config, listener)
        .await
        .expect("assemble gateway");
    (server, grant)
}

mod bots;
mod protocol;
mod sessions;
mod swarms;
mod transport;

async fn create_bot_chat(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    workspace: &Path,
) -> (String, String) {
    create_bot_chat_with_config(
        sender,
        events,
        workspace,
        crate::wire::AgentComposition::default(),
    )
    .await
}

async fn create_bot_chat_with_config(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    workspace: &Path,
    config: crate::wire::AgentComposition,
) -> (String, String) {
    let token = Uuid::new_v4().simple().to_string();
    let bot_request_id = format!("bot-{token}");
    let bot_name = format!("Test Bot {}", &token[..12]);
    sender
        .send(ClientMessage::CreateBot {
            request_id: bot_request_id.clone(),
            name: bot_name.clone(),
            description: "Own this gateway integration test.".into(),
        })
        .await
        .expect("create Bot");
    let mut bot = loop {
        match next_gateway_message(events).await {
            ServerMessage::Bots {
                request_id: Some(actual),
                bots,
            } if actual == bot_request_id => {
                break bots
                    .into_iter()
                    .find(|bot| bot.name == bot_name)
                    .expect("created Bot");
            }
            ServerMessage::Rejected {
                request_id,
                code,
                message,
                ..
            } if request_id == bot_request_id => {
                panic!("Bot creation rejected ({code}): {message}")
            }
            _ => {}
        }
    };
    if bot.config.config != config {
        let update_request_id = format!("update-{token}");
        sender
            .send(ClientMessage::UpdateBot {
                request_id: update_request_id.clone(),
                id: bot.id.clone(),
                expected_revision: bot.config.revision,
                name: bot.name.clone(),
                description: bot.description.clone(),
                tint: bot.tint,
                config,
            })
            .await
            .expect("update Bot");
        bot = loop {
            match next_gateway_message(events).await {
                ServerMessage::Bots {
                    request_id: Some(actual),
                    bots,
                } if actual == update_request_id => {
                    break bots
                        .into_iter()
                        .find(|candidate| candidate.id == bot.id)
                        .expect("updated Bot");
                }
                ServerMessage::Rejected {
                    request_id,
                    code,
                    message,
                    ..
                } if request_id == update_request_id => {
                    panic!("Bot update rejected ({code}): {message}")
                }
                _ => {}
            }
        };
    }
    let bot_id = bot.id;
    let request_id = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::CreateSession {
            request_id: request_id.clone(),
            workspace: workspace.into(),
            bot_id: bot_id.clone(),
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
            return (payload.session.session_id, bot_id);
        }
    }
}

async fn create_chat(
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    workspace: &Path,
) -> String {
    create_bot_chat(sender, events, workspace).await.0
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

fn user_message(text: impl Into<String>, attachments: Vec<SessionFileReference>) -> Op {
    Op::Message {
        message: MessageSubmission {
            author: MessageAuthor::User,
            text: text.into(),
            attachments,
            requested_delivery: None,
            target_turn_id: None,
        },
    }
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
