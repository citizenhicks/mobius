use super::{Command, Update, needs_bootstrap};
use crate::fixture::opened;
use mobius::SessionId;
use std::time::Duration;

#[test]
fn storage_export_advances_only_the_current_selection_and_request() {
    use crate::files::Transfer;
    use crate::storage::Storage;
    use std::path::Path;

    let (commands, mut receiver) = super::mpsc::channel(1);
    let gateway = super::Gateway {
        commands,
        updates: None,
        task: None,
    };
    let gateway = Some(&gateway);
    let mut storage = Storage::default();
    let begin = |storage: &mut Storage, folder: &str| {
        let sessions = ["one", "two"].map(|id| SessionId::new(id).unwrap());
        storage.begin_export(gateway, folder.into(), sessions.into(), [true, false, true])
    };
    let mut take_request = |session: &str, path: &str| {
        let Command::Transfer(Transfer::Export {
            id,
            session_id,
            destination,
            uploads,
            artifacts,
        }) = receiver.try_recv().unwrap()
        else {
            panic!("expected an export request")
        };
        assert_eq!(session_id.as_ref(), session);
        assert_eq!(destination, Path::new(path));
        assert!(uploads && !artifacts, "only uploads were selected");
        id
    };
    assert!(storage.can_purge(), "an export is optional");
    begin(&mut storage, "first").unwrap();
    let first = take_request("one", "first/Chat-1");
    assert!(
        begin(&mut storage, "other").is_err(),
        "a late picker cannot replace the first export"
    );
    assert!(!storage.can_purge(), "cannot purge during an export");
    assert_eq!(
        storage.exported(gateway, &first, Err("failed".into())),
        Err("failed".into())
    );
    storage.selection_changed();
    assert!(
        storage.can_purge(),
        "a failed export permits a purge or retry"
    );

    begin(&mut storage, "retry").unwrap();
    let retried = take_request("one", "retry/Chat-1");
    assert_eq!(storage.exported(gateway, &first, Ok(())), Ok(None));
    assert_eq!(storage.exported(gateway, &retried, Ok(())), Ok(None));
    let last = take_request("two", "retry/Chat-2");
    assert_eq!(
        storage.exported(gateway, &last, Ok(())),
        Ok(Some("retry".into()))
    );
    assert_eq!(storage.exported(gateway, &last, Ok(())), Ok(None));
    assert!(
        storage.export_saved() && storage.can_purge(),
        "the current selection was saved"
    );
    storage.selection_changed();
    assert!(
        !storage.can_purge(),
        "a different selection requires a fresh export"
    );
    storage.open_cleanup();
    assert!(
        storage.can_purge(),
        "a new cleanup starts with an optional export"
    );
}

async fn fake_gateway() -> (
    super::Endpoint,
    tokio::sync::mpsc::UnboundedSender<super::ServerMessage>,
    tokio::task::JoinHandle<()>,
) {
    use mobius_gateway::wire::{ClientFrame, ServerFrame, read_frame, write_frame};
    let (listener, endpoint) = crate::fixture::listener().await;
    let (frames, mut messages) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (mut reader, mut writer) = crate::fixture::authenticate(listener).await;
        loop {
            tokio::select! {
                frame = read_frame::<ClientFrame>(&mut reader) => if frame.unwrap().is_none() { return; },
                Some(message) = messages.recv() => write_frame(&mut writer, &ServerFrame::new(message)).await.unwrap(),
            }
        }
    });
    (endpoint, frames, server)
}

async fn connected_transport() -> (
    super::Transport,
    tokio::sync::mpsc::Receiver<Update>,
    tokio::sync::mpsc::UnboundedSender<super::ServerMessage>,
    tokio::task::JoinHandle<()>,
) {
    let (endpoint, frames, server) = fake_gateway().await;
    let client = super::GatewayClient::connect(&endpoint, "test", super::APP_CLIENT)
        .await
        .unwrap();
    let (updates, mut output) = tokio::sync::mpsc::channel(16);
    let mut transport = transport(updates);
    transport
        .connected(Ok((endpoint, "test".into(), client)))
        .await
        .unwrap();
    assert!(matches!(output.recv().await, Some(Update::Connected(_))));
    (transport, output, frames, server)
}

fn expire_wait(
    mut transport: super::Transport,
) -> (
    tokio::sync::mpsc::Sender<Command>,
    tokio::task::JoinHandle<super::Sent>,
) {
    transport
        .waiting
        .as_mut()
        .unwrap()
        .deadline
        .as_mut()
        .reset(tokio::time::Instant::now());
    let (commands, receiver) = tokio::sync::mpsc::channel(1);
    (
        commands,
        tokio::spawn(super::run_transport(receiver, transport)),
    )
}

