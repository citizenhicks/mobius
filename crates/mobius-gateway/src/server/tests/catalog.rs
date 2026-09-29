use crate::client::ConnectOptions;
use crate::wire::{
    CatalogHint, GitDiffScope, READY_SECTIONS, ReadyPayload, ReadySection, SessionSlot,
};

use super::*;

async fn ready(events: &mut GatewayEvents) -> ReadyPayload {
    loop {
        if let ServerMessage::Ready { payload } = next_gateway_message(events).await {
            return payload;
        }
    }
}

/// The next message `wanted` accepts; a catalog the client holds may not be sent again.
async fn next_without_catalog<T>(
    events: &mut GatewayEvents,
    wanted: impl Fn(ServerMessage) -> Option<T>,
) -> T {
    loop {
        let message = next_gateway_message(events).await;
        match &message {
            // The client still learns of each change, without the catalog it holds.
            ServerMessage::Ready { payload } => {
                assert_eq!(payload.omitted, READY_SECTIONS.into(), "{payload:?}");
            }
            ServerMessage::Bots {
                request_id: None, ..
            } => panic!("a Bot catalog this client holds was sent again"),
            _ => {}
        }
        if let Some(found) = wanted(message) {
            return found;
        }
    }
}

fn changed_sessions(message: ServerMessage) -> Option<Vec<SessionSlot>> {
    match message {
        ServerMessage::SessionsChanged { sessions, .. } => Some(sessions),
        _ => None,
    }
}

