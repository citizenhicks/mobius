use super::*;

#[tokio::test]
async fn session_context_round_trips_through_save_catalog_and_fork() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    let context = crate::protocol::SessionContext {
        bot_id: "test-bot".into(),
        workspace_id: Some("workspace-1".into()),
        workspace_label: Some("Project One".into()),
        origin_label: Some("routine".into()),
        ..crate::protocol::SessionContext::default()
    };
    let mut parent = checkpoint("parent");
    parent.session_context.clone_from(&context);
    store.save(&parent, &[], None).await.expect("save parent");
    let mut child = checkpoint("child");
    child.session_context.clone_from(&context);

    let fork = store
        .fork(&parent.session_id, parent.sequence, &child)
        .await
        .expect("fork session");
    let page = store
        .list_sessions_page(SessionPageRequest {
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
        (context.clone(), context.clone(), vec![&context, &context])
    );
}

#[tokio::test]
async fn save_rejects_a_blank_bot_id() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    let mut checkpoint = checkpoint("session");
    checkpoint.session_context.bot_id = " ".into();

    let error = store
        .save(&checkpoint, &[], None)
        .await
        .expect_err("blank Bot ID must fail");

    assert_eq!(
        error.to_string(),
        "checkpoint error: session Bot ID cannot be blank"
    );
}

#[tokio::test]
async fn session_catalog_rejects_a_blank_bot_id() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    store
        .save(&checkpoint("session"), &[], None)
        .await
        .expect("save session");
    store
        .run(|connection| {
            connection.execute(
                "UPDATE sessions SET session_context_json = ?1 WHERE session_id = ?2",
                [r#"{"bot_id":" "}"#, "session"],
            )?;
            Ok(())
        })
        .await
        .expect("replace session context");

    let error = store
        .list_sessions_page(SessionPageRequest {
            cursor: None,
            limit: 1,
        })
        .await
        .expect_err("blank catalog Bot ID must fail");

    assert_eq!(
        error.to_string(),
        "checkpoint error: session Bot ID cannot be blank"
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
            &parent,
            &[json!({"role": "user", "content": "hello"})],
            None,
        )
        .await
        .expect("save parent");
    store
        .fork("parent", 1, &checkpoint("child"))
        .await
        .expect("fork child");
    store
        .fork("child", 0, &checkpoint("grandchild"))
        .await
        .expect("fork grandchild");
    store
        .save(&checkpoint("other"), &[], None)
        .await
        .expect("save other root");
    for session_id in ["parent", "child", "grandchild"] {
        store
            .append_event(
                session_id,
                1,
                &Event {
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
        .run(|connection| {
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
async fn fork_preserves_a_historical_parent_sequence() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    let mut parent = checkpoint("parent");
    store.save(&parent, &[], None).await.expect("save parent");
    for sequence in 1..=2 {
        parent.sequence = sequence;
        store
            .save(&parent, &[], None)
            .await
            .expect("advance parent");
    }

    let fork = store
        .fork("parent", 1, &checkpoint("child"))
        .await
        .expect("fork historical checkpoint");

    assert_eq!(fork.parent_sequence, Some(1));
}

#[tokio::test]
async fn fork_rejects_a_future_parent_sequence() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    store
        .save(&checkpoint("parent"), &[], None)
        .await
        .expect("save parent");

    let error = store
        .fork("parent", 1, &checkpoint("child"))
        .await
        .expect_err("future fork must fail");

    assert_eq!(
        error.to_string(),
        "checkpoint error: fork point is newer than the parent checkpoint"
    );
}

#[tokio::test]
async fn fork_rejects_a_missing_parent() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");

    let error = store
        .fork("missing", 0, &checkpoint("child"))
        .await
        .expect_err("missing parent must fail");

    assert_eq!(
        error.to_string(),
        "checkpoint error: fork parent does not exist"
    );
}

#[tokio::test]
async fn metadata_round_trips_through_save_and_fork() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    let mut parent = checkpoint("parent");
    parent
        .metadata
        .insert("gateway.chat".into(), json!({"workspace": "/srv/project"}));
    store.save(&parent, &[], None).await.expect("save parent");
    let mut child = checkpoint("child");
    child.metadata.clone_from(&parent.metadata);

    store
        .fork(&parent.session_id, parent.sequence, &child)
        .await
        .expect("fork session");
    let loaded_parent = store
        .load("parent")
        .await
        .expect("load parent")
        .expect("parent checkpoint");
    let loaded_child = store
        .load("child")
        .await
        .expect("load child")
        .expect("child checkpoint");

    assert_eq!(
        (loaded_parent.metadata, loaded_child.metadata),
        (parent.metadata, child.metadata)
    );
}

#[tokio::test]
async fn session_catalog_reads_context_without_decoding_the_checkpoint() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    let context = SessionContext {
        bot_id: "test-bot".into(),
        workspace_id: Some("workspace-1".into()),
        ..SessionContext::default()
    };
    let mut checkpoint = checkpoint("session");
    checkpoint.session_context.clone_from(&context);
    store
        .save(&checkpoint, &[], None)
        .await
        .expect("save session");
    store
        .run(|connection| {
            connection.execute(
                "UPDATE sessions SET latest_checkpoint_json = ?1 WHERE session_id = ?2",
                ["invalid", "session"],
            )?;
            Ok(())
        })
        .await
        .expect("replace full checkpoint payload");

    let page = store
        .list_sessions_page(SessionPageRequest {
            cursor: None,
            limit: 1,
        })
        .await
        .expect("list sessions");

    assert_eq!(page.sessions[0].session_context, context);
}

#[tokio::test]
async fn session_catalog_continues_from_a_stable_cursor() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let store = SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
        .expect("open checkpoint database");
    for session_id in ["a", "b", "c"] {
        store
            .save(&checkpoint(session_id), &[], None)
            .await
            .expect("save session");
    }

    let first = store
        .list_sessions_page(SessionPageRequest {
            cursor: None,
            limit: 2,
        })
        .await
        .expect("load first page");
    let second = store
        .list_sessions_page(SessionPageRequest {
            cursor: first.next_cursor.clone(),
            limit: 2,
        })
        .await
        .expect("load second page");

    assert_eq!(
        (
            first
                .sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            first
                .next_cursor
                .as_ref()
                .map(|cursor| cursor.session_id.as_str()),
            second
                .sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            second.next_cursor,
        ),
        (vec!["c", "b"], Some("b"), vec!["a"], None)
    );
}