#[tokio::test]
async fn recap_and_foreground_history_share_one_socket_without_stealing_replies() {
    use mobius_gateway::wire::{
        ClientFrame, ClientMessage, ServerFrame, ServerMessage, TranscriptPage, read_frame,
        write_frame,
    };

    let (listener, endpoint) = crate::fixture::listener().await;
    let server = tokio::spawn(async move {
        // This listener accepts exactly one authenticated connection.
        let (mut reader, mut writer) = crate::fixture::authenticate(listener).await;
        for _ in 0..2 {
            let request = read_frame::<ClientFrame>(&mut reader)
                .await
                .unwrap()
                .unwrap();
            let ClientMessage::GetSessionHistory {
                request_id,
                session_id,
                before: None,
            } = request.message
            else {
                panic!("unexpected request")
            };
            let item = crate::fixture::user_message("item", &session_id);
            let page = TranscriptPage {
                items: vec![std::sync::Arc::new(item)],
                ..Default::default()
            };
            let reply = ServerMessage::SessionHistory {
                request_id,
                session_id,
                page,
                next: None,
            };
            write_frame(&mut writer, &ServerFrame::new(reply))
                .await
                .unwrap();
        }
        reader
    });
    let client = super::GatewayClient::connect(&endpoint, "test", super::APP_CLIENT)
        .await
        .unwrap();
    let (updates, mut output) = tokio::sync::mpsc::channel(8);
    let mut transport = transport(updates);
    transport
        .connected(Ok((endpoint, "test".into(), client)))
        .await
        .unwrap();
    let (commands, incoming) = tokio::sync::mpsc::channel(2);
    let actor = tokio::spawn(super::run_transport(incoming, transport));
    commands
        .send(Command::Recent {
            session_id: SessionId::new("background").unwrap(),
            since: None,
        })
        .await
        .unwrap();
    commands
        .send(Command::Send(Box::new(ClientMessage::GetSessionHistory {
            request_id: "visible".into(),
            session_id: SessionId::new("foreground").unwrap(),
            before: None,
        })))
        .await
        .unwrap();
    let read_both = async {
        let mut foreground = false;
        let mut recap = false;
        while !foreground || !recap {
            match output.recv().await.expect("actor update") {
                Update::Message(message) => {
                    if let ServerMessage::SessionHistory { request_id, .. } = *message {
                        assert_eq!(&*request_id, "visible");
                        foreground = true;
                    }
                }
                Update::Recent { result, .. } => {
                    assert_eq!(result.unwrap().last_message.as_deref(), Some("background"));
                    recap = true;
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read_both)
        .await
        .unwrap();
    drop(commands);
    actor.await.unwrap().unwrap();
    drop(server.await.unwrap());
}

#[tokio::test]
async fn authenticated_socket_that_never_becomes_ready_reconnects() {
    let (transport, mut output, _frames, server) = connected_transport().await;
    let (commands, task) = expire_wait(transport);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), output.recv()).await.unwrap(),
        Some(Update::Reconnecting { attempt: 1, message, .. }) if message.contains("did not become ready")
    ));
    drop(commands);
    task.await.unwrap().unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn replacement_connect_drops_the_pending_attempt() {
    struct Cancelled(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Cancelled {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&cancelled);
    let (started, waiting) = tokio::sync::oneshot::channel();
    let (updates, mut output) = tokio::sync::mpsc::channel(4);
    let mut transport = transport(updates);
    transport.dial = Some(Box::pin(async move {
        let _cancelled = Cancelled(flag);
        started.send(()).unwrap();
        std::future::pending().await
    }));
    let (commands, receiver) = tokio::sync::mpsc::channel(1);
    let task = tokio::spawn(super::run_transport(receiver, transport));
    waiting.await.unwrap();
    // Parse fails before reading any real gateway state or Keychain credentials.
    commands
        .send(Command::Connect(super::Connection::Saved("invalid".into())))
        .await
        .unwrap();
    assert!(
        matches!(output.recv().await, Some(Update::Connecting { label, .. }) if label == "invalid")
    );
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), output.recv())
            .await
            .unwrap(),
        Some(Update::Error(_))
    ));
    assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
    drop(commands);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn missing_open_or_replay_completion_reports_uncertain_delivery_without_recreating() {
    for opened_first in [false, true] {
        let (mut transport, mut output, _frames, server) = connected_transport().await;
        transport.waiting = None;
        transport
            .send(super::ClientMessage::CreateSession {
                request_id: "open".into(),
                bot_id: "bot".into(),
                workspace: "/workspace".into(),
                auto: Default::default(),
                prompt: None,
            })
            .await
            .unwrap();
        if opened_first {
            transport
                .frame(Ok(Some(mobius_gateway::wire::ServerFrame::new(opened(
                    "chat", 5,
                )))))
                .await
                .unwrap();
            assert!(matches!(output.recv().await, Some(Update::Message(_))));
        }
        let (commands, task) = expire_wait(transport);
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(1), output.recv()).await.unwrap(), Some(Update::SendFailed {
                request_id: Some(request_id), delivery_uncertain: true, ..
            }) if &*request_id == "open")
        );
        assert!(matches!(
            output.recv().await,
            Some(Update::Reconnecting { attempt: 1, .. })
        ));
        drop(commands);
        task.await.unwrap().unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn only_current_replay_progress_extends_the_session_wait() {
    let (updates, _output) = tokio::sync::mpsc::channel(8);
    let mut transport = transport(updates);
    transport.await_session("open".into());
    transport.observe(&mut opened("chat", 5));
    // Replay record numbers start below the snapshot's latest durable sequence.
    transport.waiting.as_mut().unwrap().deadline =
        Box::pin(tokio::time::sleep(Duration::from_millis(1)));
    transport.observe(&mut event("chat", 2));
    let advanced = transport.waiting.as_ref().unwrap().deadline.deadline();
    assert!(advanced > tokio::time::Instant::now() + Duration::from_secs(29));
    for mut message in [event("other", 99), event("chat", 1), event("chat", 2)] {
        transport.observe(&mut message);
    }
    transport.finish_wait("old-open");
    assert_eq!(
        transport.waiting.as_ref().unwrap().deadline.deadline(),
        advanced
    );

    transport.await_session("replacement".into());
    transport.open = None;
    let replacement = transport.waiting.as_ref().unwrap().deadline.deadline();
    transport.observe(&mut opened("old-chat", 100));
    transport.finish_wait("open");
    assert!(transport.open.is_none());
    assert_eq!(
        transport.waiting.as_ref().unwrap().deadline.deadline(),
        replacement
    );
    transport.finish_wait("replacement");
    assert!(transport.waiting.is_none());
}

