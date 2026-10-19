use super::*;

#[tokio::test]
async fn session_context_round_trips_through_save_catalog_and_fork() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    let context = crate::protocol::SessionContext {
        owner_id: "test-bot".into(),
        workspace_id: Some("workspace-1".into()),
        workspace_label: Some("Project One".into()),
        origin_label: Some("routine".into()),
        ..crate::protocol::SessionContext::default()
    };
    let mut parent = checkpoint("parent");
    parent.session_context = Arc::new(context.clone());
    store
        .save(parent.clone(), Vec::new(), None)
        .await
        .expect("save parent");
    let mut child = checkpoint("child");
    child.session_context = Arc::new(context.clone());

    let fork = store
        .fork(&parent.session_id, parent.sequence, child.clone())
        .await
        .expect("fork session");
    let page = store
        .list_sessions_page(SessionPageRequest {
            owner_id: None,
            cursor: None,
            limit: 10,
        })
        .await
        .expect("list sessions");

    assert_eq!(
        (
            store
                .load(&parent.session_id)
                .await
                .expect("load parent")
                .expect("parent checkpoint")
                .session_context,
            fork.session_context,
            page.sessions
                .iter()
                .map(|session| &session.session_context)
                .collect::<Vec<_>>(),
        ),
        (
            Arc::new(context.clone()),
            context.clone(),
            vec![&context, &context]
        )
    );
}

#[tokio::test]
async fn delete_sessions_remove_complete_trees_atomically() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    let mut parent = checkpoint("parent");
    parent.sequence = 1;
    store
        .save(
            parent.clone(),
            [json!({"role": "user", "content": "hello"}).into()].to_vec(),
            None,
        )
        .await
        .expect("save parent");
    store
        .fork("parent", 1, checkpoint("child"))
        .await
        .expect("fork child");
    store
        .fork("child", 0, checkpoint("grandchild"))
        .await
        .expect("fork grandchild");
    store
        .save(checkpoint("other"), Vec::new(), None)
        .await
        .expect("save other root");
    for session_id in ["parent", "child", "grandchild"] {
        store
            .append_event(
                session_id,
                1,
                Event {
                    submission_id: None,
                    msg: EventMsg::Warning(crate::protocol::WarningEvent {
                        message: session_id.into(),
                    }),
                },
            )
            .await
            .expect("append event");
        store
            .save_state(session_id, "owned", &json!(session_id))
            .await
            .expect("save session state");
    }
    store
        .save_state("global", "retained", &json!(true))
        .await
        .expect("save global state");

    assert!(
        !store
            .delete_sessions(&["parent".into(), "missing".into(), "other".into()])
            .await
            .expect("reject incomplete selection")
    );
    assert!(store.load("parent").await.expect("load parent").is_some());
    assert!(store.load("other").await.expect("load other").is_some());

    assert!(
        store
            .delete_sessions(&["parent".into(), "other".into()])
            .await
            .expect("delete trees")
    );

    let counts = store
        .run(|connection, _cache| {
            let sessions =
                connection.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
            let transcripts =
                connection.query_row("SELECT COUNT(*) FROM transcript_delta", [], |row| {
                    row.get(0)
                })?;
            let events =
                connection.query_row("SELECT COUNT(*) FROM event_journal", [], |row| row.get(0))?;
            let session_state = connection.query_row(
                "SELECT COUNT(*) FROM middleware_state WHERE scope != 'global'",
                [],
                |row| row.get(0),
            )?;
            Ok((sessions, transcripts, events, session_state))
        })
        .await
        .expect("count remaining rows");
    assert_eq!(counts, (0_i64, 0_i64, 0_i64, 0_i64));
    assert_eq!(
        store
            .load_state("global", "retained")
            .await
            .expect("load global state"),
        Some(json!(true))
    );
    assert!(
        !store
            .delete_sessions(&["parent".into()])
            .await
            .expect("delete absent tree")
    );
}

#[tokio::test]
async fn session_catalog_filters_owner_membership_before_pagination() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    for (session_id, owner_id) in [
        ("a", "owner"),
        ("b", "foreign"),
        ("c", "owner"),
        ("d", "foreign"),
    ] {
        let mut session = checkpoint(session_id);
        std::sync::Arc::make_mut(&mut session.session_context).owner_id = owner_id.into();
        store
            .save(session.clone(), Vec::new(), None)
            .await
            .expect("save session");
    }
    let first = store
        .list_sessions_page(SessionPageRequest {
            owner_id: Some("owner".into()),
            cursor: None,
            limit: 1,
        })
        .await
        .expect("first owned page");
    assert_eq!(&*first.sessions[0].session_id, "c");
    assert_eq!(
        &*first.next_cursor.as_ref().expect("owned cursor").session_id,
        "c"
    );
    let second = store
        .list_sessions_page(SessionPageRequest {
            owner_id: Some("owner".into()),
            cursor: first.next_cursor,
            limit: 1,
        })
        .await
        .expect("second owned page");
    assert_eq!(&*second.sessions[0].session_id, "a");
    assert!(second.next_cursor.is_none());
    assert!(
        store
            .list_sessions_page(SessionPageRequest {
                owner_id: Some(" ".into()),
                cursor: None,
                limit: 1,
            })
            .await
            .is_err()
    );
}
