use mobius::backend::checkpoint::sqlite::SqliteCheckpoint;

use crate::wire::AgentComposition;

use super::*;

fn store() -> (
    tempfile::TempDir,
    Arc<dyn CheckpointStore>,
    SwarmStore,
    mpsc::UnboundedReceiver<SwarmDelivery>,
) {
    let directory = tempfile::tempdir().expect("workspace");
    let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
        SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3")).expect("checkpoints"),
    );
    let state_dir = directory.path().join("state");
    std::fs::create_dir(&state_dir).expect("state directory");
    let bots = Arc::new(BotStore::open(&state_dir).expect("Bot store"));
    for handle in ["leader", "reviewer", "observer", "third", "overflow"] {
        add_bot(&bots, handle);
    }
    let (store, deliveries) = SwarmStore::new(Arc::clone(&checkpoints), bots);
    (directory, checkpoints, store, deliveries)
}

fn add_bot(bots: &BotStore, handle: &str) -> String {
    let mut composition = AgentComposition::default();
    composition.middleware.set_setting(
        "bots",
        "collaboration",
        Some(mobius::protocol::FrontendSettingValue::String(
            "swarm".into(),
        )),
    );
    bots.create_bot(handle, &format!("Test Bot {handle}"), composition)
        .expect("create Bot")
        .id
}

fn bot_id(store: &SwarmStore, handle: &str) -> String {
    store
        .bots
        .bots()
        .expect("Bots")
        .into_iter()
        .find(|bot| bot.handle == handle)
        .expect("Bot handle")
        .id
}

fn reload(
    checkpoints: Arc<dyn CheckpointStore>,
    store: &SwarmStore,
) -> (SwarmStore, mpsc::UnboundedReceiver<SwarmDelivery>) {
    SwarmStore::new(checkpoints, Arc::clone(&store.bots))
}

async fn create_swarm(store: &SwarmStore) -> SwarmSummary {
    let leader = bot_id(store, "leader");
    store
        .create(
            "Review team".into(),
            leader.clone(),
            vec![leader, bot_id(store, "reviewer")],
        )
        .await
        .expect("create swarm")
}

async fn post(store: &SwarmStore, handle: &str, text: String) -> Result<SwarmPost> {
    store
        .post(
            &bot_id(store, handle),
            &format!("{handle}-thread"),
            text,
            None,
        )
        .await
}