#[tokio::test]
async fn fatal_authentication_failures_do_not_enter_retry() {
    for error in [
        super::Error::Unauthorized,
        super::Error::Invalid("invalid gateway configuration".into()),
    ] {
        let (updates, mut output) = tokio::sync::mpsc::channel(1);
        let mut transport = transport(updates);
        transport.connected(Err(error)).await.unwrap();
        assert!(matches!(output.recv().await, Some(Update::Error(_))));
        assert!(transport.target.is_none() && transport.retry.is_none());
    }
}

#[tokio::test]
async fn startup_retries_follow_the_server_delay_for_saved_and_pairing_connections() {
    for target in [
        super::Connection::Saved("wss://example.com".into()),
        super::Connection::Pair {
            endpoint: "wss://example.com".into(),
            code: "one-time-code".into(),
        },
    ] {
        let (updates, mut output) = tokio::sync::mpsc::channel(1);
        let mut transport = transport(updates);
        transport.target = Some(target.clone());
        transport.attempt = 8;
        let delay = Duration::from_secs(1);
        let before = tokio::time::Instant::now();
        transport
            .connected(Err(super::Error::WebSocketUpgrade {
                status: 503,
                retry_after: Some(delay),
            }))
            .await
            .unwrap();
        let after = tokio::time::Instant::now();
        let deadline = transport.retry.as_ref().unwrap().deadline();
        assert!(deadline >= before + delay && deadline <= after + delay);
        assert!(transport.target.as_ref() == Some(&target));
        assert!(matches!(
            output.recv().await,
            Some(Update::Reconnecting { attempt: 9, message, .. }) if message == super::super::GATEWAY_STARTING
        ));
    }
}

