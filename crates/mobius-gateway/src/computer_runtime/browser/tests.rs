use super::*;

/// Answers the next page request on `connection` with `endpoint`, returning the chat asked for.
async fn answer(connection: &mut BrowserConnection, endpoint: Option<&str>) -> String {
    let Some(ServerMessage::BrowserPageRequested {
        request_id,
        session_id,
    }) = connection.outgoing.recv().await
    else {
        panic!("the app is asked for a page");
    };
    connection
        .reply(&request_id, endpoint.map(str::to_owned))
        .expect("reply");
    session_id
}

#[tokio::test]
async fn without_a_lending_app_the_worker_keeps_its_own_browser() {
    let host = Arc::new(BrowserHost::default());
    assert_eq!(host.page("chat").await, None);
}

#[tokio::test]
async fn the_app_lends_its_chats_page() {
    let host = Arc::new(BrowserHost::default());
    let mut connection = host.attach();
    let endpoint = "ws+unix:///Users/me/Library/Caches/app.mobius.desktop/browser.sock:/token";
    let (page, asked) = tokio::join!(host.page("chat"), answer(&mut connection, Some(endpoint)));
    assert_eq!(asked, "chat");
    assert_eq!(page.as_deref(), Some(endpoint));
    let (page, _) = tokio::join!(host.page("chat"), answer(&mut connection, None));
    assert_eq!(page, None, "an app may decline");
}

#[tokio::test]
async fn only_unix_socket_endpoints_are_lent() {
    let host = Arc::new(BrowserHost::default());
    let mut connection = host.attach();
    for endpoint in [
        "ws://127.0.0.1:9222/devtools/browser/x",
        "ws+unix://relative.sock:/t",
        "ws+unix:///a b:/t",
    ] {
        let (page, _) = tokio::join!(host.page("chat"), answer(&mut connection, Some(endpoint)));
        assert_eq!(page, None, "{endpoint}");
    }
}

#[tokio::test]
async fn stray_replies_are_rejected() {
    let host = Arc::new(BrowserHost::default());
    let connection = host.attach();
    assert!(connection.reply("not-a-request", None).is_err());
    assert!(connection.reply(&Uuid::new_v4().to_string(), None).is_err());
}

#[tokio::test]
async fn a_newer_app_connection_replaces_the_older_one() {
    let host = Arc::new(BrowserHost::default());
    let older = host.attach();
    let mut newer = host.attach();
    drop(older);
    let endpoint = "ws+unix:///tmp/browser.sock:/token";
    let (page, _) = tokio::join!(host.page("chat"), answer(&mut newer, Some(endpoint)));
    assert_eq!(
        page.as_deref(),
        Some(endpoint),
        "dropping the older left the newer lending"
    );
}

#[tokio::test]
async fn a_closing_app_answers_waiting_calls_with_none() {
    let host = Arc::new(BrowserHost::default());
    let mut connection = host.attach();
    let asked = async {
        let request = connection.outgoing.recv().await;
        assert!(matches!(
            request,
            Some(ServerMessage::BrowserPageRequested { .. })
        ));
        drop(connection);
    };
    let (page, ()) = tokio::join!(host.page("chat"), asked);
    assert_eq!(page, None);
    assert_eq!(host.page("chat").await, None);
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_request_times_out() {
    let host = Arc::new(BrowserHost::default());
    let _connection = host.attach();
    assert_eq!(host.page("chat").await, None);
    assert!(
        host.app()
            .as_ref()
            .is_some_and(|app| app.pending.is_empty())
    );
}

async fn register(
    host: &Arc<BrowserHost>,
    connection: &mut Option<BrowserConnection>,
    enabled: bool,
    local: bool,
    kind: crate::wire::ClientKind,
) -> ServerMessage {
    use crate::wire::{ClientMessage, FrameReader, ServerFrame, read_frame};
    let (reader, mut writer) = tokio::io::duplex(4096);
    handle_message(
        ClientMessage::SetBrowserRuntime {
            request_id: "register".into(),
            enabled,
        },
        host,
        connection,
        local,
        kind,
        &mut writer,
    )
    .await
    .expect("registration handled");
    read_frame::<ServerFrame>(&mut FrameReader::new(reader))
        .await
        .expect("response frame")
        .expect("registration response")
        .message
}

#[tokio::test]
async fn only_the_local_mac_app_lends_its_browser() {
    use crate::wire::ClientKind;
    for (local, kind) in [(false, ClientKind::Macos), (true, ClientKind::Ios)] {
        let host = Arc::new(BrowserHost::default());
        let mut connection = None;
        let response = register(&host, &mut connection, true, local, kind).await;
        assert!(
            matches!(response, ServerMessage::Rejected { code, fatal: false, .. } if code == "browser")
        );
        assert!(connection.is_none());
        assert!(host.app().is_none());
    }
    if cfg!(target_os = "macos") {
        let host = Arc::new(BrowserHost::default());
        let mut connection = None;
        let response = register(&host, &mut connection, true, true, ClientKind::Macos).await;
        assert!(matches!(response, ServerMessage::Accepted { .. }));
        assert!(host.app().is_some());
        register(&host, &mut connection, false, true, ClientKind::Macos).await;
        assert!(host.app().is_none(), "withdrawing ends the lending");
    }
}

#[tokio::test(start_paused = true)]
async fn a_stalled_app_cannot_queue_unbounded_or_unending_page_requests() {
    let host = Arc::new(BrowserHost::default());
    let mut connection = host.attach();
    // Timeouts leave messages in an unresponsive app's bounded outgoing queue.
    for _ in 0..MAX_PENDING_PAGES {
        assert_eq!(host.page("chat").await, None);
    }
    assert_eq!(
        tokio::time::timeout(PAGE_TIMEOUT, host.page("chat")).await,
        Ok(None),
        "a full app queue must fall back without waiting for capacity"
    );
    assert!(host.app().as_ref().unwrap().pending.is_empty());
    while connection.outgoing.try_recv().is_ok() {}

    {
        let cancelled = host.page("cancelled");
        tokio::pin!(cancelled);
        tokio::select! {
            biased;
            _ = &mut cancelled => panic!("request must wait for its reply"),
            _ = connection.outgoing.recv() => {}
        }
    }
    let (page, _) = tokio::join!(host.page("next"), answer(&mut connection, None));
    assert_eq!(page, None);
    assert!(host.app().as_ref().unwrap().pending.is_empty());
}
