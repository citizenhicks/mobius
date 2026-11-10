use super::*;
use tokio::sync::mpsc::unbounded_channel;

type Client = WebSocketStream<UnixStream>;

async fn connect(endpoint: &str) -> Option<Client> {
    let (socket, path) = endpoint.strip_prefix("ws+unix://")?.split_once(':')?;
    let stream = UnixStream::connect(socket).await.ok()?;
    tokio_tungstenite::client_async(format!("ws://localhost{path}"), stream)
        .await
        .ok()
        .map(|(client, _)| client)
}

#[tokio::test]
async fn only_live_tokens_connect() {
    let folder = tempfile::tempdir().expect("folder");
    let server = Server::listen(folder.path()).expect("listen");
    let (page, _commands) = unbounded_channel();
    let grant = server.grant(page);
    let endpoint = grant.endpoint().to_owned();
    let token = endpoint.rsplit('/').next().expect("token");
    assert!(connect(&endpoint.replace(token, "guess")).await.is_none());
    assert!(connect(&endpoint).await.is_some());
    drop(grant);
    assert!(connect(&endpoint).await.is_none());
}

#[test]
fn only_this_user_reaches_the_socket() {
    let folder = tempfile::tempdir().expect("folder");
    let server = Server::listen(&folder.path().join("agents")).expect("listen");
    let mode = |path: &Path| {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&folder.path().join("agents")), 0o700);
    assert_eq!(mode(&server.socket), 0o600);
    assert!(!server.socket.to_string_lossy().contains(' '));
}
