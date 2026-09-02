use super::*;

#[tokio::test]
async fn local_sandbox_rejects_parent_path_escape() {
    let workspace = TempDir::new().expect("create workspace");
    let sandbox = LocalSandbox::new(workspace.path()).expect("sandbox");

    let error = sandbox.read("../outside").await.expect_err("reject escape");

    assert!(matches!(error, Error::Sandbox(_)));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn local_sandbox_confines_command_writes_to_the_workspace() {
    if std::env::var("CODEX_SANDBOX").as_deref() == Ok("seatbelt") {
        return;
    }
    let workspace = TempDir::new().expect("create workspace");
    let outside = workspace.path().parent().expect("parent").join(format!(
        "{}-outside.txt",
        workspace
            .path()
            .file_name()
            .expect("workspace name")
            .to_string_lossy()
    ));
    let probe = outside.with_extension("probe");
    let can_write_outside = std::process::Command::new("sh")
        .args(["-c", "printf probe > \"$1\"", "sh"])
        .arg(&probe)
        .status()
        .is_ok_and(|status| status.success());
    if !can_write_outside {
        return;
    }
    std::fs::remove_file(&probe).expect("clean sandbox probe");
    let sandbox = LocalSandbox::new(workspace.path()).expect("sandbox");

    let output = sandbox
        .execute(
            &format!(
                "printf mobius > command.txt; printf blocked > ../{}",
                outside.file_name().expect("outside name").to_string_lossy()
            ),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Denied,
            mobius::backend::sandbox::CommandMode::Foreground,
            CommandOutputSink::default(),
        )
        .await
        .expect("execute sandboxed command");
    let escaped = outside.exists();
    if escaped {
        std::fs::remove_file(&outside).expect("clean escaped file");
    }

    let workspace_output = std::fs::read_to_string(workspace.path().join("command.txt"))
        .unwrap_or_else(|error| {
            panic!("read workspace output: {error}; command output: {output:?}")
        });
    assert_eq!(workspace_output, "mobius");
    assert!(!escaped);
}

#[tokio::test]
async fn sqlite_persists_latest_checkpoint_transcript_and_fork_lineage() {
    let workspace = TempDir::new().expect("create workspace");
    let path = workspace.path().join("mobius.sqlite3");
    let store = SqliteCheckpoint::new(&path).expect("open checkpoint database");
    let mut empty = Checkpoint::empty("session");
    empty.session_context.bot_id = "test-bot".into();
    store
        .save(&empty, &[], None)
        .await
        .expect("save empty checkpoint");
    let first_user = mobius::backend::model::user_message("parent question");
    let assistant = serde_json::json!({
        "role": "assistant",
        "content": [{"type": "output_text", "text": "parent answer"}]
    });
    let mut first = empty.clone();
    first.sequence = 1;
    first.first_user_message = Some("parent question".into());
    first.context.push(first_user.clone());
    store
        .save(&first, std::slice::from_ref(&first_user), None)
        .await
        .expect("save first message");
    let mut state_only = first.clone();
    state_only.sequence = 2;
    state_only.total_usage.input_tokens = 1;
    state_only.catalog_visible = false;
    store
        .save(&state_only, &[], None)
        .await
        .expect("save state only");
    let mut grown = state_only.clone();
    grown.sequence = 3;
    grown.context.push(assistant.clone());
    store
        .save(&grown, std::slice::from_ref(&assistant), None)
        .await
        .expect("grow context");
    let compacted_item = serde_json::json!({
        "role": "assistant",
        "content": [{"type": "output_text", "text": "compact summary"}]
    });
    let post_compaction = mobius::backend::model::user_message("parent follow-up");
    let mut compacted = grown;
    compacted.sequence = 4;
    compacted.context = vec![compacted_item.clone(), post_compaction.clone()];
    store
        .save(&compacted, std::slice::from_ref(&post_compaction), None)
        .await
        .expect("replace context and append transcript");
    let branch_user = mobius::backend::model::user_message("branch question");
    let latest = compacted;
    let mut branch = Checkpoint::empty("branch");
    branch.session_context.clone_from(&latest.session_context);
    branch.context.clone_from(&latest.context);
    let branch_summary = store
        .fork("session", latest.sequence, &branch)
        .await
        .expect("fork session");
    let mut branch_latest = branch.clone();
    branch_latest.sequence = 1;
    branch_latest.first_user_message = Some("branch question".into());
    branch_latest.context.push(branch_user.clone());
    store
        .save(&branch_latest, std::slice::from_ref(&branch_user), None)
        .await
        .expect("append branch message");
    drop(store);

    let store = SqliteCheckpoint::new(&path).expect("reopen checkpoint database");
    let sessions = store
        .list_sessions_page(SessionPageRequest {
            cursor: None,
            limit: 100,
        })
        .await
        .expect("list sessions")
        .sessions;
    let parent_summary = sessions
        .iter()
        .find(|session| session.session_id == "session")
        .expect("parent summary");
    let persisted_branch = sessions
        .iter()
        .find(|session| session.session_id == "branch")
        .expect("branch summary");
    let parent_transcript = store
        .transcript_page(
            "session",
            TranscriptPageRequest {
                before_sequence: None,
                max_batches: 100,
            },
        )
        .await
        .expect("load transcript")
        .into_positioned_items_chronological()
        .into_iter()
        .map(|(_, item)| item)
        .collect::<Vec<_>>();
    let branch_transcript = store
        .transcript_page(
            "branch",
            TranscriptPageRequest {
                before_sequence: None,
                max_batches: 100,
            },
        )
        .await
        .expect("load branch transcript")
        .into_positioned_items_chronological()
        .into_iter()
        .map(|(_, item)| item)
        .collect::<Vec<_>>();

    assert_eq!(
        (
            store.load("session").await.expect("load checkpoint"),
            parent_transcript,
            store.load("branch").await.expect("load branch"),
            branch_transcript,
            parent_summary.first_user_message.as_deref(),
            persisted_branch.first_user_message.as_deref(),
            parent_summary.catalog_visible,
            persisted_branch.catalog_visible,
            branch_summary.first_user_message.as_deref(),
            branch_summary.parent_session_id.as_deref(),
            branch_summary.parent_sequence,
        ),
        (
            Some(latest.clone()),
            vec![
                first_user.clone(),
                assistant.clone(),
                post_compaction.clone()
            ],
            Some(branch_latest),
            vec![compacted_item, post_compaction, branch_user],
            Some("parent question"),
            Some("branch question"),
            false,
            true,
            None,
            Some("session"),
            Some(4),
        )
    );
}