#[tokio::test]
async fn participant_workspaces_follow_current_members_across_catalog_pages_and_reload() {
    let (_directory, checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    let leader = bot_id(&store, "leader");
    let reviewer = bot_id(&store, "reviewer");
    let outsider = bot_id(&store, "third");
    for index in 0..102 {
        let mut checkpoint =
            mobius::backend::checkpoint::Checkpoint::empty(format!("chat-{index:03}"));
        let (bot, workspace) = match index {
            0 => (&reviewer, "/projects/review"),
            1 => (&outsider, "/projects/unrelated"),
            _ => (&leader, "/projects/main"),
        };
        checkpoint.session_context.bot_id.clone_from(bot);
        checkpoint.session_context.workspace_label = Some(workspace.into());
        checkpoints
            .save(&checkpoint, &[], None)
            .await
            .expect("save chat");
    }
    let participant = participant_session_id(&swarm.id, &leader);
    assert_eq!(
        store
            .participant_workspaces(&leader, &participant)
            .await
            .expect("workspaces"),
        Some(BTreeSet::from([
            PathBuf::from("/projects/main"),
            PathBuf::from("/projects/review")
        ]))
    );
    assert_eq!(
        store
            .participant_workspaces(&leader, "chat-002")
            .await
            .expect("ordinary chat"),
        None
    );
    store
        .leave(&swarm.id, &reviewer)
        .await
        .expect("leave swarm");
    store.join(&swarm.id, outsider).await.expect("join swarm");
    let (reloaded, _deliveries) = reload(checkpoints, &store);
    assert_eq!(
        reloaded
            .participant_workspaces(&leader, &participant)
            .await
            .expect("updated workspaces"),
        Some(BTreeSet::from([
            PathBuf::from("/projects/main"),
            PathBuf::from("/projects/unrelated")
        ]))
    );
}

#[tokio::test]
async fn membership_is_unique_and_lazily_reloads() {
    let (_directory, checkpoints, store, _deliveries) = store();
    let created = create_swarm(&store).await;
    let created_title = created.title.clone();
    assert!(created.created_at_ms > 0);
    assert_eq!(created.created_at_ms, created.updated_at_ms);

    let leader = bot_id(&store, "leader");
    let duplicate = store
        .create(
            "Another".into(),
            leader.clone(),
            vec![leader, bot_id(&store, "third")],
        )
        .await
        .expect_err("Bot cannot join two swarms");
    assert!(duplicate.to_string().contains("already belongs"));

    let (reloaded, _deliveries) = reload(checkpoints, &store);
    assert_eq!(reloaded.summaries().await.expect("reload"), vec![created]);
    let snapshot = reloaded
        .snapshot_for_bot(&bot_id(&store, "reviewer"))
        .await
        .expect("snapshot")
        .expect("membership");
    assert_eq!(snapshot.handle, "reviewer");
    assert_eq!(snapshot.swarm.title, created_title);
}

#[tokio::test]
async fn bot_removal_reloads_when_the_deleted_bot_is_still_referenced() {
    let (_directory, checkpoints, store, _deliveries) = store();
    create_swarm(&store).await;
    let reviewer = store
        .bots
        .bot(&bot_id(&store, "reviewer"))
        .expect("reviewer");
    let deletion = store
        .bots
        .prepare_bot_deletion(&reviewer.id, reviewer.config.revision)
        .expect("prepare Bot deletion");
    store.bots.delete_bot(deletion).expect("delete Bot");

    let (reloaded, _deliveries) = reload(checkpoints, &store);
    let removal = reloaded
        .remove_bot(&reviewer.id)
        .await
        .expect("recover Swarm removal")
        .expect("reviewer membership");

    assert!(!removal.disbanded);
    assert_eq!(
        reloaded.summaries().await.expect("remaining swarm").len(),
        1
    );
}

#[tokio::test]
async fn persisted_roster_is_keyed_only_by_bot_id() {
    let (_directory, checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    let expected = swarm
        .members
        .iter()
        .map(|member| member.bot_id.as_str())
        .collect::<BTreeSet<_>>();

    let state = checkpoints
        .load_state(STATE_SCOPE, STATE_KEY)
        .await
        .expect("load swarm state")
        .expect("persisted swarm state");
    let members = state["swarms"][swarm.id.as_str()]["members"]
        .as_object()
        .expect("member map");

    assert_eq!(
        members.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        expected
    );
    assert!(
        members
            .values()
            .all(|member| member.get("handle").is_none())
    );
}

#[tokio::test]
async fn disabled_collaboration_preserves_membership_and_pauses_pending_delivery() {
    let (directory, _checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    let reviewer = store
        .bots
        .bot(&bot_id(&store, "reviewer"))
        .expect("reviewer");
    post(&store, "leader", "@reviewer review later".into())
        .await
        .expect("post");
    let mut disabled = reviewer.config.config.clone();
    disabled.middleware.set_setting(
        "bots",
        "collaboration",
        Some(mobius::protocol::FrontendSettingValue::String("off".into())),
    );
    let reviewer = store
        .bots
        .update_bot(
            &reviewer.id,
            reviewer.config.revision,
            &reviewer.name,
            &reviewer.description,
            reviewer.tint,
            disabled,
        )
        .expect("disable");
    assert!(
        store
            .snapshot_for_bot(&reviewer.id)
            .await
            .expect("active membership")
            .is_none()
    );
    assert!(
        store
            .claim_next_delivery(&reviewer.id)
            .await
            .expect("claim")
            .is_none()
    );
    assert_eq!(
        store
            .pending_deliveries(&reviewer.id)
            .await
            .expect("pending")
            .len(),
        1
    );
    assert_eq!(
        store.records().await.expect("stored membership")[0]
            .members
            .len(),
        2
    );
    assert!(
        store
            .post_user(&swarm.id, "@reviewer new work".into())
            .await
            .expect_err("disabled recipient")
            .to_string()
            .contains("Enable Swarm collaboration")
    );
    assert!(
        store
            .post(&reviewer.id, "chat", "@leader reply".into(), None)
            .await
            .is_err()
    );
    assert!(store.tool_read(&reviewer.id).await.is_err());
    BotsBackend::create_routine(
        &store,
        &reviewer.id,
        None,
        directory.path(),
        "Independent work".into(),
        serde_json::json!({"kind": "interval", "every_seconds": 3600}),
        None,
    )
    .await
    .expect("self routine remains available");
    let mut enabled = reviewer.config.config.clone();
    enabled.middleware.set_setting(
        "bots",
        "collaboration",
        Some(mobius::protocol::FrontendSettingValue::String(
            "swarm".into(),
        )),
    );
    store
        .bots
        .update_bot(
            &reviewer.id,
            reviewer.config.revision,
            &reviewer.name,
            &reviewer.description,
            reviewer.tint,
            enabled,
        )
        .expect("reenable");
    assert!(
        store
            .claim_next_delivery(&reviewer.id)
            .await
            .expect("resumed claim")
            .is_some()
    );
}

#[tokio::test]
async fn default_bots_cannot_join_a_swarm_until_they_opt_in() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    let disabled = store
        .bots
        .create_bot(
            "Independent",
            "No collaboration",
            AgentComposition::default(),
        )
        .expect("default Bot");
    assert!(!disabled.collaboration_enabled());
    assert!(
        store
            .create(
                "Disabled team".into(),
                disabled.id.clone(),
                vec![disabled.id.clone(), bot_id(&store, "leader")]
            )
            .await
            .is_err()
    );
    let swarm = create_swarm(&store).await;
    assert!(store.join(&swarm.id, disabled.id).await.is_err());
}

#[tokio::test]
async fn membership_queries_resolve_current_swarm_scope() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    let reviewer = bot_id(&store, "reviewer");

    assert_eq!(
        (
            store
                .snapshot_for_bot(&reviewer)
                .await
                .expect("membership")
                .is_some(),
            store
                .contains_swarm(&swarm.id)
                .await
                .expect("existing swarm"),
            store
                .contains_swarm(&Uuid::new_v4().to_string())
                .await
                .expect("missing swarm"),
            BotsBackend::scratchpad_scope(&store, &reviewer)
                .await
                .expect("scratchpad scope"),
        ),
        (true, true, false, Some(swarm.id))
    );
}

#[tokio::test]
async fn bot_can_schedule_itself_without_a_swarm() {
    let (directory, _checkpoints, store, _deliveries) = store();
    let observer = bot_id(&store, "observer");

    let output = BotsBackend::create_routine(
        &store,
        &observer,
        None,
        directory.path(),
        "Check the project state.".into(),
        serde_json::json!({"kind": "interval", "every_seconds": 3600}),
        None,
    )
    .await
    .expect("create self routine");
    let output: serde_json::Value = serde_json::from_str(&output).expect("routine output");
    let routines = store
        .bots
        .routine_records(Some(&observer), unix_ms() / 1_000)
        .expect("self routines");

    assert_eq!(output["bot_handle"], "observer");
    assert_eq!(routines.len(), 1);
    assert_eq!(routines[0].instructions, "Check the project state.");
}

#[tokio::test]
async fn leader_can_schedule_a_current_swarm_member() {
    let (directory, _checkpoints, store, _deliveries) = store();
    create_swarm(&store).await;
    let leader = bot_id(&store, "leader");
    let reviewer = bot_id(&store, "reviewer");

    BotsBackend::create_routine(
        &store,
        &leader,
        Some("reviewer".into()),
        directory.path(),
        "Review dependency releases.".into(),
        serde_json::json!({
            "kind": "cron",
            "expression": "0 9 * * 1",
            "time_zone": "Asia/Singapore"
        }),
        None,
    )
    .await
    .expect("create member routine");

    assert_eq!(
        store
            .bots
            .routine_records(Some(&reviewer), unix_ms() / 1_000)
            .expect("member routines")
            .len(),
        1
    );
}

#[tokio::test]
async fn leader_cannot_schedule_a_bot_outside_its_swarm() {
    let (directory, _checkpoints, store, _deliveries) = store();
    create_swarm(&store).await;
    let leader = bot_id(&store, "leader");
    let observer = bot_id(&store, "observer");

    let error = BotsBackend::create_routine(
        &store,
        &leader,
        Some("observer".into()),
        directory.path(),
        "Unauthorized work.".into(),
        serde_json::json!({"kind": "interval", "every_seconds": 3600}),
        None,
    )
    .await
    .expect_err("nonmember routine");

    assert!(error.to_string().contains("not a current Swarm member"));
    assert!(
        store
            .bots
            .routine_records(Some(&observer), unix_ms() / 1_000)
            .expect("routines")
            .is_empty()
    );
}

#[tokio::test]
async fn nonleader_cannot_schedule_another_swarm_member() {
    let (directory, _checkpoints, store, _deliveries) = store();
    create_swarm(&store).await;
    let reviewer = bot_id(&store, "reviewer");

    let error = BotsBackend::create_routine(
        &store,
        &reviewer,
        Some("leader".into()),
        directory.path(),
        "Unauthorized work.".into(),
        serde_json::json!({"kind": "interval", "every_seconds": 3600}),
        None,
    )
    .await
    .expect_err("nonleader routine");

    assert!(error.to_string().contains("only a Swarm leader"));
    assert!(
        store
            .bots
            .routine_records(None, unix_ms() / 1_000)
            .expect("routines")
            .is_empty()
    );
}

#[tokio::test]
async fn joining_cannot_exceed_the_member_limit() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    let members = (0..MAX_SWARM_MEMBERS)
        .map(|index| add_bot(&store.bots, &format!("member-{index}")))
        .collect::<Vec<_>>();
    let swarm = store
        .create("Full team".into(), members[0].clone(), members)
        .await
        .expect("full swarm");

    let error = store
        .join(&swarm.id, bot_id(&store, "overflow"))
        .await
        .expect_err("member limit");

    assert!(error.to_string().contains("at most 100"));
}

