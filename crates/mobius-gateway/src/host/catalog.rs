use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use mobius::backend::checkpoint::CheckpointStore;
use serde::{Deserialize, Serialize};

use crate::bots::BotStore;
use crate::chats::{Chat, ChatStore};
use crate::wire::{BackgroundApproval, SessionActivity, SessionActivityState, SessionRecord};
use crate::{Error, Result};

use super::{Rejection, SessionActivities};

const SESSION_PAGE_SIZE: usize = 100;
const SESSION_CATALOG_SCOPE: &str = "gateway";
const SESSION_CATALOG_KEY: &str = "session_catalog";
const MAX_SESSION_TITLE_BYTES: usize = 256;
const MAX_SESSION_PREVIEW_BYTES: usize = 512;

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct SessionMetadata {
    pub(super) title: Option<String>,
    pub(super) pinned: bool,
    pub(super) hidden: bool,
}

pub(super) type SessionCatalogMetadata = BTreeMap<String, SessionMetadata>;

#[derive(Default)]
pub(super) struct SessionCatalog {
    pub(super) activities: HashMap<String, SessionActivity>,
    pub(super) approvals: BTreeMap<String, BackgroundApproval>,
    pub(super) snapshot: Option<SessionCatalogMetadata>,
}

pub(super) async fn session_catalog(
    checkpoints: &Arc<dyn CheckpointStore>,
    chats: &ChatStore,
    activities: &SessionActivities,
) -> Result<Vec<SessionRecord>> {
    let mut catalog = activities.lock().await;
    catalog.snapshot = Some(load_session_metadata(checkpoints).await?);
    chat_catalog(checkpoints, chats, &catalog).await
}

pub(super) async fn activity_catalog(
    checkpoints: &Arc<dyn CheckpointStore>,
    chats: &ChatStore,
    activities: &SessionActivities,
) -> Result<(Vec<SessionRecord>, Vec<BackgroundApproval>)> {
    let mut catalog = activities.lock().await;
    if catalog.snapshot.is_none() {
        catalog.snapshot = Some(load_session_metadata(checkpoints).await?);
    }
    Ok((
        chat_catalog(checkpoints, chats, &catalog).await?,
        catalog.approvals.values().cloned().collect(),
    ))
}

async fn chat_catalog(
    checkpoints: &Arc<dyn CheckpointStore>,
    chats: &ChatStore,
    catalog: &SessionCatalog,
) -> Result<Vec<SessionRecord>> {
    let chats = chats.chats(false).await?;
    let mut sessions = Vec::new();
    for chat in &chats {
        let metadata = catalog
            .snapshot
            .as_ref()
            .and_then(|items| items.get(&chat.id));
        if metadata.is_some_and(|item| item.hidden) {
            continue;
        }
        let activity = chat
            .participants
            .iter()
            .filter_map(|participant| catalog.activities.get(&participant.session_id))
            .max_by_key(|activity| match activity.state {
                SessionActivityState::Idle => 0,
                SessionActivityState::Running => 1,
                SessionActivityState::AwaitingApproval => 2,
            })
            .cloned()
            .unwrap_or_default();
        let mut preview = chat.first_user_message.clone();
        if let Some(message) = &mut preview {
            message.truncate(
                message.floor_char_boundary(MAX_SESSION_PREVIEW_BYTES.min(message.len())),
            );
        }
        sessions.push(SessionRecord {
            session_context: chat.context(),
            member_bot_ids: chat.member_bot_ids(),
            primary_bot_id: chat.primary_bot_id.clone(),
            session_id: chat.id.clone(),
            parent_session_id: None,
            parent_sequence: None,
            sequence: chat.sequence,
            first_user_message: preview,
            execution_stats: Default::default(),
            title: metadata.and_then(|item| item.title.clone()),
            pinned: metadata.is_some_and(|item| item.pinned),
            activity,
            created_at: chat.created_at,
            updated_at: chat.updated_at,
        });
    }
    sort_sessions(&mut sessions);
    sessions.truncate(SESSION_PAGE_SIZE);
    for chat in &chats {
        let Some(session) = sessions
            .iter_mut()
            .find(|session| session.session_id == chat.id)
        else {
            continue;
        };
        for participant in chat.execution_participants() {
            if let Some(summary) = checkpoints.session_summary(&participant.session_id).await? {
                session
                    .execution_stats
                    .checked_add(&summary.execution_stats)
                    .ok_or_else(|| {
                        Error::Config("Chat execution statistics exceed supported totals".into())
                    })?;
            }
        }
    }
    Ok(sessions)
}

