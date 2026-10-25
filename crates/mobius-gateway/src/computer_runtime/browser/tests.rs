use super::*;

/// Answers the next page request on `connection` with `endpoint`, returning the chat asked for.
async fn answer(connection: &mut BrowserConnection, endpoint: Option<&str>) -> SessionId {
    let Some(ServerMessage::BrowserPageRequested {
        request_id,
        session_id,
        ..
    }) = connection.outgoing.recv().await
    else {
        panic!("the app is asked for a page");
    };
    connection
        .reply(&request_id, endpoint.map(str::to_owned))
        .expect("reply");
    session_id
}

async fn register(
    host: &Arc<BrowserHost>,
    connection: &mut Option<BrowserConnection>,
    enabled: bool,
    local: bool,
    client: crate::wire::ClientCapabilities,
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
        client,
        true,
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
async fn only_a_local_browser_renderer_lends_its_browser() {
    let renderer = crate::wire::ClientCapabilities {
        browser_renderer: true,
        ..Default::default()
    };
    let desktop_host = crate::wire::ClientCapabilities {
        presence: true,
        desktop_host: true,
        ..Default::default()
    };
    for (local, client) in [(false, renderer), (true, desktop_host)] {
        let host = Arc::new(BrowserHost::default());
        let mut connection = None;
        let response = register(&host, &mut connection, true, local, client).await;
        assert!(
            matches!(response, ServerMessage::Rejected { code, fatal: false, .. } if code == "browser")
        );
        assert!(connection.is_none());
        assert!(host.app().is_none());
    }
    if cfg!(target_os = "macos") {
        let host = Arc::new(BrowserHost::default());
        let mut connection = None;
        let response = register(&host, &mut connection, true, true, renderer).await;
        assert!(matches!(response, ServerMessage::Accepted { .. }));
        assert!(host.app().is_some());
        register(&host, &mut connection, false, true, renderer).await;
        assert!(host.app().is_none(), "withdrawing ends the lending");
    }
}

#[tokio::test(start_paused = true)]
async fn a_stalled_app_cannot_queue_unbounded_or_unending_page_requests() {
    let host = Arc::new(BrowserHost::default());
    let mut connection = host.attach();
    // Timeouts leave messages in an unresponsive app's bounded outgoing queue.
    for _ in 0..MAX_PENDING_PAGES {
        assert_eq!(
            host.page(&SessionId::new("chat").unwrap(), false).await,
            None
        );
    }
    assert_eq!(
        tokio::time::timeout(
            PAGE_TIMEOUT,
            host.page(&SessionId::new("chat").unwrap(), false)
        )
        .await,
        Ok(None),
        "a full app queue must fall back without waiting for capacity"
    );
    assert!(host.app().as_ref().unwrap().pending.is_empty());
    while connection.outgoing.try_recv().is_ok() {}

    {
        let session = SessionId::new("cancelled").unwrap();
        let cancelled = host.page(&session, false);
        tokio::pin!(cancelled);
        tokio::select! {
            biased;
            _ = &mut cancelled => panic!("request must wait for its reply"),
            _ = connection.outgoing.recv() => {}
        }
    }
    let next = SessionId::new("next").unwrap();
    let (page, _) = tokio::join!(host.page(&next, false), answer(&mut connection, None));
    assert_eq!(page, None);
    assert!(host.app().as_ref().unwrap().pending.is_empty());
}
