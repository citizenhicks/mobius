use super::*;

#[tokio::test]
async fn computer_view_follows_local_and_encrypted_connection_authority() {
    use crate::wire::ComputerView;
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let ready = server.host.ready().await.unwrap();
    let all = crate::wire::ClientCapabilities {
        presence: true,
        browser_renderer: true,
        desktop_host: true,
        remote_desktop_viewer: true,
    };
    let viewer = crate::wire::ClientCapabilities {
        remote_desktop_viewer: true,
        ..Default::default()
    };
    let renderer = crate::wire::ClientCapabilities {
        browser_renderer: true,
        ..Default::default()
    };
    for (available, local, encrypted, client, expected) in [
        (
            ComputerView::EmbeddedBrowser,
            true,
            false,
            all,
            ComputerView::EmbeddedBrowser,
        ),
        (
            ComputerView::EmbeddedBrowser,
            false,
            true,
            all,
            ComputerView::Unavailable,
        ),
        (
            ComputerView::EmbeddedBrowser,
            true,
            false,
            viewer,
            ComputerView::Unavailable,
        ),
        (
            ComputerView::RemoteDesktop,
            true,
            false,
            all,
            ComputerView::Unavailable,
        ),
        (
            ComputerView::RemoteDesktop,
            false,
            true,
            all,
            ComputerView::RemoteDesktop,
        ),
        (
            ComputerView::RemoteDesktop,
            false,
            true,
            renderer,
            ComputerView::Unavailable,
        ),
    ] {
        let mut view = ClientView::new(CatalogHint::default());
        view.local = local;
        view.desktop_transport = encrypted;
        view.client = client;
        let mut payload = ready.clone();
        payload.computer_view = available;
        let mut bytes = Vec::new();
        view.write_catalog(
            &mut bytes,
            ServerFrame::new(ServerMessage::Ready { payload }),
        )
        .await
        .unwrap();
        let frame = read_frame::<ServerFrame>(&mut FrameReader::new(bytes.as_slice()))
            .await
            .unwrap()
            .unwrap();
        let ServerMessage::Ready { payload } = frame.message else {
            panic!("ready");
        };
        assert_eq!(payload.computer_view, expected);
    }
    server.host.shutdown().await;
}
