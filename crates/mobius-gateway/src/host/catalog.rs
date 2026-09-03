use std::collections::BTreeMap;
use std::sync::Arc;

use mobius::backend::checkpoint::{CheckpointStore, SessionPageRequest};
use serde::{Deserialize, Serialize};

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

pub(super) async fn session_catalog(
    checkpoints: &Arc<dyn CheckpointStore>,
    activities: &SessionActivities,
) -> Result<Vec<SessionRecord>> {
    filtered_session_catalog(checkpoints, activities, CatalogFilter::Visible).await
}

pub(super) async fn hidden_bot_session_catalog(
    checkpoints: &Arc<dyn CheckpointStore>,
    activities: &SessionActivities,
    bot_id: &str,
) -> Result<Vec<SessionRecord>> {
    filtered_session_catalog(checkpoints, activities, CatalogFilter::HiddenBot(bot_id)).await
}

pub(super) async fn restore_pending_approval_activities(
    checkpoints: &Arc<dyn CheckpointStore>,
    activities: &SessionActivities,
) -> Result<()> {
    let mut cursor = None;
    let mut restored = Vec::new();
    loop {
        let page = checkpoints
            .list_sessions_page(SessionPageRequest {
                cursor,
                limit: SESSION_PAGE_SIZE,
            })
            .await?;
        for summary in page.sessions {
            let Some(checkpoint) = checkpoints.load(&summary.session_id).await? else {
                continue;
            };
            let Some(approval) = checkpoint
                .pending_approval
                .filter(|approval| !approval.decision_received)
            else {
                continue;
            };
            restored.push((
                summary.session_id,
                SessionActivity {
                    state: SessionActivityState::AwaitingApproval,
                    turn_id: Some(approval.turn_id),
                    approval_request_id: Some(approval.request_id),
                    started_at: checkpoint
                        .active_execution
                        .map(|execution| execution.started_at_ms.div_euclid(1_000)),
                    ..SessionActivity::default()
                },
            ));
        }
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }
    activities
        .lock()
        .map_err(|_| Error::Config("session activity lock is poisoned".into()))?
        .extend(restored);
    Ok(())
}

