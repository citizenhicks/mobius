use super::*;

#[tokio::test]
async fn computer_view_follows_local_and_encrypted_connection_authority() {
    use crate::wire::ComputerView;
    let root = tempfile::tempdir().unwrap();
    let (server, _) = configured_test_server(root.path().join("state")).await;
    let ready = server.host.ready().await.unwrap();
    for (available, local, encrypted, expected) in [
        (
            ComputerView::EmbeddedBrowser,
            true,
            false,
            ComputerView::EmbeddedBrowser,
        ),
        (
            ComputerView::EmbeddedBrowser,
            false,
            true,
            ComputerView::Unavailable,
        ),
        (
            ComputerView::RemoteDesktop,
            true,
            false,
            ComputerView::Unavailable,
        ),
        (
            ComputerView::RemoteDesktop,
            false,
            true,
            ComputerView::RemoteDesktop,
        ),
    ] {
        let mut view = ClientView::new(CatalogHint::default());
        view.local = local;
        view.desktop_transport = encrypted;
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
