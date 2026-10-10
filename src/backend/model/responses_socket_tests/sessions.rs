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
}

#[tokio::test]
async fn idle_socket_sessions_remain_cached_below_capacity() {
    let provider = OpenAiSocket::new("test-key", "test-model").expect("provider");
    let stale = provider.session("stale").await.expect("stale session");
    stale.lock().await.last_used_at = Instant::now() - Duration::from_secs(60 * 60);
    drop(stale);

    provider.session("fresh").await.expect("fresh session");

    let sessions = provider.sessions.lock().await;
    assert!(sessions.contains_key("stale"));
    assert!(sessions.contains_key("fresh"));
}

#[tokio::test]
async fn http_fallback_session_entries_remain_bounded() {
    let provider = OpenAiSocket::new("test-key", "test-model").expect("provider");
    for index in 0..MAX_SESSION_ENTRIES {
        let session = provider
            .session(&format!("fallback-{index:03}"))
            .await
            .expect("fallback session");
        session.lock().await.use_http = true;
    }

    let overflow = provider
        .session("overflow")
        .await
        .expect("idle HTTP-only session can be evicted");
    assert!(!overflow.lock().await.use_http);
    drop(overflow);

    let sessions = provider.sessions.lock().await;
    assert_eq!(sessions.len(), MAX_SESSION_ENTRIES);
    assert!(sessions.contains_key("overflow"));
    assert_eq!(
        sessions
            .iter()
            .filter(|(id, _)| id.starts_with("fallback-"))
            .count(),
        MAX_SESSION_ENTRIES - 1
    );
    for session in sessions
        .iter()
        .filter(|(id, _)| id.starts_with("fallback-"))
        .map(|(_, session)| session)
    {
        assert!(
            session
                .try_lock()
                .is_ok_and(|state| state.use_http && state.connection.is_none())
        );
    }
}
