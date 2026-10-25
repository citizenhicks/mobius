use crate::client::ConnectOptions;
use crate::wire::{GitDiffScope, ReadyPayload};

use super::*;

async fn ready(events: &mut GatewayEvents) -> ReadyPayload {
    loop {
        if let ServerMessage::Ready { payload } = next_gateway_message(events).await {
            return payload;
        }
    }
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
        stopped.await.ok();
    }));
    let (client, identity) = GatewayClient::pair(
        &endpoint,
        grant.code,
        "resume",
        crate::wire::ClientCapabilities {
            presence: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (sender, mut events) = client.into_parts();
    let session_id = create_chat(&sender, &mut events, &workspace).await;
    let (_, mut watcher) = GatewayClient::connect(
        &endpoint,
        identity.token.as_str(),
        crate::wire::ClientCapabilities {
            presence: true,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .into_parts();
    let submission = Uuid::new_v4().to_string();
    sender
        .send(ClientMessage::Submit {
            session_id: session_id.share(),
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
        session_id: session_id.share(),
        last_sequence,
    };
    let (_, mut full) = GatewayClient::connect_with(
        &endpoint,
        identity.token.as_str(),
        crate::wire::ClientCapabilities {
            presence: true,
            browser_renderer: true,
            desktop_host: true,
            remote_desktop_viewer: true,
        },
        &mut ConnectOptions {
            pipelined: vec![open(None)],
            ..ConnectOptions::default()
        },
    )
    .await
    .unwrap()
    .into_parts();
    let (latest, replayed, transcript) = opened_replay(&mut full).await;
    assert_eq!(replayed, 0, "a fresh open sends no frames");
    assert!(
        transcript.is_some_and(|items| items > 0),
        "a fresh open receives the chat's transcript"
    );

    let (_, mut resumed) = GatewayClient::connect_with(
        &endpoint,
        identity.token.as_str(),
        crate::wire::ClientCapabilities {
            presence: true,
            browser_renderer: true,
            desktop_host: true,
            remote_desktop_viewer: true,
        },
        &mut ConnectOptions {
            pipelined: vec![open(Some(latest))],
            ..ConnectOptions::default()
        },
    )
    .await
    .unwrap()
    .into_parts();
    assert_eq!(opened_replay(&mut resumed).await, (latest, 0, None));

    let (selector, mut selected) = GatewayClient::connect_with(
        &endpoint,
        identity.token.as_str(),
        crate::wire::ClientCapabilities {
            presence: true,
            browser_renderer: true,
            desktop_host: true,
            remote_desktop_viewer: true,
        },
        &mut ConnectOptions {
            pipelined: vec![ClientMessage::SelectSession {
                request_id: "select".into(),
                session_id: session_id.share(),
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
            session_id: session_id.share(),
            scope: GitDiffScope::Unstaged,
        },
        ClientMessage::GetGitDiff {
            request_id: "diff".into(),
            session_id: session_id.share(),
            scope: GitDiffScope::Unstaged,
        },
    ] {
        selector.send(request).await.unwrap();
    }
    let (mut opened, mut totals, mut diff) = (false, None, None);
    while totals.is_none() || diff.is_none() {
        match next_gateway_message(&mut selected).await {
            ServerMessage::SessionOpened { request_id, .. } if &*request_id == "select" => {
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

/// Waits for Ready, then the pipelined open: its latest sequence, replayed events and
/// the item count of a transcript snapshot.
async fn opened_replay(events: &mut GatewayEvents) -> (u64, usize, Option<usize>) {
    ready(events).await;
    let mut latest = None;
    let mut replayed = 0;
    let mut transcript = None;
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
            ServerMessage::SessionTranscript {
                through_sequence,
                page,
                ..
            } => {
                assert_eq!(Some(through_sequence), latest);
                transcript = Some(page.items.len());
            }
            ServerMessage::SessionReplayComplete { .. } => {
                return (
                    latest.expect("opened before replay completes"),
                    replayed,
                    transcript,
                );
            }
            _ => {}
        }
    }
}