#[tokio::test]
async fn rename_changes_the_durable_swarm_title() {
    let (_directory, checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;

    store
        .rename(&swarm.id, "Release crew".into())
        .await
        .expect("rename swarm");

    let (reloaded, _deliveries) = reload(checkpoints, &store);
    assert_eq!(
        reloaded.summaries().await.expect("reload")[0].title,
        "Release crew"
    );
}

#[tokio::test]
async fn peer_replies_stop_at_the_private_hop_limit() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    create_swarm(&store).await;
    let leader = bot_id(&store, "leader");
    let reviewer = bot_id(&store, "reviewer");

    let first = post(&store, "leader", "@reviewer please review".into())
        .await
        .expect("initial post");
    let second = store
        .post(
            &reviewer,
            "reviewer-thread",
            "@leader found an issue".into(),
            Some(first.entry.id),
        )
        .await
        .expect("first reply");
    let third = store
        .post(
            &leader,
            "leader-reply-thread",
            "@reviewer please verify the fix".into(),
            Some(second.entry.id),
        )
        .await
        .expect("second reply");
    let fourth = store
        .post(
            &reviewer,
            "reviewer-final-thread",
            "@leader verified".into(),
            Some(third.entry.id),
        )
        .await
        .expect("third reply");

    assert!(
        !store
            .can_reply(&leader, &fourth.entry.id)
            .await
            .expect("reply policy")
    );
    assert!(
        store
            .post(
                &leader,
                "leader-too-deep",
                "@reviewer another loop".into(),
                Some(fourth.entry.id),
            )
            .await
            .expect_err("reply depth")
            .to_string()
            .contains("3-hop")
    );
}

