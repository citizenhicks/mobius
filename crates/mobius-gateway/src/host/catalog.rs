use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use mobius::backend::checkpoint::{CheckpointStore, SessionPageRequest, SessionSummary};
use serde::{Deserialize, Serialize};

use crate::wire::{BackgroundApproval, SessionActivity, SessionActivityState, SessionRecord};
use crate::{Error, Result};

use super::{Rejection, SessionActivities};

use super::SESSION_PAGE_SIZE;
const SESSION_CATALOG_SCOPE: &str = "gateway";
const SESSION_CATALOG_KEY: &str = "session_catalog";
const MAX_SESSION_TITLE_BYTES: usize = 256;
const MAX_SESSION_PREVIEW_BYTES: usize = 512;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionMetadata {
    pub(super) title: Option<String>,
    pub(super) pinned: bool,
}

pub(super) type SessionCatalogMetadata = BTreeMap<String, SessionMetadata>;

#[derive(Default)]
pub(super) struct SessionCatalog {
    pub(super) activities: HashMap<Arc<str>, SessionActivity>,
    pub(super) approvals: BTreeMap<Arc<str>, Arc<BackgroundApproval>>,
    pub(super) snapshot: Option<CatalogSnapshot>,
}

pub(super) struct CatalogSnapshot {
    metadata: SessionCatalogMetadata,
    sessions: Vec<SessionRecord>,
}

pub(super) async fn session_catalog(
    checkpoints: &Arc<dyn CheckpointStore>,
    activities: &SessionActivities,
) -> Result<Vec<SessionRecord>> {
    let mut catalog = activities.lock().await;
    refresh_catalog(checkpoints, &mut catalog).await
}

pub(super) async fn activity_catalog(
    checkpoints: &Arc<dyn CheckpointStore>,
    activities: &SessionActivities,
) -> Result<(Vec<SessionRecord>, Vec<Arc<BackgroundApproval>>)> {
    let mut catalog = activities.lock().await;
    let sessions = match &catalog.snapshot {
        Some(snapshot) => snapshot.sessions.clone(), // rules-allow: the catalog snapshot keeps the records it returns
        None => refresh_catalog(checkpoints, &mut catalog).await?,
    };
    Ok((
        sessions,
        catalog.approvals.values().map(Arc::clone).collect(),
    ))
}

pub(super) async fn gateway_catalog(
    checkpoints: &Arc<dyn CheckpointStore>,
    activities: &SessionActivities,
) -> Result<(Vec<SessionRecord>, Vec<Arc<BackgroundApproval>>)> {
    let mut catalog = activities.lock().await;
    let sessions = refresh_catalog(checkpoints, &mut catalog).await?;
    Ok((
        sessions,
        catalog.approvals.values().map(Arc::clone).collect(),
    ))
}

async fn refresh_catalog(
    checkpoints: &Arc<dyn CheckpointStore>,
    catalog: &mut SessionCatalog,
) -> Result<Vec<SessionRecord>> {
    let metadata = load_session_metadata(checkpoints).await?;
    let sessions = filtered_session_catalog(
        checkpoints,
        &catalog.activities,
        &metadata,
        CatalogFilter::Visible,
    )
    .await?;
    catalog.snapshot = Some(CatalogSnapshot {
        metadata,
        sessions: sessions.clone(), // rules-allow: the catalog snapshot keeps the records it returns
    });
    Ok(sessions)
}

pub(super) async fn hidden_bot_session_catalog(
    checkpoints: &Arc<dyn CheckpointStore>,
    activities: &SessionActivities,
    bot_id: &str,
) -> Result<Vec<SessionRecord>> {
    let catalog = activities.lock().await;
    let metadata = load_session_metadata(checkpoints).await?;
    filtered_session_catalog(
        checkpoints,
        &catalog.activities,
        &metadata,
        CatalogFilter::HiddenBot(bot_id),
    )
    .await
}