#[tokio::test]
async fn failed_send_schedules_reconnect_instead_of_leaving_the_ui_ready() {
    let (listener, endpoint) = crate::fixture::listener().await;
    let server = tokio::spawn(async move {
        drop(crate::fixture::authenticate(listener).await);
    });
    let client = super::GatewayClient::connect(&endpoint, "test", super::APP_CLIENT)
        .await
        .unwrap();
    let (updates, mut output) = tokio::sync::mpsc::channel(2);
    let mut transport = transport(updates);
    transport.connection = Some(super::GatewayLink::new(client, super::RequestUsage::Shared));
    // GatewaySender deterministically rejects a second authentication message.
    transport
        .send(super::ClientMessage::Authenticate {
            token: "test".into(),
            client: super::APP_CLIENT,
            catalog: Default::default(),
        })
        .await
        .unwrap();
    assert!(matches!(
        output.recv().await,
        Some(Update::SendFailed {
            delivery_uncertain: true,
            ..
        })
    ));
    assert!(matches!(
        output.recv().await,
        Some(Update::Reconnecting { attempt: 1, .. })
    ));
    assert!(transport.connection.is_none() && transport.retry.is_some());
    server.await.unwrap();
}

fn transport(updates: tokio::sync::mpsc::Sender<Update>) -> super::Transport {
    super::Transport::new(updates, Some(super::Connection::Local))
}

fn event(session: &str, sequence: u64) -> super::ServerMessage {
    super::ServerMessage::AgentEvent {
        session_id: SessionId::new(session).unwrap(),
        record: mobius_gateway::wire::RecordedEvent {
            sequence,
            recorded_at_ms: 0,
            event: mobius::protocol::Event {
                submission_id: None,
                msg: mobius::protocol::EventMsg::ContextCompacted,
            },
            stream_metrics: Vec::new(),
            preview: None,
        },
        transcript: Vec::new(),
        run_stats: None,
    }
}

#[tokio::test]
async fn reconnects_send_the_held_catalog_and_resume_the_open_chat() {
    use mobius_gateway::wire::{ReadySection, ServerMessage};
    let (updates, _output) = tokio::sync::mpsc::channel(8);
    let mut transport = transport(updates);
    transport.prepare_resume();
    assert!(transport.next.catalog.known.is_empty() && transport.next.pipelined.is_empty());

    let mut payload = crate::state::tests::catalog::ready(&[], 0);
    payload.revisions = [
        (ReadySection::Config, "config".into()),
        (ReadySection::Sessions, "sessions".into()),
    ]
    .into();
    let mut ready = ServerMessage::Ready { payload };
    transport.observe(&mut ready);
    let ServerMessage::Ready { payload } = &ready else {
        unreachable!()
    };
    assert!(
        payload.revisions.is_empty(),
        "the window does not need revisions"
    );
    transport.observe(&mut ServerMessage::SessionsChanged {
        sessions: Vec::new(),
        revision: "delta".into(),
    });

    // Only a chat whose replay finished resumes, from the last event received for it.
    transport.await_session("open".into());
    for mut message in [opened("chat", 5), event("chat", 7), event("other", 9)] {
        transport.observe(&mut message);
    }
    transport.prepare_resume();
    assert!(
        transport.next.pipelined.is_empty(),
        "an unfinished replay reopens in full"
    );
    transport.await_session("open".into());
    for mut message in [
        opened("chat", 5),
        event("chat", 7),
        event("other", 9),
        ServerMessage::SessionReplayComplete {
            request_id: "open".into(),
            session_id: SessionId::new("chat").unwrap(),
        },
    ] {
        transport.observe(&mut message);
    }
    transport.prepare_resume();
    assert_eq!(
        transport.next.catalog.known,
        [
            (ReadySection::Config, "config".into()),
            (ReadySection::Sessions, "delta".into())
        ]
        .into()
    );
    assert!(matches!(
        transport.next.pipelined.as_slice(),
        [super::ClientMessage::OpenSession {
            session_id,
            last_sequence: Some(7),
            ..
        }] if &**session_id == "chat"
    ));
    transport.observe(&mut ServerMessage::Sessions {
        request_id: Some("list".into()),
        sessions: Vec::new(),
    });
    assert_eq!(
        transport.next.catalog.known.keys().collect::<Vec<_>>(),
        [&ReadySection::Config]
    );
}

#[test]
fn local_bootstrap_recovers_only_absent_or_empty_state() {
    let parent = tempfile::tempdir().unwrap();
    let state = parent.path().join("gateway");
    assert!(needs_bootstrap(&state).unwrap());
    std::fs::create_dir(&state).unwrap();
    assert!(needs_bootstrap(&state).unwrap());
    assert!(!state.exists());
    std::fs::create_dir(&state).unwrap();
    let existing = state.join("gateway.toml");
    std::fs::write(&existing, "preserve malformed configuration").unwrap();
    assert!(!needs_bootstrap(&state).unwrap());
    assert_eq!(
        std::fs::read_to_string(existing).unwrap(),
        "preserve malformed configuration"
    );
}