#[tokio::test]
async fn mentions_are_resolved_delivered_and_acknowledged() {
    let (_directory, checkpoints, store, mut deliveries) = store();
    let swarm = create_swarm(&store).await;
    let leader = bot_id(&store, "leader");
    let reviewer = bot_id(&store, "reviewer");
    let leader_handle = swarm
        .members
        .iter()
        .find(|member| member.bot_id == leader)
        .expect("leader")
        .handle
        .clone();
    let reviewer_handle = swarm
        .members
        .iter()
        .find(|member| member.bot_id == reviewer)
        .expect("reviewer")
        .handle
        .clone();

    let unknown = post(&store, "leader", "Can @missing check this?".into())
        .await
        .expect_err("unknown mention");
    assert!(unknown.to_string().contains("@missing"));

    let text = format!("Can @{reviewer_handle} check this?");
    let post = post(&store, "leader", text.clone()).await.expect("post");
    assert_eq!(deliveries.recv().await, Some(SwarmDelivery::Changed));
    assert_eq!(
        deliveries.recv().await,
        Some(SwarmDelivery::Pending {
            target_bot_id: reviewer.clone()
        })
    );
    assert_eq!(post.entry.sequence, 1);
    assert!(post.entry.created_at_ms >= swarm.created_at_ms);
    assert_eq!(post.entry.author.handle, leader_handle);
    assert_eq!(
        post.resolved_recipient_bot_ids,
        std::slice::from_ref(&reviewer)
    );
    for (id, name) in [(&reviewer, "Renamed Reviewer"), (&leader, "Renamed Leader")] {
        let bot = store.bots.bot(id).expect("Bot");
        store
            .bots
            .update_bot(
                id,
                bot.config.revision,
                name,
                &bot.description,
                bot.tint,
                bot.config.config,
            )
            .expect("rename Bot");
    }
    let snapshot = store
        .snapshot_for_bot(&reviewer)
        .await
        .expect("reviewer snapshot")
        .expect("reviewer membership");
    assert_eq!(snapshot.handle, "renamed-reviewer");
    assert_eq!(
        store
            .pending_deliveries(&reviewer)
            .await
            .expect("pending")
            .len(),
        1
    );
    let records = store.records().await.expect("wire records");
    assert_eq!(records[0].messages[0].text, text);
    assert_eq!(records[0].messages[0].author_handle, leader_handle);
    assert_eq!(
        records[0]
            .members
            .iter()
            .find(|member| member.bot_id == reviewer)
            .expect("renamed reviewer projection")
            .handle,
        "renamed-reviewer"
    );

    assert_eq!(
        store
            .acknowledge(&post.entry.id, &reviewer)
            .await
            .expect("acknowledge"),
        AcknowledgeOutcome::Acknowledged
    );
    assert_eq!(
        deliveries.recv().await,
        Some(SwarmDelivery::Acknowledged {
            target_bot_id: reviewer.clone(),
            message_id: post.entry.id.clone(),
        })
    );
    assert_eq!(
        store
            .acknowledge(&post.entry.id, &reviewer)
            .await
            .expect("idempotent acknowledge"),
        AcknowledgeOutcome::AlreadyAcknowledged
    );
    assert_eq!(
        deliveries.recv().await,
        Some(SwarmDelivery::Acknowledged {
            target_bot_id: reviewer.clone(),
            message_id: post.entry.id.clone(),
        })
    );
    assert!(
        store
            .pending_deliveries(&reviewer)
            .await
            .expect("pending")
            .is_empty()
    );

    let (reloaded, _deliveries) = reload(checkpoints, &store);
    let page = reloaded
        .board_page(&swarm.id, None, 10)
        .await
        .expect("board page");
    assert_eq!(page.entries[0].id, post.entry.id);
    assert!(page.entries[0].pending_recipient_bot_ids.is_empty());

    let old_handle = reloaded
        .post(
            &leader,
            "leader-thread",
            format!("Can @{reviewer_handle} check this again?"),
            None,
        )
        .await
        .expect_err("old reviewer handle must not resolve");
    assert!(
        old_handle
            .to_string()
            .contains(&format!("@{reviewer_handle}"))
    );
    let reopened_post = reloaded
        .post(
            &leader,
            "leader-thread",
            "Can @renamed-reviewer check this again?".into(),
            None,
        )
        .await
        .expect("new reviewer handle resolves after reopen");
    assert_eq!(
        reopened_post.resolved_recipient_bot_ids,
        std::slice::from_ref(&reviewer)
    );
}

#[tokio::test]
async fn delivery_claim_reuses_one_durable_participant_conversation() {
    let (_directory, checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    let first = post(&store, "leader", "@reviewer please review".into())
        .await
        .expect("post");
    let leader = bot_id(&store, "leader");
    let reviewer = bot_id(&store, "reviewer");

    let claim = store
        .claim_next_delivery(&reviewer)
        .await
        .expect("claim delivery")
        .expect("pending delivery");
    let session_id = claim.session_id().to_owned();
    Uuid::parse_str(&session_id).expect("generated session UUID");
    assert_eq!(session_id, participant_session_id(&swarm.id, &reviewer));
    assert!(
        store
            .swarm_chat_context(&reviewer, &session_id)
            .await
            .expect("active participant context")
            .is_some()
    );
    assert!(
        store
            .swarm_chat_context(&reviewer, &Uuid::new_v4().to_string())
            .await
            .expect("unrelated conversation context")
            .is_none()
    );
    drop(claim);
    let repeated = store
        .claim_next_delivery(&reviewer)
        .await
        .expect("repeat claim")
        .expect("pending delivery");
    assert_eq!(repeated.session_id(), session_id);
    drop(repeated);
    assert!(
        store
            .claim_next_delivery(&leader)
            .await
            .expect("non-recipient claim")
            .is_none()
    );
    store
        .settle_delivery(
            &first.entry.id,
            &session_id,
            &reviewer,
            SwarmRunOutcome::Succeeded {
                summary: "First review complete".into(),
            },
        )
        .await
        .expect("settle first delivery");
    assert!(
        store
            .swarm_chat_context(&reviewer, &session_id)
            .await
            .expect("settled participant context")
            .is_none()
    );
    let second_post = post(&store, "leader", "@reviewer please review again".into())
        .await
        .expect("second post");
    let second = store
        .claim_next_delivery(&reviewer)
        .await
        .expect("second claim")
        .expect("second pending delivery");
    assert_eq!(second.session_id(), session_id);
    assert_eq!(second.delivery().entry.id, second_post.entry.id);
    drop(second);
    assert!(
        !store
            .settle_delivery(
                "input-client-request",
                &session_id,
                &reviewer,
                SwarmRunOutcome::Succeeded {
                    summary: "unrelated user turn".into(),
                },
            )
            .await
            .expect("ignore unrelated user turn")
    );
    assert!(
        !store
            .settle_delivery(
                &first.entry.id,
                &session_id,
                &reviewer,
                SwarmRunOutcome::Succeeded {
                    summary: "stale replay".into(),
                },
            )
            .await
            .expect("ignore stale settlement")
    );
    assert_eq!(
        store
            .pending_deliveries(&reviewer)
            .await
            .expect("second delivery remains pending")[0]
            .entry
            .id,
        second_post.entry.id
    );
    assert!(
        store
            .swarm_chat_context(&reviewer, &session_id)
            .await
            .expect("reused participant context")
            .is_some()
    );

    let (reloaded, _deliveries) = reload(checkpoints, &store);
    let reloaded = reloaded
        .claim_next_delivery(&reviewer)
        .await
        .expect("reloaded claim")
        .expect("pending delivery");
    assert_eq!(reloaded.session_id(), session_id);
}

#[tokio::test]
async fn claimed_delivery_serializes_leave_until_queue_acceptance() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    post(&store, "leader", "@reviewer please review".into())
        .await
        .expect("post");
    let reviewer = bot_id(&store, "reviewer");
    let claim = store
        .claim_next_delivery(&reviewer)
        .await
        .expect("claim delivery")
        .expect("pending delivery");
    let leaving = store.leave(&swarm.id, &reviewer);
    tokio::pin!(leaving);
    tokio::select! {
        biased;
        result = &mut leaving => panic!("leave settled before queue acceptance: {result:?}"),
        () = std::future::ready(()) => {}
    }

    assert_eq!(
        claim
            .accept(std::future::ready("accepted"))
            .await
            .expect("accept delivery"),
        Some("accepted")
    );
    let left = leaving.await.expect("leave after acceptance");
    assert!(left.members.iter().all(|member| member.bot_id != reviewer));
}