pub(super) async fn background_approvals(
    checkpoints: &Arc<dyn CheckpointStore>,
    activities: &SessionActivities,
) -> Result<Vec<BackgroundApproval>> {
    let mut pending = activities
        .lock()
        .map_err(|_| Error::Config("session activity lock is poisoned".into()))?
        .iter()
        .filter(|(_, activity)| activity.state == SessionActivityState::AwaitingApproval)
        .map(|(session_id, activity)| {
            Ok((
                session_id.clone(),
                (
                    activity
                        .turn_id
                        .clone()
                        .ok_or_else(|| Error::Config("approval activity has no turn id".into()))?,
                    activity.approval_request_id.clone().ok_or_else(|| {
                        Error::Config("approval activity has no request id".into())
                    })?,
                ),
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut approvals = Vec::new();
    let mut cursor = None;
    while !pending.is_empty() {
        let page = checkpoints
            .list_sessions_page(SessionPageRequest {
                cursor,
                limit: SESSION_PAGE_SIZE,
            })
            .await?;
        for summary in page.sessions {
            let Some((turn_id, request_id)) = pending.remove(&summary.session_id) else {
                continue;
            };
            if !summary.catalog_visible && summary.parent_session_id.is_none() {
                approvals.push(BackgroundApproval {
                    session_id: summary.session_id,
                    bot_id: summary.session_context.bot_id,
                    turn_id,
                    request_id,
                });
            }
        }
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }
    approvals.sort_by(|left, right| left.session_id.cmp(&right.session_id));
    Ok(approvals)
}

#[derive(Clone, Copy)]
enum CatalogFilter<'a> {
    Visible,
    HiddenBot(&'a str),
}

async fn filtered_session_catalog(
    checkpoints: &Arc<dyn CheckpointStore>,
    activities: &SessionActivities,
    filter: CatalogFilter<'_>,
) -> Result<Vec<SessionRecord>> {
    let metadata = load_session_metadata(checkpoints).await?;
    let mut cursor = None;
    let mut sessions = Vec::new();
    while sessions.len() < SESSION_PAGE_SIZE {
        let page = checkpoints
            .list_sessions_page(SessionPageRequest {
                cursor,
                limit: SESSION_PAGE_SIZE,
            })
            .await?;
        sessions.extend(page.sessions.into_iter().filter(|session| {
            let manually_hidden = metadata
                .get(&session.session_id)
                .is_some_and(|metadata| metadata.hidden);
            match filter {
                CatalogFilter::Visible => session.catalog_visible && !manually_hidden,
                CatalogFilter::HiddenBot(bot_id) => {
                    session.session_context.bot_id == bot_id
                        && session.parent_session_id.is_none()
                        && !session.catalog_visible
                }
            }
        }));
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }
    sessions.truncate(SESSION_PAGE_SIZE);
    for session in &mut sessions {
        if let Some(message) = &mut session.first_user_message
            && message.len() > MAX_SESSION_PREVIEW_BYTES
        {
            let mut end = MAX_SESSION_PREVIEW_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
    }
    let activities = activities
        .lock()
        .map_err(|_| Error::Config("session activity lock is poisoned".into()))?;
    let mut sessions = sessions
        .into_iter()
        .map(|summary| {
            let metadata = metadata.get(&summary.session_id);
            let activity = activities
                .get(&summary.session_id)
                .cloned()
                .unwrap_or_default();
            SessionRecord {
                session_id: summary.session_id,
                session_context: summary.session_context,
                parent_session_id: summary.parent_session_id,
                parent_sequence: summary.parent_sequence,
                sequence: summary.sequence,
                first_user_message: summary.first_user_message,
                execution_stats: summary.execution_stats,
                title: metadata.and_then(|metadata| metadata.title.clone()),
                pinned: metadata.is_some_and(|metadata| metadata.pinned),
                activity,
                created_at: summary.created_at,
                updated_at: summary.updated_at,
            }
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        right
            .pinned
            .cmp(&left.pinned)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| right.sequence.cmp(&left.sequence))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    Ok(sessions)
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
    use mobius::backend::checkpoint::{
        ActiveExecution, Checkpoint, ExecutionPhase, PendingApproval, sqlite::SqliteCheckpoint,
    };

    use crate::wire::{SessionActivity, SessionActivityState};

    use super::*;

    fn activities() -> SessionActivities {
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    #[tokio::test]
    async fn session_catalog_includes_empty_roots_and_fresh_forks() {
        let workspace = tempfile::tempdir().expect("workspace");
        let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
            SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
                .expect("checkpoints"),
        );
        let mut parent = Checkpoint::empty("parent");
        parent.session_context.bot_id = "bot-fixture".into();
        parent.session_context.workspace_id = Some("workspace".into());
        parent.sequence = 1;
        checkpoints
            .save(&parent, &[], None)
            .await
            .expect("save parent");
        let mut empty_root = Checkpoint::empty("empty-root");
        empty_root.session_context.bot_id = "bot-fixture".into();
        empty_root.session_context.workspace_id = Some("workspace".into());
        checkpoints
            .save(&empty_root, &[], None)
            .await
            .expect("save empty root");
        let mut child = Checkpoint::empty("child");
        child.session_context.bot_id = "bot-fixture".into();
        child.session_context.workspace_id = Some("workspace".into());
        checkpoints
            .fork("parent", parent.sequence, &child)
            .await
            .expect("fork parent");

        let mut sessions = session_catalog(&checkpoints, &activities())
            .await
            .expect("session catalog")
            .into_iter()
            .map(|record| (record.session_id, record.parent_session_id))
            .collect::<Vec<_>>();
        sessions.sort();

        assert_eq!(
            sessions,
            vec![
                ("child".into(), Some("parent".into())),
                ("empty-root".into(), None),
                ("parent".into(), None)
            ]
        );
    }

    #[tokio::test]
    async fn session_catalog_is_bounded_and_truncates_utf8_previews() {
        let workspace = tempfile::tempdir().expect("workspace");
        let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
            SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
                .expect("checkpoints"),
        );
        for index in 0..=SESSION_PAGE_SIZE {
            let mut checkpoint = Checkpoint::empty(format!("{index:03}"));
            checkpoint.session_context.bot_id = "bot-fixture".into();
            checkpoint.session_context.workspace_id = Some("workspace".into());
            checkpoint.sequence = 1;
            checkpoint.first_user_message = Some(if index == SESSION_PAGE_SIZE {
                "€".repeat(MAX_SESSION_PREVIEW_BYTES / '€'.len_utf8() + 1)
            } else {
                format!("chat {index}")
            });
            checkpoints
                .save(&checkpoint, &[], None)
                .await
                .expect("save chat");
        }

        let sessions = session_catalog(&checkpoints, &activities())
            .await
            .expect("session catalog");
        let preview = sessions
            .iter()
            .find(|session| session.session_id == "100")
            .and_then(|session| session.first_user_message.as_deref())
            .expect("UTF-8 preview");

        assert_eq!(sessions.len(), SESSION_PAGE_SIZE);
        assert!(sessions.iter().all(|session| session.session_id != "000"));
        assert_eq!(
            preview,
            "€".repeat(MAX_SESSION_PREVIEW_BYTES / '€'.len_utf8())
        );
    }

    #[tokio::test]
    async fn session_catalog_attaches_gateway_activity() {
        let workspace = tempfile::tempdir().expect("workspace");
        let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
            SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
                .expect("checkpoints"),
        );
        let mut checkpoint = Checkpoint::empty("active");
        checkpoint.session_context.bot_id = "bot-fixture".into();
        checkpoints
            .save(&checkpoint, &[], None)
            .await
            .expect("save session");
        let activities = activities();
        activities.lock().expect("activities").insert(
            "active".into(),
            SessionActivity {
                state: SessionActivityState::Running,
                turn_id: Some("turn-a".into()),
                started_at: Some(1),
                ..SessionActivity::default()
            },
        );

        let sessions = session_catalog(&checkpoints, &activities)
            .await
            .expect("session catalog");

        assert_eq!(sessions[0].activity.state, SessionActivityState::Running);
    }

    #[tokio::test]
    async fn hidden_bot_catalog_contains_only_owned_roots() {
        let workspace = tempfile::tempdir().expect("workspace");
        let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
            SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
                .expect("checkpoints"),
        );
        let mut root = Checkpoint::empty("hidden-root");
        root.catalog_visible = false;
        root.session_context.bot_id = "bot-a".into();
        root.sequence = 1;
        checkpoints.save(&root, &[], None).await.expect("save root");
        let mut child = Checkpoint::empty("hidden-child");
        child.catalog_visible = false;
        child.session_context.bot_id = "bot-a".into();
        checkpoints
            .fork("hidden-root", 1, &child)
            .await
            .expect("fork child");
        let mut other = Checkpoint::empty("other-root");
        other.catalog_visible = false;
        other.session_context.bot_id = "bot-b".into();
        checkpoints
            .save(&other, &[], None)
            .await
            .expect("save other");

        let sessions = hidden_bot_session_catalog(&checkpoints, &activities(), "bot-a")
            .await
            .expect("hidden Bot sessions");

        assert_eq!(
            sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["hidden-root"]
        );
    }

    #[tokio::test]
    async fn restores_and_exposes_only_hidden_pending_approvals() {
        let workspace = tempfile::tempdir().expect("workspace");
        let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
            SqliteCheckpoint::new(workspace.path().join("checkpoints.sqlite3"))
                .expect("checkpoints"),
        );
        for (session_id, visible) in [("background", false), ("chat", true)] {
            let mut checkpoint = Checkpoint::empty(session_id);
            checkpoint.catalog_visible = visible;
            checkpoint.session_context.bot_id = "bot-a".into();
            checkpoint.active_execution = Some(ActiveExecution {
                submission_id: "submission".into(),
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
                submission_id: "submission".into(),
                turn_id: "turn".into(),
                request_id: format!("request-{session_id}"),
                approval_call_ids: Vec::new(),
                authorized_call_ids: Vec::new(),
                calls: Vec::new(),
                reason: "Approve command".into(),
                sandbox_mode: Default::default(),
                network_access: Default::default(),
                decision_received: false,
            });
            checkpoints
                .save(&checkpoint, &[], None)
                .await
                .expect("save session");
        }
        let mut child = Checkpoint::empty("background-child");
        child.catalog_visible = false;
        child.session_context.bot_id = "bot-a".into();
        checkpoints
            .fork("background", 0, &child)
            .await
            .expect("fork session");
        let activities = activities();

        restore_pending_approval_activities(&checkpoints, &activities)
            .await
            .expect("restore approvals");
        activities.lock().expect("activities").insert(
            "background-child".into(),
            SessionActivity {
                state: SessionActivityState::AwaitingApproval,
                turn_id: Some("child-turn".into()),
                approval_request_id: Some("child-request".into()),
                ..SessionActivity::default()
            },
        );
        let approvals = background_approvals(&checkpoints, &activities)
            .await
            .expect("background approvals");

        assert_eq!(
            approvals,
            vec![BackgroundApproval {
                session_id: "background".into(),
                bot_id: "bot-a".into(),
                turn_id: "turn".into(),
                request_id: "request-background".into(),
            }]
        );
    }

    #[test]
    fn session_titles_are_trimmed_and_bounded() {
        assert_eq!(
            validate_session_title("  hello  ").expect("valid title"),
            "hello"
        );
        assert_eq!(
            validate_session_title(" ").expect_err("blank title").code,
            "invalid_session_title"
        );
        assert!(validate_session_title(&"x".repeat(MAX_SESSION_TITLE_BYTES + 1)).is_err());
    }
}