pub(super) async fn restore_pending_approval_activities(
    checkpoints: &Arc<dyn CheckpointStore>,
    chats: &ChatStore,
    bots: &BotStore,
    activities: &SessionActivities,
) -> Result<()> {
    let mut catalog = activities.lock().await;
    for chat in chats.chats(false).await? {
        for participant in &chat.participants {
            let Some(checkpoint) = checkpoints.load(&participant.session_id).await? else {
                continue;
            };
            let Some(approval) = checkpoint
                .pending_approval
                .filter(|approval| !approval.decision_received)
            else {
                continue;
            };
            let request = approval.request_event();
            let activity = SessionActivity {
                state: SessionActivityState::AwaitingApproval,
                turn_id: Some(approval.turn_id),
                approval_request_id: Some(approval.request_id),
                started_at: checkpoint
                    .active_execution
                    .map(|execution| execution.started_at_ms.div_euclid(1_000)),
                ..SessionActivity::default()
            };
            catalog.update(
                &participant.session_id,
                Some(&chat),
                None,
                Some(request),
                activity,
            );
        }
    }
    for run in bots.history(None)? {
        let Some(session_id) = run.session_id else {
            continue;
        };
        let Some(checkpoint) = checkpoints.load(&session_id).await? else {
            continue;
        };
        let Some(approval) = checkpoint
            .pending_approval
            .filter(|approval| !approval.decision_received)
        else {
            continue;
        };
        let request = approval.request_event();
        let activity = SessionActivity {
            state: SessionActivityState::AwaitingApproval,
            turn_id: Some(approval.turn_id),
            approval_request_id: Some(approval.request_id),
            started_at: checkpoint
                .active_execution
                .map(|execution| execution.started_at_ms.div_euclid(1_000)),
            ..SessionActivity::default()
        };
        catalog.update(
            &session_id,
            None,
            Some(&run.bot_id),
            Some(request),
            activity,
        );
    }

    Ok(())
}

pub(super) async fn background_approvals(
    activities: &SessionActivities,
) -> Vec<BackgroundApproval> {
    activities
        .lock()
        .await
        .approvals
        .values()
        .cloned()
        .collect()
}

pub(super) async fn update_session_activity(
    checkpoints: &Arc<dyn CheckpointStore>,
    chats: &ChatStore,
    bots: &BotStore,
    activities: &SessionActivities,
    session_id: &str,
    activity: SessionActivity,
) -> Result<()> {
    let chat = chats.chat_for_session(session_id).await?;
    let routine_bot_id =
        if chat.is_none() && activity.state == SessionActivityState::AwaitingApproval {
            bots.routine_session_bot_id(session_id)?
        } else {
            None
        };
    let request = if activity.state == SessionActivityState::AwaitingApproval {
        Some(
            checkpoints
                .load(session_id)
                .await?
                .and_then(|checkpoint| checkpoint.pending_approval)
                .filter(|approval| {
                    !approval.decision_received
                        && Some(&approval.request_id) == activity.approval_request_id.as_ref()
                        && Some(&approval.turn_id) == activity.turn_id.as_ref()
                })
                .ok_or_else(|| {
                    Error::Config("approval activity has no matching pending request".into())
                })?
                .request_event(),
        )
    } else {
        None
    };
    activities.lock().await.update(
        session_id,
        chat.as_ref(),
        routine_bot_id.as_deref(),
        request,
        activity,
    );
    Ok(())
}

impl SessionCatalog {
    fn update(
        &mut self,
        session_id: &str,
        chat: Option<&Chat>,
        routine_bot_id: Option<&str>,
        request: Option<mobius::protocol::ExecApprovalRequestEvent>,
        activity: SessionActivity,
    ) {
        let owner = if let Some(chat) = chat {
            chat.participants
                .iter()
                .find(|participant| participant.session_id == session_id)
                .map(|participant| (Some(chat.id.as_str()), participant.bot_id.as_str()))
        } else {
            routine_bot_id.map(|bot_id| (None, bot_id))
        };
        let approval = match (owner, request) {
            (Some((chat_id, bot_id)), Some(request)) => Some(BackgroundApproval {
                chat_id: chat_id.map(str::to_owned),
                bot_id: bot_id.into(),
                request,
            }),
            _ => None,
        };
        self.approvals.remove(session_id);
        if let Some(approval) = approval {
            self.approvals.insert(session_id.into(), approval);
        }
        self.activities.insert(session_id.into(), activity);
    }
}