#[tokio::test]
async fn claimed_delivery_serializes_disband_until_queue_acceptance() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    post(&store, "leader", "@reviewer please review".into())
        .await
        .expect("post");
    let reviewer = bot_id(&store, "reviewer");
    let claim = store
        .claim_next_delivery(&reviewer)
        .await
        .expect("claim delivery")
        .expect("pending delivery");
    let disbanding = store.disband(&swarm.id);
    tokio::pin!(disbanding);
    tokio::select! {
        biased;
        result = &mut disbanding => {
            panic!("disband settled before queue acceptance: {result:?}");
        }
        () = std::future::ready(()) => {}
    }

    assert_eq!(
        claim
            .accept(std::future::ready("accepted"))
            .await
            .expect("accept delivery"),
        Some("accepted")
    );
    disbanding.await.expect("disband after acceptance");
    assert!(store.summaries().await.expect("swarms").is_empty());
}

#[tokio::test]
async fn pending_source_sessions_are_protected_until_acknowledgement() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    create_swarm(&store).await;
    let post = post(&store, "leader", "@reviewer please review".into())
        .await
        .expect("post");
    let source_tree = vec!["unrelated-thread".into(), "leader-thread".into()];
    let reviewer = bot_id(&store, "reviewer");

    assert!(
        store
            .has_pending_source_sessions(&source_tree)
            .await
            .expect("pending source")
    );
    store
        .acknowledge(&post.entry.id, &reviewer)
        .await
        .expect("acknowledge");
    assert!(
        !store
            .has_pending_source_sessions(&source_tree)
            .await
            .expect("settled source")
    );
}

#[tokio::test]
async fn leave_and_disband_settle_removed_pending_deliveries() {
    let (_directory, _checkpoints, store, mut deliveries) = store();
    let leader = bot_id(&store, "leader");
    let observer = bot_id(&store, "observer");
    let reviewer = bot_id(&store, "reviewer");
    let swarm = store
        .create(
            "Review team".into(),
            leader.clone(),
            vec![leader, observer.clone(), reviewer.clone()],
        )
        .await
        .expect("create swarm");
    let handle = |bot_id: &str| {
        swarm
            .members
            .iter()
            .find(|member| member.bot_id == bot_id)
            .expect("member")
            .handle
            .clone()
    };
    let post = post(
        &store,
        "leader",
        format!(
            "@{} @{} please review",
            handle(&observer),
            handle(&reviewer)
        ),
    )
    .await
    .expect("post");
    for _ in 0..3 {
        deliveries.recv().await.expect("initial delivery signal");
    }

    store
        .leave(&swarm.id, &reviewer)
        .await
        .expect("leave swarm");
    store.disband(&swarm.id).await.expect("disband swarm");

    assert_eq!(
        [deliveries.recv().await, deliveries.recv().await],
        [
            Some(SwarmDelivery::Acknowledged {
                target_bot_id: reviewer,
                message_id: post.entry.id.clone(),
            }),
            Some(SwarmDelivery::Acknowledged {
                target_bot_id: observer,
                message_id: post.entry.id,
            }),
        ]
    );
}

#[tokio::test]
async fn acknowledgement_is_idempotent_after_its_board_is_gone() {
    let (_directory, checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    let reviewer = bot_id(&store, "reviewer");
    let reviewer_handle = swarm
        .members
        .iter()
        .find(|member| member.bot_id == reviewer)
        .expect("reviewer")
        .handle
        .clone();
    let post = post(&store, "leader", format!("@{reviewer_handle} review this"))
        .await
        .expect("post");
    store.disband(&swarm.id).await.expect("disband");
    let (reloaded, mut deliveries) = reload(checkpoints, &store);

    let outcome = reloaded
        .acknowledge(&post.entry.id, &reviewer)
        .await
        .expect("acknowledge removed board");

    assert_eq!(outcome, AcknowledgeOutcome::MessageGone);
    assert_eq!(
        deliveries.recv().await,
        Some(SwarmDelivery::Acknowledged {
            target_bot_id: reviewer,
            message_id: post.entry.id,
        })
    );
}

#[tokio::test]
async fn acknowledgement_rejects_a_non_recipient_of_a_retained_message() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    create_swarm(&store).await;
    let post = post(&store, "leader", "Board-only update".into())
        .await
        .expect("post");
    let reviewer = bot_id(&store, "reviewer");

    let error = store
        .acknowledge(&post.entry.id, &reviewer)
        .await
        .expect_err("non-recipient acknowledgement");

    assert!(error.to_string().contains("is not a recipient"));
}

