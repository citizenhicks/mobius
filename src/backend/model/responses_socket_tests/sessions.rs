use super::super::*;

#[tokio::test]
async fn active_socket_sessions_are_never_evicted() {
    let provider = OpenAiSocket::new("test-key", "test-model").expect("provider");
    let mut active = Vec::new();
    for index in 0..MAX_SESSION_ENTRIES {
        active.push(
            provider
                .session(&format!("session-{index:03}"))
                .await
                .expect("session"),
        );
    }

    assert!(provider.session("overflow").await.is_err());
    drop(active.remove(0));
    provider
        .session("overflow")
        .await
        .expect("idle session can be evicted");

    let sessions = provider.sessions.lock().await;
    assert_eq!(sessions.len(), MAX_SESSION_ENTRIES);
    assert!(sessions.contains_key("overflow"));
    assert!(
        active
            .iter()
            .all(|session| sessions.values().any(|cached| Arc::ptr_eq(cached, session)))
    );
    drop(sessions);
}