fn sort_sessions(sessions: &mut [SessionRecord]) {
    sessions.sort_by(|left, right| {
        right
            .pinned
            .cmp(&left.pinned)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| right.sequence.cmp(&left.sequence))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
}

pub(super) async fn load_session_metadata(
    checkpoints: &Arc<dyn CheckpointStore>,
) -> Result<SessionCatalogMetadata> {
    let Some(value) = checkpoints
        .load_state(SESSION_CATALOG_SCOPE, SESSION_CATALOG_KEY)
        .await?
    else {
        return Ok(SessionCatalogMetadata::default());
    };
    Ok(serde_json::from_value(value)?)
}

pub(super) async fn save_session_metadata(
    checkpoints: &Arc<dyn CheckpointStore>,
    metadata: &SessionCatalogMetadata,
) -> Result<()> {
    checkpoints
        .save_state(
            SESSION_CATALOG_SCOPE,
            SESSION_CATALOG_KEY,
            &serde_json::to_value(metadata)?,
        )
        .await?;
    Ok(())
}

pub(super) fn validate_session_title(title: &str) -> std::result::Result<&str, Rejection> {
    let title = title.trim();
    if title.is_empty() || title.len() > MAX_SESSION_TITLE_BYTES {
        return Err(Rejection {
            code: "invalid_session_title",
            message: format!("chat title must be 1–{MAX_SESSION_TITLE_BYTES} UTF-8 bytes"),
            fatal: false,
        });
    }
    Ok(title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobius::backend::checkpoint::{
        ActiveExecution, Checkpoint, ExecutionPhase, PendingApproval, sqlite::SqliteCheckpoint,
    };

    #[tokio::test]
    async fn catalog_lists_only_public_chats_and_projects_participant_activity() {
        let root = tempfile::tempdir().unwrap();
        let bots = Arc::new(BotStore::open(root.path()).unwrap());
        let first = bots
            .create_bot("First", "First", Default::default())
            .unwrap();
        let second = bots
            .create_bot("Second", "Second", Default::default())
            .unwrap();
        let (chats, _) = ChatStore::new(root.path(), bots.clone()).unwrap();
        let id = chats
            .create(
                root.path().into(),
                vec![first.id.clone(), second.id.clone()],
                Some(&first.id),
            )
            .await
            .unwrap();
        let other = chats
            .create(
                root.path().into(),
                vec![second.id.clone()],
                Some(&second.id),
            )
            .await
            .unwrap();
        let chat = chats.load(&id).await.unwrap().unwrap();
        let checkpoints: Arc<dyn CheckpointStore> =
            Arc::new(SqliteCheckpoint::new(root.path().join("checkpoints.sqlite3")).unwrap());
        for session_id in [
            "unowned-visible-checkpoint",
            chat.session_id(&first.id).unwrap(),
        ] {
            let mut checkpoint = Checkpoint::empty(session_id);
            checkpoint.execution_stats.run_count = if session_id == "unowned-visible-checkpoint" {
                99
            } else {
                3
            };
            checkpoints.save(&checkpoint, &[], None).await.unwrap();
        }
        save_session_metadata(
            &checkpoints,
            &BTreeMap::from([(
                id.clone(),
                SessionMetadata {
                    title: Some("Pinned chat".into()),
                    pinned: true,
                    hidden: false,
                },
            )]),
        )
        .await
        .unwrap();
        let activities = Arc::new(tokio::sync::Mutex::new(SessionCatalog::default()));
        update_session_activity(
            &checkpoints,
            &chats,
            &bots,
            &activities,
            chat.session_id(&first.id).unwrap(),
            SessionActivity {
                state: SessionActivityState::Running,
                turn_id: Some("active-turn".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let sessions = session_catalog(&checkpoints, &chats, &activities)
            .await
            .unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, id);
        assert_eq!(
            sessions[0].primary_bot_id.as_deref(),
            Some(first.id.as_str())
        );
        assert_eq!(sessions[0].member_bot_ids, chat.member_bot_ids());
        assert_eq!(sessions[0].title.as_deref(), Some("Pinned chat"));
        assert_eq!(sessions[0].activity.state, SessionActivityState::Running);
        assert_eq!(sessions[1].session_id, other);
        assert_eq!(sessions[0].execution_stats.run_count, 3);
        assert_eq!(sessions[1].execution_stats.run_count, 0);
    }

    #[tokio::test]
    async fn approval_restore_resolves_chat_and_routine_ownership_without_checkpoint_identity() {
        let root = tempfile::tempdir().unwrap();
        let bots = Arc::new(BotStore::open(root.path()).unwrap());
        let bot = bots
            .create_bot("First", "First", Default::default())
            .unwrap();
        let (chats, _) = ChatStore::new(root.path(), bots.clone()).unwrap();
        let chat_id = chats
            .create(root.path().into(), vec![bot.id.clone()], Some(&bot.id))
            .await
            .unwrap();
        let chat = chats.load(&chat_id).await.unwrap().unwrap();
        let routine = bots
            .create_routine(
                &bot.id,
                root.path(),
                "Test",
                crate::wire::RoutineSchedule {
                    kind: crate::wire::RoutineScheduleKind::Once,
                    at: Some(chrono::Utc::now().timestamp() + 60),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
                None,
            )
            .unwrap();
        let crate::bots::BeginRun::Started(run) = bots.begin_run(&routine.id).unwrap() else {
            panic!("routine starts");
        };
        let private_id = chat.session_id(&bot.id).unwrap();
        let checkpoints: Arc<dyn CheckpointStore> =
            Arc::new(SqliteCheckpoint::new(root.path().join("checkpoints.sqlite3")).unwrap());
        for id in [private_id, run.session_id(), "unowned-approval"] {
            let mut checkpoint = Checkpoint::empty(id);
            checkpoint.active_execution = Some(ActiveExecution {
                submission_id: "original".into(),
                turn_id: "turn".into(),
                started_at_ms: 1_000,
                model_calls: 1,
                tool_calls: 0,
                failed_tool_calls: 0,
                usage: Default::default(),
                next_model_step: 1,
                stop_hook_active: false,
                phase: ExecutionPhase::Model,
            });
            checkpoint.pending_approval = Some(PendingApproval {
                submission_id: "original".into(),
                turn_id: "turn".into(),
                request_id: format!("approval-{id}"),
                approval_call_ids: Vec::new(),
                authorized_call_ids: Vec::new(),
                calls: Vec::new(),
                reason: "Test".into(),
                sandbox_mode: Default::default(),
                network_access: Default::default(),
                decision_received: false,
            });
            checkpoints.save(&checkpoint, &[], None).await.unwrap();
        }
        assert_eq!(
            bots.routine_session_bot_id(run.session_id()).unwrap(),
            Some(bot.id.clone())
        );
        assert_eq!(bots.routine_session_bot_id(private_id).unwrap(), None);
        let activities = Arc::new(tokio::sync::Mutex::new(SessionCatalog::default()));
        restore_pending_approval_activities(&checkpoints, &chats, &bots, &activities)
            .await
            .unwrap();
        let approvals = background_approvals(&activities).await;
        assert_eq!(approvals.len(), 2);
        assert!(approvals.iter().all(|approval| approval.bot_id == bot.id));
        assert!(approvals.iter().any(|approval| approval.chat_id.as_deref()
            == Some(chat_id.as_str())
            && approval.request.id == format!("approval-{private_id}")));
        assert!(approvals.iter().any(|approval| approval.chat_id.is_none()
            && approval.request.id == format!("approval-{}", run.session_id())));
        assert_eq!(
            session_catalog(&checkpoints, &chats, &activities)
                .await
                .unwrap()
                .len(),
            1
        );
        for id in [private_id, run.session_id()] {
            update_session_activity(
                &checkpoints,
                &chats,
                &bots,
                &activities,
                id,
                SessionActivity::default(),
            )
            .await
            .unwrap();
        }
        assert!(background_approvals(&activities).await.is_empty());
    }
}