#[tokio::test]
async fn board_only_posts_signal_catalog_changes_without_peer_delivery() {
    let (_directory, _checkpoints, store, mut deliveries) = store();
    create_swarm(&store).await;

    post(&store, "leader", "Shared status update".into())
        .await
        .expect("board-only post");

    assert_eq!(deliveries.recv().await, Some(SwarmDelivery::Changed));
    assert!(matches!(
        deliveries.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn model_board_read_stays_valid_json_below_the_tool_output_cap() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    create_swarm(&store).await;
    post(&store, "leader", "\0".repeat(MAX_MESSAGE_BYTES))
        .await
        .expect("escape-heavy post");

    let output = store
        .tool_read(&bot_id(&store, "leader"))
        .await
        .expect("model board read");
    let output: serde_json::Value = serde_json::from_str(&output).expect("valid JSON output");

    assert!(serde_json::to_vec(&output).expect("encoded output").len() <= MAX_TOOL_READ_BYTES);
    assert_eq!(output["entries"][0]["text_truncated"], true);
    assert_eq!(output["has_older"], false);
}

#[tokio::test]
async fn retention_never_evicts_pending_entries() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    let reviewer = bot_id(&store, "reviewer");
    let reviewer_handle = swarm
        .members
        .iter()
        .find(|member| member.bot_id == reviewer)
        .expect("reviewer")
        .handle
        .clone();
    let pending = post(&store, "leader", format!("Please check @{reviewer_handle}"))
        .await
        .expect("pending post");
    for sequence in 0..=MAX_ACKNOWLEDGED_ENTRIES {
        post(&store, "leader", format!("board update {sequence}"))
            .await
            .expect("board post");
    }

    let page = store
        .board_page(&swarm.id, None, MAX_PAGE_ENTRIES)
        .await
        .expect("board page");
    assert_eq!(page.entries.len(), MAX_ACKNOWLEDGED_ENTRIES);
    assert!(page.next_before_sequence.is_some());

    let pending_delivery = store
        .pending_deliveries(&reviewer)
        .await
        .expect("pending delivery");
    assert_eq!(pending_delivery.len(), 1);
    assert_eq!(pending_delivery[0].entry.id, pending.entry.id);
}

#[tokio::test]
async fn posting_backpressures_a_recipient_with_a_full_pending_queue() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    let reviewer = bot_id(&store, "reviewer");
    let reviewer_handle = swarm
        .members
        .iter()
        .find(|member| member.bot_id == reviewer)
        .expect("reviewer")
        .handle
        .clone();
    for sequence in 0..MAX_PENDING_DELIVERIES_PER_RECIPIENT {
        post(
            &store,
            "leader",
            format!("@{reviewer_handle} review {sequence}"),
        )
        .await
        .expect("pending post within limit");
    }

    let error = post(&store, "leader", format!("@{reviewer_handle} one too many"))
        .await
        .expect_err("pending delivery limit");

    assert!(error.to_string().contains("pending swarm messages"));
    assert_eq!(
        store
            .pending_deliveries(&reviewer)
            .await
            .expect("pending deliveries")
            .len(),
        MAX_PENDING_DELIVERIES_PER_RECIPIENT
    );
}

#[tokio::test]
async fn human_and_bot_messages_share_the_leaders_swarm_conversation() {
    let (_directory, checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;

    let posted = store
        .post_user(&swarm.id, "Please coordinate this".into())
        .await
        .expect("human post");

    assert_eq!(posted.entry.author.bot_id, USER_AUTHOR_ID);
    assert_eq!(posted.entry.author.handle, USER_HANDLE);
    assert_eq!(
        posted.resolved_recipient_bot_ids.as_slice(),
        std::slice::from_ref(&swarm.leader_bot_id)
    );
    let (reloaded, _deliveries) = reload(checkpoints, &store);
    let delivery = reloaded
        .claim_next_delivery(&posted.resolved_recipient_bot_ids[0])
        .await
        .expect("claim")
        .expect("leader delivery");
    assert_eq!(delivery.delivery().entry.id, posted.entry.id);
    let session_id = delivery.session_id().to_owned();
    assert_eq!(
        session_id,
        participant_session_id(&swarm.id, &swarm.leader_bot_id)
    );
    drop(delivery);
    reloaded
        .settle_delivery(
            &posted.entry.id,
            &session_id,
            &swarm.leader_bot_id,
            SwarmRunOutcome::Succeeded {
                summary: "Coordinated".into(),
            },
        )
        .await
        .expect("settle human post");

    let peer_post = post(&reloaded, "reviewer", "@leader please follow up".into())
        .await
        .expect("Bot post");
    let peer_delivery = reloaded
        .claim_next_delivery(&swarm.leader_bot_id)
        .await
        .expect("claim Bot post")
        .expect("leader delivery");

    assert_eq!(peer_delivery.session_id(), session_id);
    let context = reloaded
        .swarm_chat_context(&swarm.leader_bot_id, &session_id)
        .await
        .expect("Swarm Chat context")
        .expect("active participant context");
    assert!(context.contains(&posted.entry.id));
    assert!(context.contains(&peer_post.entry.id));
}

#[tokio::test]
async fn terminal_delivery_projects_once_and_wakes_only_the_leader() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    let leader = bot_id(&store, "leader");
    let reviewer = bot_id(&store, "reviewer");
    let posted = post(&store, "leader", "@reviewer review this".into())
        .await
        .expect("post");
    let claim = store
        .claim_next_delivery(&reviewer)
        .await
        .expect("claim")
        .expect("review delivery");
    let session_id = claim.session_id().to_owned();
    drop(claim);

    assert!(
        store
            .settle_delivery(
                &posted.entry.id,
                &session_id,
                &reviewer,
                SwarmRunOutcome::Succeeded {
                    summary: "Looks good".into(),
                },
            )
            .await
            .expect("settle")
    );
    assert!(
        !store
            .settle_delivery(
                &posted.entry.id,
                &session_id,
                &reviewer,
                SwarmRunOutcome::Failed {
                    message: "duplicate".into(),
                },
            )
            .await
            .expect("idempotent settle")
    );
    let page = store
        .board_page(&swarm.id, None, MAX_PAGE_ENTRIES)
        .await
        .expect("board");
    let outcome = page.entries.first().expect("terminal projection");
    assert_eq!(outcome.author.bot_id, reviewer);
    assert_eq!(outcome.pending_recipient_bot_ids, vec![leader]);
    assert_eq!(
        outcome.in_reply_to_message_id.as_deref(),
        Some(posted.entry.id.as_str())
    );
    assert_eq!(outcome.text, "Looks good");
    assert!(
        store
            .pending_deliveries(&reviewer)
            .await
            .expect("reviewer pending")
            .is_empty()
    );
}