pub(super) async fn restore_pending_approval_activities(
    checkpoints: &Arc<dyn CheckpointStore>,
    activities: &SessionActivities,
) -> Result<()> {
    let mut catalog = activities.lock().await;
    let mut cursor = None;
    loop {
        let page = checkpoints
            .list_sessions_page(SessionPageRequest {
                owner_id: None,
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
            let activity = SessionActivity {
                state: SessionActivityState::AwaitingApproval,
                turn_id: Some(approval.turn_id),
                approval_request_id: Some(approval.request_id),
                started_at: checkpoint
                    .active_execution
                    .map(|execution| execution.started_at_ms.div_euclid(1_000)),
                ..SessionActivity::default()
            };
            catalog.update(summary, activity)?;
        }
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }
    drop(catalog);
    Ok(())
}

pub(super) async fn update_session_activity(
    checkpoints: &Arc<dyn CheckpointStore>,
    activities: &SessionActivities,
    session_id: &str,
    activity: SessionActivity,
) -> Result<()> {
    let mut catalog = activities.lock().await;
    let summary = checkpoints
        .session_summary(session_id)
        .await?
        .ok_or_else(|| Error::Config("the running session has no catalog entry".into()))?;
    catalog.update(summary, activity)
}

impl SessionCatalog {
    fn update(&mut self, summary: SessionSummary, activity: SessionActivity) -> Result<()> {
        let approval = if !summary.catalog_visible
            && summary.parent_session_id.is_none()
            && activity.state == SessionActivityState::AwaitingApproval
        {
            Some(Arc::new(BackgroundApproval {
                session_id: Arc::clone(&summary.session_id),
                bot_id: Arc::clone(&summary.session_context.owner_id),
                turn_id: activity
                    .turn_id
                    .as_ref()
                    .map(Arc::clone)
                    .ok_or_else(|| Error::Config("approval activity has no turn id".into()))?,
                request_id: activity
                    .approval_request_id
                    .as_ref()
                    .map(Arc::clone)
                    .ok_or_else(|| Error::Config("approval activity has no request id".into()))?,
            }))
        } else {
            None
        };
        self.approvals.remove(&summary.session_id);
        if let Some(approval) = approval {
            self.approvals
                .insert(Arc::clone(&summary.session_id), approval);
        }
        self.activities
            .insert(Arc::clone(&summary.session_id), activity.clone()); // rules-allow: the activity map and the session record each own the activity
        if let Some(snapshot) = &mut self.snapshot {
            snapshot
                .sessions
                .retain(|record| record.session_id != summary.session_id);
            let metadata = snapshot.metadata.get(&*summary.session_id);
            if summary.catalog_visible {
                snapshot
                    .sessions
                    .push(session_record(summary, metadata, activity));
                snapshot.sessions.sort_by(|left, right| {
                    right
                        .updated_at
                        .cmp(&left.updated_at)
                        .then_with(|| right.sequence.cmp(&left.sequence))
                        .then_with(|| right.session_id.cmp(&left.session_id))
                });
                retain_catalog_page(&mut snapshot.sessions);
                sort_sessions(&mut snapshot.sessions);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum CatalogFilter<'a> {
    Visible,
    HiddenBot(&'a str),
}

async fn filtered_session_catalog(
    checkpoints: &Arc<dyn CheckpointStore>,
    activities: &HashMap<Arc<str>, SessionActivity>,
    metadata: &SessionCatalogMetadata,
    filter: CatalogFilter<'_>,
) -> Result<Vec<SessionRecord>> {
    let mut cursor = None;
    let mut sessions = Vec::new();
    loop {
        let page = checkpoints
            .list_sessions_page(SessionPageRequest {
                owner_id: match filter {
                    CatalogFilter::Visible => None,
                    CatalogFilter::HiddenBot(id) => Some(id.into()),
                },
                cursor,
                limit: SESSION_PAGE_SIZE,
            })
            .await?;
        let ordinary_count = sessions
            .iter()
            .filter(|session: &&SessionSummary| {
                *session.session_id
                    != *crate::bots::conversation_session_id(&session.session_context.owner_id)
            })
            .count();
        let mut available = SESSION_PAGE_SIZE.saturating_sub(ordinary_count);
        sessions.extend(page.sessions.into_iter().filter(|session| match filter {
            CatalogFilter::Visible => {
                let persistent = *session.session_id
                    == *crate::bots::conversation_session_id(&session.session_context.owner_id);
                if session.catalog_visible && (persistent || available > 0) {
                    if !persistent {
                        available -= 1;
                    }
                    true
                } else {
                    false
                }
            }
            CatalogFilter::HiddenBot(bot_id) => {
                *session.session_context.owner_id == *bot_id
                    && session.parent_session_id.is_none()
                    && !session.catalog_visible
            }
        }));
        if matches!(filter, CatalogFilter::HiddenBot(_)) && sessions.len() >= SESSION_PAGE_SIZE {
            break;
        }
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }
    if matches!(filter, CatalogFilter::HiddenBot(_)) {
        sessions.truncate(SESSION_PAGE_SIZE);
    }
    let mut sessions = sessions
        .into_iter()
        .map(|summary| {
            let metadata = metadata.get(&*summary.session_id);
            let activity = activities
                .get(&summary.session_id)
                .cloned() // rules-allow: the activity map keeps its entry
                .unwrap_or_default();
            session_record(summary, metadata, activity)
        })
        .collect::<Vec<_>>();
    sort_sessions(&mut sessions);
    Ok(sessions)
}

fn session_record(
    summary: SessionSummary,
    metadata: Option<&SessionMetadata>,
    activity: SessionActivity,
) -> SessionRecord {
    let mut preview = summary.first_user_message;
    if let Some(message) = &mut preview {
        message.truncate(message.floor_char_boundary(MAX_SESSION_PREVIEW_BYTES.min(message.len())));
    }
    SessionRecord {
        session_id: summary.session_id,
        session_context: summary.session_context,
        parent_session_id: summary.parent_session_id,
        parent_sequence: summary.parent_sequence,
        sequence: summary.sequence,
        first_user_message: preview,
        execution_stats: summary.execution_stats,
        title: metadata.and_then(|metadata| metadata.title.clone()), // rules-allow: the catalog metadata keeps its title
        pinned: metadata.is_some_and(|metadata| metadata.pinned),
        activity,
        created_at: summary.created_at,
        updated_at: summary.updated_at,
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
        return Err(Rejection::new(
            "invalid_session_title",
            format!("chat title must be 1–{MAX_SESSION_TITLE_BYTES} UTF-8 bytes"),
        ));
    }
    Ok(title)
}

fn retain_catalog_page(sessions: &mut Vec<SessionRecord>) {
    let mut ordinary = 0;
    sessions.retain(|session| {
        if *session.session_id
            == *crate::bots::conversation_session_id(&session.session_context.owner_id)
        {
            return true;
        }
        ordinary += 1;
        ordinary <= SESSION_PAGE_SIZE
    });
}

#[cfg(test)]
mod tests {
    use mobius::backend::checkpoint::{Checkpoint, sqlite::SqliteCheckpoint};

    use super::*;

    fn activities() -> SessionActivities {
        Arc::new(tokio::sync::Mutex::new(SessionCatalog::default()))
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
            Arc::make_mut(&mut checkpoint.session_context).owner_id = "bot-fixture".into();
            Arc::make_mut(&mut checkpoint.session_context).workspace_id = Some("workspace".into());
            checkpoint.sequence = 1;
            checkpoint.first_user_message = Some(if index == SESSION_PAGE_SIZE {
                "€".repeat(MAX_SESSION_PREVIEW_BYTES / '€'.len_utf8() + 1)
            } else {
                format!("chat {index}")
            });
            checkpoints
                .save(checkpoint.clone(), Vec::new(), None)
                .await
                .expect("save chat");
        }

        let sessions = session_catalog(&checkpoints, &activities())
            .await
            .expect("session catalog");
        let preview = sessions
            .iter()
            .find(|session| &*session.session_id == "100")
            .and_then(|session| session.first_user_message.as_deref())
            .expect("UTF-8 preview");

        assert_eq!(sessions.len(), SESSION_PAGE_SIZE);
        assert!(sessions.iter().all(|session| &*session.session_id != "000"));
        assert_eq!(
            preview,
            "€".repeat(MAX_SESSION_PREVIEW_BYTES / '€'.len_utf8())
        );
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
        Arc::make_mut(&mut root.session_context).owner_id = "bot-a".into();
        root.sequence = 1;
        checkpoints
            .save(root.clone(), Vec::new(), None)
            .await
            .expect("save root");
        let mut child = Checkpoint::empty("hidden-child");
        child.catalog_visible = false;
        Arc::make_mut(&mut child.session_context).owner_id = "bot-a".into();
        checkpoints
            .fork("hidden-root", 1, child.clone())
            .await
            .expect("fork child");
        let mut other = Checkpoint::empty("other-root");
        other.catalog_visible = false;
        Arc::make_mut(&mut other.session_context).owner_id = "bot-b".into();
        checkpoints
            .save(other.clone(), Vec::new(), None)
            .await
            .expect("save other");

        let sessions = hidden_bot_session_catalog(&checkpoints, &activities(), "bot-a")
            .await
            .expect("hidden Bot sessions");

        assert_eq!(
            sessions
                .iter()
                .map(|session| session.session_id.as_ref())
                .collect::<Vec<_>>(),
            vec!["hidden-root"]
        );
    }
}