#[tokio::test]
async fn clients_receive_only_the_catalog_they_do_not_hold() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let (server, grant) = configured_test_server(root.path().join("state")).await;
    let host = server.host.clone();
    let endpoint: Endpoint = format!("tcp://{}", server.listen_addr()).parse().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async {
        let _ = stopped.await;
    }));
    let (plain, identity) = GatewayClient::pair(&endpoint, grant.code, "plain", ClientKind::Cli)
        .await
        .unwrap();
    let (plain_sender, mut plain_events) = plain.into_parts();
    let first = ready(&mut plain_events).await;
    assert!(first.omitted.is_empty() && !first.providers.is_empty());
    assert_eq!(first.revisions.len(), READY_SECTIONS.len());
    let session_id = create_chat(&plain_sender, &mut plain_events, &workspace).await;
    drain_ready_replay(&mut plain_events).await;

    let (_, mut current) =
        GatewayClient::connect(&endpoint, identity.token.as_str(), ClientKind::Cli)
            .await
            .unwrap()
            .into_parts();
    let known = ready(&mut current).await.revisions;
    let (sender, mut events) = GatewayClient::connect_with(
        &endpoint,
        identity.token.as_str(),
        ClientKind::Macos,
        &mut ConnectOptions {
            catalog: CatalogHint {
                known,
                skip: BTreeSet::new(),
            },
            pipelined: Vec::new(),
        },
    )
    .await
    .unwrap()
    .into_parts();
    let held = ready(&mut events).await;
    assert_eq!(held.omitted, READY_SECTIONS.into());
    assert!(held.sessions.is_empty() && held.bots.is_empty() && held.providers.is_empty());

    host.rename_session(&session_id, "renamed").await.unwrap();
    for events in [&mut events, &mut plain_events] {
        let slots = next_without_catalog(events, changed_sessions).await;
        assert!(matches!(
            slots.as_slice(),
            [SessionSlot::Changed(session)] if session.title.as_deref() == Some("renamed")
        ));
    }

    // A client's own change comes back without the catalog it holds.
    sender
        .send(ClientMessage::ConfigureBotDefaults {
            request_id: "defaults".into(),
            expected_revision: first.bot_defaults.as_ref().unwrap().revision,
            config: first.bot_defaults.unwrap().config,
        })
        .await
        .unwrap();
    let configured = next_without_catalog(&mut events, |message| match message {
        ServerMessage::GatewayConfigured { payload, .. } => Some(payload),
        _ => None,
    })
    .await;
    assert_eq!(
        configured.omitted,
        [ReadySection::Bots, ReadySection::Sessions].into()
    );
    sender
        .send(ClientMessage::CreateBot {
            request_id: "bot".into(),
            name: "Echo".into(),
            description: "Checks echoes.".into(),
        })
        .await
        .unwrap();
    next_without_catalog(&mut events, |message| {
        matches!(message, ServerMessage::Bots { request_id: Some(id), .. } if id == "bot")
            .then_some(())
    })
    .await;
    host.rename_session(&session_id, "again").await.unwrap();
    next_without_catalog(&mut events, changed_sessions).await;
    let mut other_saw = (false, false);
    while other_saw != (true, true) {
        match next_gateway_message(&mut plain_events).await {
            ServerMessage::Ready { payload } => {
                assert_eq!(
                    payload.omitted,
                    [ReadySection::Bots, ReadySection::Sessions].into()
                );
                other_saw.0 = true;
            }
            ServerMessage::Bots {
                request_id: None, ..
            } => other_saw.1 = true,
            _ => {}
        }
    }
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn skipped_sections_are_never_sent() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let (server, grant) = configured_test_server(root.path().join("state")).await;
    let host = server.host.clone();
    let endpoint: Endpoint = format!("tcp://{}", server.listen_addr()).parse().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async {
        let _ = stopped.await;
    }));
    let (client, identity) = GatewayClient::pair(&endpoint, grant.code, "skip", ClientKind::Cli)
        .await
        .unwrap();
    let (sender, mut events) = client.into_parts();
    let first = ready(&mut events).await;
    let session_id = create_chat(&sender, &mut events, &workspace).await;
    let (_, mut quiet) = GatewayClient::connect_with(
        &endpoint,
        identity.token.as_str(),
        ClientKind::Macos,
        &mut ConnectOptions {
            catalog: CatalogHint {
                known: BTreeMap::new(),
                skip: READY_SECTIONS.into(),
            },
            pipelined: vec![ClientMessage::SetNotifications {
                request_id: "quiet".into(),
                disabled: [GatewayNotification::Sessions, GatewayNotification::Bots].into(),
            }],
        },
    )
    .await
    .unwrap()
    .into_parts();
    let skipped = ready(&mut quiet).await;
    assert_eq!(skipped.omitted, READY_SECTIONS.into());

    sender
        .send(ClientMessage::ConfigureBotDefaults {
            request_id: "defaults".into(),
            expected_revision: first.bot_defaults.as_ref().unwrap().revision,
            config: first.bot_defaults.unwrap().config,
        })
        .await
        .unwrap();
    loop {
        if matches!(
            next_gateway_message(&mut events).await,
            ServerMessage::GatewayConfigured { .. }
        ) {
            break;
        }
    }
    host.rename_session(&session_id, "later").await.unwrap();
    next_without_catalog(&mut quiet, |message| {
        matches!(message, ServerMessage::BackgroundApprovals { .. }).then_some(())
    })
    .await;
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn pipelined_open_resumes_after_ready_and_selection_skips_replay() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    run_git(&workspace, &["init", "--quiet"]);
    fs::write(workspace.join("new.txt"), "one\ntwo\n").unwrap();
    let (server, grant) = configured_test_server(root.path().join("state")).await;
    let endpoint: Endpoint = format!("tcp://{}", server.listen_addr()).parse().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.serve_until(async {
        let _ = stopped.await;
    }));
    let (client, identity) = GatewayClient::pair(&endpoint, grant.code, "resume", ClientKind::Cli)
        .await
        .unwrap();
    let (sender, mut events) = client.into_parts();
    let session_id = create_chat(&sender, &mut events, &workspace).await;
    let (_, mut watcher) =
        GatewayClient::connect(&endpoint, identity.token.as_str(), ClientKind::Cli)
            .await
            .unwrap()
            .into_parts();
    let submission = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::Submit {
            session_id: session_id.as_str().into(),
            submission: Submission {
                id: submission.as_str().into(),
                op: user_message("hello", Vec::new()),
            },
        })
        .await
        .unwrap();
    wait_submission(&mut events, &submission).await;
    wait_session_activity(&mut watcher, &session_id, SessionActivityState::Running).await;
    wait_session_activity(&mut watcher, &session_id, SessionActivityState::Idle).await;

    let open = |last_sequence| ClientMessage::OpenSession {
        request_id: "open".into(),
        session_id: session_id.as_str().into(),
        last_sequence,
    };
    let (_, mut full) = GatewayClient::connect_with(
        &endpoint,
        identity.token.as_str(),
        ClientKind::Macos,
        &mut ConnectOptions {
            pipelined: vec![open(None)],
            ..ConnectOptions::default()
        },
    )
    .await
    .unwrap()
    .into_parts();
    let (latest, replayed) = opened_replay(&mut full).await;
    assert!(replayed > 0, "a full open replays the chat");

    let (_, mut resumed) = GatewayClient::connect_with(
        &endpoint,
        identity.token.as_str(),
        ClientKind::Macos,
        &mut ConnectOptions {
            pipelined: vec![open(Some(latest))],
            ..ConnectOptions::default()
        },
    )
    .await
    .unwrap()
    .into_parts();
    assert_eq!(opened_replay(&mut resumed).await, (latest, 0));

    let (selector, mut selected) = GatewayClient::connect_with(
        &endpoint,
        identity.token.as_str(),
        ClientKind::Macos,
        &mut ConnectOptions {
            pipelined: vec![ClientMessage::SelectSession {
                request_id: "select".into(),
                session_id: session_id.as_str().into(),
            }],
            ..ConnectOptions::default()
        },
    )
    .await
    .unwrap()
    .into_parts();
    for request in [
        ClientMessage::GetGitDiffTotals {
            request_id: "totals".into(),
            session_id: session_id.as_str().into(),
            scope: GitDiffScope::Unstaged,
        },
        ClientMessage::GetGitDiff {
            request_id: "diff".into(),
            session_id: session_id.as_str().into(),
            scope: GitDiffScope::Unstaged,
        },
    ] {
        selector.send(request).await.unwrap();
    }
    let (mut opened, mut totals, mut diff) = (false, None, None);
    while totals.is_none() || diff.is_none() {
        match next_gateway_message(&mut selected).await {
            ServerMessage::SessionOpened { request_id, .. } if request_id == "select" => {
                opened = true;
            }
            ServerMessage::AgentEvent { .. } | ServerMessage::SessionReplayComplete { .. } => {
                panic!("a selection must not replay the chat")
            }
            ServerMessage::GitDiffTotals { totals: found, .. } => totals = Some(found),
            ServerMessage::GitDiff { diff: found, .. } => diff = Some(found),
            _ => {}
        }
    }
    assert!(opened);
    let totals = totals.unwrap_or_default();
    assert!(totals.additions >= 2 && totals.deletions == 0, "{totals:?}");
    assert!(diff.is_some_and(|diff| !diff.is_empty()));
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
}

/// Waits for Ready, then the pipelined open: its latest sequence and replayed events.
async fn opened_replay(events: &mut GatewayEvents) -> (u64, usize) {
    ready(events).await;
    let mut latest = None;
    let mut replayed = 0;
    loop {
        match next_gateway_message(events).await {
            ServerMessage::SessionOpened { payload, .. } => {
                latest = Some(payload.latest_sequence);
            }
            ServerMessage::AgentEvent { record, .. } => {
                assert!(latest.is_some(), "replay follows the open");
                replayed += 1;
                assert!(record.sequence <= latest.unwrap_or_default());
            }
            ServerMessage::SessionReplayComplete { .. } => {
                return (latest.expect("opened before replay completes"), replayed);
            }
            _ => {}
        }
    }
}