#[tokio::test]
async fn terminal_projection_stops_routing_at_the_hop_cap() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    let leader = bot_id(&store, "leader");
    let reviewer = bot_id(&store, "reviewer");
    let first = store
        .post(
            &reviewer,
            "causal-visible-chat",
            "@leader start".into(),
            None,
        )
        .await
        .expect("first hop");
    store
        .acknowledge(&first.entry.id, &leader)
        .await
        .expect("first delivered");
    let second = store
        .post(
            &leader,
            "leader-hidden",
            "@reviewer second".into(),
            Some(first.entry.id.clone()),
        )
        .await
        .expect("second hop");
    store
        .acknowledge(&second.entry.id, &reviewer)
        .await
        .expect("second delivered");
    let third = store
        .post(
            &reviewer,
            "reviewer-hidden",
            "@leader third".into(),
            Some(second.entry.id.clone()),
        )
        .await
        .expect("third hop");
    store
        .acknowledge(&third.entry.id, &leader)
        .await
        .expect("third delivered");
    let fourth = store
        .post(
            &leader,
            "leader-hidden-2",
            "@reviewer fourth".into(),
            Some(third.entry.id.clone()),
        )
        .await
        .expect("fourth hop");
    let claim = store
        .claim_next_delivery(&reviewer)
        .await
        .expect("claim fourth")
        .expect("fourth delivery");
    let session_id = claim.session_id().to_owned();
    drop(claim);

    store
        .settle_delivery(
            &fourth.entry.id,
            &session_id,
            &reviewer,
            SwarmRunOutcome::Succeeded {
                summary: "Finished".into(),
            },
        )
        .await
        .expect("terminal at cap");

    let page = store
        .board_page(&swarm.id, None, MAX_PAGE_ENTRIES)
        .await
        .expect("board");
    let capped = page
        .entries
        .iter()
        .filter(|entry| entry.reply_depth == MAX_TERMINAL_REPLY_DEPTH)
        .collect::<Vec<_>>();
    assert_eq!(capped.len(), 1);
    assert!(capped.iter().all(|entry| {
        entry.in_reply_to_message_id.as_deref() == Some(fourth.entry.id.as_str())
            && entry.mentioned_recipient_bot_ids.is_empty()
            && entry.pending_recipient_bot_ids.is_empty()
    }));
    assert!(
        store
            .pending_deliveries(&leader)
            .await
            .expect("leader delivery")
            .is_empty()
    );
    let state = store.lock_loaded().await.expect("catalog");
    let mut invalid = state.as_ref().expect("loaded catalog").clone();
    let terminal = invalid
        .swarms
        .get_mut(&swarm.id)
        .expect("swarm")
        .board
        .iter_mut()
        .find(|entry| entry.reply_depth == MAX_TERMINAL_REPLY_DEPTH)
        .expect("terminal projection");
    terminal.mentioned_recipient_bot_ids.push(leader);
    assert!(
        validate_catalog(&invalid)
            .expect_err("routable depth-four entry")
            .to_string()
            .contains("routes beyond")
    );
}

#[tokio::test]
async fn swarm_attention_is_durable_and_clears_with_human_reply_or_disband() {
    let (_directory, checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    let leader = bot_id(&store, "leader");
    let attention = store
        .post(
            &leader,
            "hidden-work",
            "Choose the release scope @user".into(),
            None,
        )
        .await
        .expect("post attention");
    let (reloaded, _deliveries) = reload(checkpoints, &store);
    assert_eq!(
        reloaded.pending_attentions().await.expect("attention"),
        vec![SwarmAttention {
            swarm_id: swarm.id.clone(),
            swarm_title: swarm.title.clone(),
            message_id: attention.entry.id,
            bot_id: leader.clone(),
            text: "Choose the release scope".into(),
        }]
    );
    reloaded
        .post(
            &leader,
            "hidden-work",
            "Choose the reviewer @user".into(),
            None,
        )
        .await
        .expect("second attention");
    assert_eq!(
        reloaded
            .pending_attentions()
            .await
            .expect("all attention")
            .len(),
        2
    );

    reloaded
        .post_user(&swarm.id, "Ship the patch.".into())
        .await
        .expect("human reply");
    assert!(
        reloaded
            .pending_attentions()
            .await
            .expect("cleared attention")
            .is_empty()
    );

    reloaded
        .post(
            &leader,
            "hidden-work",
            "One more decision @user".into(),
            None,
        )
        .await
        .expect("second attention");
    reloaded.disband(&swarm.id).await.expect("disband swarm");
    assert!(
        reloaded
            .pending_attentions()
            .await
            .expect("disbanded attention")
            .is_empty()
    );
}

#[tokio::test]
async fn records_keep_old_pending_attention_context_within_the_fixed_page() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    let swarm = create_swarm(&store).await;
    let leader = bot_id(&store, "leader");
    let reviewer = bot_id(&store, "reviewer");
    let reviewer_handle = swarm
        .members
        .iter()
        .find(|member| member.bot_id == reviewer)
        .expect("reviewer")
        .handle
        .clone();
    let parent = post(&store, "leader", format!("@{reviewer_handle} investigate"))
        .await
        .expect("parent post");
    store
        .acknowledge(&parent.entry.id, &reviewer)
        .await
        .expect("acknowledge parent");
    let attention = store
        .post(
            &reviewer,
            "reviewer-thread",
            "Choose an approach @user".into(),
            Some(parent.entry.id.clone()),
        )
        .await
        .expect("attention reply");
    store
        .acknowledge(&attention.entry.id, &leader)
        .await
        .expect("acknowledge leader delivery");

    let mut newest_id = String::new();
    for sequence in 0..=MAX_PAGE_ENTRIES {
        newest_id = post(&store, "leader", format!("newer update {sequence}"))
            .await
            .expect("newer post")
            .entry
            .id;
    }

    let record = store
        .records()
        .await
        .expect("Swarm records")
        .into_iter()
        .find(|record| record.id == swarm.id)
        .expect("Swarm record");
    assert_eq!(record.messages.len(), MAX_PAGE_ENTRIES);
    assert_eq!(record.messages[0].id, parent.entry.id);
    assert_eq!(record.messages[1].id, attention.entry.id);
    assert_eq!(record.messages.last().expect("newest").id, newest_id);
    assert!(
        record
            .messages
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
}

#[tokio::test]
async fn purging_bot_messages_preserves_the_board_sequence_high_water() {
    let (_directory, _checkpoints, store, _deliveries) = store();
    create_swarm(&store).await;
    let first = post(&store, "leader", "first".into()).await.expect("first");
    let removed = post(&store, "reviewer", "removed @user".into())
        .await
        .expect("removed");
    assert_eq!(removed.entry.sequence, first.entry.sequence + 1);

    store
        .remove_bot(&bot_id(&store, "reviewer"))
        .await
        .expect("purge");
    assert!(
        store
            .pending_attentions()
            .await
            .expect("removed Bot attention")
            .is_empty()
    );
    let next = post(&store, "leader", "next".into()).await.expect("next");

    assert_eq!(next.entry.sequence, removed.entry.sequence + 1);
}

#[test]
fn catalog_encoded_size_is_bounded_before_persistence_or_broadcast() {
    let now = unix_ms();
    let author = SwarmMember {
        bot_id: "leader".into(),
        handle: "leader".into(),
        joined_at_ms: now,
    };
    let board = (1..=64)
        .map(|sequence| BoardEntry {
            id: Uuid::new_v4().to_string(),
            sequence,
            created_at_ms: now,
            author: author.clone(),
            source_session_id: "leader-thread".into(),
            text: "\0".repeat(MAX_MESSAGE_BYTES),
            mentioned_recipient_bot_ids: Vec::new(),
            pending_recipient_bot_ids: Vec::new(),
            assigned_recipient_session_ids: BTreeMap::new(),
            in_reply_to_message_id: None,
            reply_depth: 0,
        })
        .collect();
    let catalog = Catalog {
        swarms: BTreeMap::from([(
            Uuid::new_v4().to_string(),
            StoredSwarm {
                title: "Bounded swarm".into(),
                leader_bot_id: author.bot_id.clone(),
                members: BTreeMap::from([(author.bot_id, StoredMember { joined_at_ms: now })]),
                latest_sequence: 64,
                board,
                created_at_ms: now,
                updated_at_ms: now,
            },
        )]),
        ..Catalog::default()
    };

    assert!(
        validate_catalog(&catalog)
            .expect_err("oversized catalog")
            .to_string()
            .contains("encoded bytes")
    );
}

#[test]
fn catalog_rejects_attention_chains_larger_than_the_wire_page() {
    let now = unix_ms();
    let leader = SwarmMember {
        bot_id: "leader".into(),
        handle: "leader".into(),
        joined_at_ms: now,
    };
    let reviewer = SwarmMember {
        bot_id: "reviewer".into(),
        handle: "reviewer".into(),
        joined_at_ms: now,
    };
    let mut board = VecDeque::new();
    let mut pending_attention_message_ids = BTreeSet::new();
    for pair in 0..=MAX_PAGE_ENTRIES / 2 {
        let root_id = Uuid::new_v4().to_string();
        let attention_id = Uuid::new_v4().to_string();
        board.push_back(BoardEntry {
            id: root_id.clone(),
            sequence: u64::try_from(pair * 2 + 1).expect("sequence"),
            created_at_ms: now,
            author: leader.clone(),
            source_session_id: "leader-thread".into(),
            text: "Work".into(),
            mentioned_recipient_bot_ids: vec![reviewer.bot_id.clone()],
            pending_recipient_bot_ids: Vec::new(),
            assigned_recipient_session_ids: BTreeMap::new(),
            in_reply_to_message_id: None,
            reply_depth: 0,
        });
        board.push_back(BoardEntry {
            id: attention_id.clone(),
            sequence: u64::try_from(pair * 2 + 2).expect("sequence"),
            created_at_ms: now,
            author: reviewer.clone(),
            source_session_id: "reviewer-thread".into(),
            text: "Needs input @user".into(),
            mentioned_recipient_bot_ids: Vec::new(),
            pending_recipient_bot_ids: Vec::new(),
            assigned_recipient_session_ids: BTreeMap::new(),
            in_reply_to_message_id: Some(root_id),
            reply_depth: 1,
        });
        pending_attention_message_ids.insert(attention_id);
    }
    let message_count = board.len();
    let swarm_id = Uuid::new_v4().to_string();
    let catalog = Catalog {
        swarms: BTreeMap::from([(
            swarm_id,
            StoredSwarm {
                title: "Review team".into(),
                leader_bot_id: leader.bot_id.clone(),
                members: BTreeMap::from([
                    (leader.bot_id, StoredMember { joined_at_ms: now }),
                    (reviewer.bot_id, StoredMember { joined_at_ms: now }),
                ]),
                latest_sequence: u64::try_from(message_count).expect("latest sequence"),
                board,
                created_at_ms: now,
                updated_at_ms: now,
            },
        )]),
        pending_swarm_attention_message_ids: pending_attention_message_ids,
    };

    assert_eq!(
        message_count,
        MAX_PAGE_ENTRIES + 2,
        "the limit must include each attention's parent"
    );
    assert!(
        validate_catalog(&catalog)
            .expect_err("oversized attention context")
            .to_string()
            .contains("pending attention messages and ancestors")
    );
}

#[test]
fn mention_parser_ignores_email_boundaries_and_deduplicates() {
    assert_eq!(
        mentioned_handles("@one-bot mail@two @one-bot; @three"),
        BTreeSet::from(["one-bot".into(), "three".into()])
    );
}

#[test]
fn swarm_attention_text_removes_only_the_reserved_marker() {
    assert_eq!(
        swarm_attention_text("mail@user Choose a release @user"),
        "mail@user Choose a release"
    );
    assert_eq!(swarm_attention_text("@user"), "Needs your attention.");
    assert_eq!(swarm_attention_text("@user."), "Needs your attention.");

    let preview = swarm_attention_text(&format!("@user {}", "🦀".repeat(200)));
    assert!(preview.len() <= MAX_ATTENTION_TEXT_BYTES);
    assert!(preview.ends_with('…'));
    assert!(
        preview[..preview.len() - '…'.len_utf8()]
            .chars()
            .all(|character| character == '🦀')
    );
}
