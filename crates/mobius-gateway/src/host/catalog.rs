use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use mobius::backend::checkpoint::{CheckpointStore, SessionPageRequest, SessionSummary};
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

#[derive(Default)]
pub(super) struct SessionCatalog {
    pub(super) activities: HashMap<String, SessionActivity>,
    pub(super) approvals: BTreeMap<String, BackgroundApproval>,
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
) -> Result<(Vec<SessionRecord>, Vec<BackgroundApproval>)> {
    let mut catalog = activities.lock().await;
    let sessions = match &catalog.snapshot {
        Some(snapshot) => snapshot.sessions.clone(),
        None => refresh_catalog(checkpoints, &mut catalog).await?,
    };
    Ok((sessions, catalog.approvals.values().cloned().collect()))
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
        sessions: sessions.clone(),
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
                bot_id: None,
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
            Some(BackgroundApproval {
                session_id: summary.session_id.clone(),
                bot_id: summary.session_context.bot_id.clone(),
                turn_id: activity
                    .turn_id
                    .clone()
                    .ok_or_else(|| Error::Config("approval activity has no turn id".into()))?,
                request_id: activity
                    .approval_request_id
                    .clone()
                    .ok_or_else(|| Error::Config("approval activity has no request id".into()))?,
            })
        } else {
            None
        };
        self.approvals.remove(&summary.session_id);
        if let Some(approval) = approval {
            self.approvals.insert(summary.session_id.clone(), approval);
        }
        self.activities
            .insert(summary.session_id.clone(), activity.clone());
        if let Some(snapshot) = &mut self.snapshot {
            snapshot
                .sessions
                .retain(|record| record.session_id != summary.session_id);
            let metadata = snapshot.metadata.get(&summary.session_id);
            if summary.catalog_visible && !metadata.is_some_and(|item| item.hidden) {
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
                snapshot.sessions.truncate(SESSION_PAGE_SIZE);
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
    activities: &HashMap<String, SessionActivity>,
    metadata: &SessionCatalogMetadata,
    filter: CatalogFilter<'_>,
) -> Result<Vec<SessionRecord>> {
    let mut cursor = None;
    let mut sessions = Vec::new();
    while sessions.len() < SESSION_PAGE_SIZE {
        let page = checkpoints
            .list_sessions_page(SessionPageRequest {
                bot_id: match filter {
                    CatalogFilter::Visible => None,
                    CatalogFilter::HiddenBot(id) => Some(id.into()),
                },
                cursor,
                limit: SESSION_PAGE_SIZE,
            })
            .await?;
        sessions.extend(page.sessions.into_iter().filter(|session| {
            match filter {
                CatalogFilter::Visible => {
                    session.catalog_visible
                        && !metadata
                            .get(&session.session_id)
                            .is_some_and(|item| item.hidden)
                }
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
    let mut sessions = sessions
        .into_iter()
        .map(|summary| {
            let metadata = metadata.get(&summary.session_id);
            let activity = activities
                .get(&summary.session_id)
                .cloned()
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
        title: metadata.and_then(|metadata| metadata.title.clone()),
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

    use mobius::backend::checkpoint::{
        EventPage, EventPageRequest, ExecutionRecord, JournalEvent, SessionPage, TimestampedEvent,
    };
    use mobius::{BoxFuture, protocol::Event};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountedStore {
        inner: Arc<dyn CheckpointStore>,
        pages: std::sync::Mutex<Vec<Option<String>>>,
        rows: AtomicUsize,
        summaries: AtomicUsize,
        metadata_reads: AtomicUsize,
    }

    impl CountedStore {
        fn new(inner: Arc<dyn CheckpointStore>) -> Self {
            Self {
                inner,
                pages: Default::default(),
                rows: AtomicUsize::new(0),
                summaries: AtomicUsize::new(0),
                metadata_reads: AtomicUsize::new(0),
            }
        }
        fn reset(&self) {
            self.pages.lock().unwrap().clear();
            self.rows.store(0, Ordering::Relaxed);
            self.summaries.store(0, Ordering::Relaxed);
            self.metadata_reads.store(0, Ordering::Relaxed);
        }
    }

    impl CheckpointStore for CountedStore {
        fn load<'a>(&'a self, id: &'a str) -> BoxFuture<'a, mobius::Result<Option<Checkpoint>>> {
            self.inner.load(id)
        }
        fn delete_sessions<'a>(&'a self, ids: &'a [String]) -> BoxFuture<'a, mobius::Result<bool>> {
            self.inner.delete_sessions(ids)
        }
        fn save<'a>(
            &'a self,
            checkpoint: &'a Checkpoint,
            delta: &'a [Value],
            execution: Option<&'a ExecutionRecord>,
        ) -> BoxFuture<'a, mobius::Result<()>> {
            self.inner.save(checkpoint, delta, execution)
        }
        fn save_with_events<'a>(
            &'a self,
            checkpoint: Checkpoint,
            delta: Vec<Value>,
            execution: Option<ExecutionRecord>,
            events: Vec<TimestampedEvent>,
        ) -> BoxFuture<'a, mobius::Result<Vec<JournalEvent>>> {
            self.inner
                .save_with_events(checkpoint, delta, execution, events)
        }
        fn append_event<'a>(
            &'a self,
            id: &'a str,
            at: i64,
            event: &'a Event,
        ) -> BoxFuture<'a, mobius::Result<JournalEvent>> {
            self.inner.append_event(id, at, event)
        }
        fn event_page<'a>(
            &'a self,
            id: &'a str,
            request: EventPageRequest,
        ) -> BoxFuture<'a, mobius::Result<EventPage>> {
            self.inner.event_page(id, request)
        }
        fn load_state<'a>(
            &'a self,
            scope: &'a str,
            key: &'a str,
        ) -> BoxFuture<'a, mobius::Result<Option<Value>>> {
            if scope == SESSION_CATALOG_SCOPE && key == SESSION_CATALOG_KEY {
                self.metadata_reads.fetch_add(1, Ordering::Relaxed);
            }
            self.inner.load_state(scope, key)
        }
        fn save_state<'a>(
            &'a self,
            scope: &'a str,
            key: &'a str,
            value: &'a Value,
        ) -> BoxFuture<'a, mobius::Result<()>> {
            self.inner.save_state(scope, key, value)
        }
        fn session_summary<'a>(
            &'a self,
            id: &'a str,
        ) -> BoxFuture<'a, mobius::Result<Option<SessionSummary>>> {
            self.summaries.fetch_add(1, Ordering::Relaxed);
            self.inner.session_summary(id)
        }
        fn list_sessions_page(
            &self,
            request: SessionPageRequest,
        ) -> BoxFuture<'_, mobius::Result<SessionPage>> {
            self.pages.lock().unwrap().push(request.bot_id.clone());
            Box::pin(async move {
                let page = self.inner.list_sessions_page(request).await?;
                self.rows.fetch_add(page.sessions.len(), Ordering::Relaxed);
                Ok(page)
            })
        }
    }

    fn activities() -> SessionActivities {
        Arc::new(tokio::sync::Mutex::new(SessionCatalog::default()))
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
        activities.lock().await.activities.insert(
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
        activities.lock().await.activities.insert(
            "background-child".into(),
            SessionActivity {
                state: SessionActivityState::AwaitingApproval,
                turn_id: Some("child-turn".into()),
                approval_request_id: Some("child-request".into()),
                ..SessionActivity::default()
            },
        );
        let approvals = background_approvals(&activities).await;

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

    #[tokio::test]
    async fn hidden_bot_listing_fetches_only_that_bots_rows() {
        let root = tempfile::tempdir().unwrap();
        let counted = Arc::new(CountedStore::new(Arc::new(
            SqliteCheckpoint::new(root.path().join("catalog.sqlite")).unwrap(),
        )));
        let checkpoints: Arc<dyn CheckpointStore> = counted.clone();
        for (id, bot) in std::iter::once(("000-target".to_owned(), "target"))
            .chain((0..101).map(|index| (format!("other-{index:03}"), "other")))
        {
            let mut checkpoint = Checkpoint::empty(id);
            checkpoint.catalog_visible = false;
            checkpoint.session_context.bot_id = bot.into();
            checkpoints.save(&checkpoint, &[], None).await.unwrap();
        }
        let result = hidden_bot_session_catalog(&checkpoints, &activities(), "target")
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(*counted.pages.lock().unwrap(), [Some("target".into())]);
        assert_eq!(counted.rows.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn activity_updates_query_only_the_changed_session() {
        let root = tempfile::tempdir().unwrap();
        let counted = Arc::new(CountedStore::new(Arc::new(
            SqliteCheckpoint::new(root.path().join("catalog.sqlite")).unwrap(),
        )));
        let checkpoints: Arc<dyn CheckpointStore> = counted.clone();
        for id in ["chat", "unrelated", "background"] {
            let mut checkpoint = Checkpoint::empty(id);
            checkpoint.session_context.bot_id = "bot".into();
            checkpoint.catalog_visible = id != "background";
            checkpoints.save(&checkpoint, &[], None).await.unwrap();
        }
        let activities = activities();
        session_catalog(&checkpoints, &activities).await.unwrap();
        counted.reset();
        for (id, state) in [
            ("chat", SessionActivityState::Running),
            ("background", SessionActivityState::AwaitingApproval),
            ("chat", SessionActivityState::Idle),
        ] {
            update_session_activity(
                &checkpoints,
                &activities,
                id,
                SessionActivity {
                    state,
                    turn_id: Some("turn".into()),
                    approval_request_id: Some("approval".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            activity_catalog(&checkpoints, &activities).await.unwrap();
        }
        let (cached, approvals) = activity_catalog(&checkpoints, &activities).await.unwrap();
        assert!(counted.pages.lock().unwrap().is_empty());
        assert_eq!(counted.metadata_reads.load(Ordering::Relaxed), 0);
        assert_eq!(counted.summaries.load(Ordering::Relaxed), 3);
        assert_eq!(approvals.len(), 1);
        assert_eq!(
            cached,
            session_catalog(&checkpoints, &activities).await.unwrap()
        );
    }

    #[tokio::test]
    async fn live_turn_activity_does_not_reload_the_catalog() {
        use crate::{
            bots::BotStore,
            config::{ConfigStore, CredentialStore},
            host::{GatewayHost, tests::create_test_session},
            wire::{ServerMessage, SessionActivityState},
        };
        use mobius::protocol::{MessageSubmission, Op, Submission};
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let (store, config) = ConfigStore::initialize(
            root.path().join("state"),
            "127.0.0.1:8741".parse().unwrap(),
            None,
        )
        .unwrap();
        let mut selection = crate::wire::AgentComposition::default().provider;
        selection.instance = "unconfigured-test".into();
        selection.provider = "openrouter".into();
        selection.model = "openai/gpt-5".into();
        selection.reasoning_effort = None;
        selection.endpoint_auth = crate::wire::ProviderEndpointAuth::ProviderDefault;
        selection.base_url = Some("https://no-credential.invalid/v1".into());
        let config = config
            .registering_provider(
                selection.clone(),
                "Unconfigured test".into(),
                Default::default(),
                vec![selection.model],
                Vec::new(),
            )
            .unwrap();
        let credentials = Arc::new(CredentialStore::open(store.credentials_path()).unwrap());
        let bots = Arc::new(BotStore::open(store.state_dir()).unwrap());
        let gateway = GatewayHost::start(store, config, credentials, bots)
            .await
            .unwrap();
        let counted = {
            let mut state = gateway.state.lock().await;
            let counted = Arc::new(CountedStore::new(Arc::clone(&state.checkpoints)));
            state.checkpoints = counted.clone();
            counted
        };
        let host = create_test_session(&gateway, &workspace).await.unwrap();
        let mut events = gateway.subscribe();
        let mut session_events = host.subscribe();
        crate::host::replay::FRAME_SIZE_MEASUREMENTS.with(|count| count.set(0));
        counted.reset();
        host.submit(Submission {
            id: "catalog-turn".into(),
            op: Op::Message {
                message: MessageSubmission {
                    author: mobius::protocol::MessageAuthor::User,
                    text: "hello".into(),
                    attachments: Vec::new(),
                    reply: None,
                    requested_delivery: None,
                    target_turn_id: None,
                },
            },
        })
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut running = false;
            loop {
                if let ServerMessage::Sessions { sessions, .. } =
                    events.recv().await.unwrap().message
                {
                    let activity = &sessions
                        .iter()
                        .find(|session| session.session_id == host.session_id())
                        .unwrap()
                        .activity;
                    running |= activity.state == SessionActivityState::Running;
                    if running && activity.state == SessionActivityState::Idle {
                        break;
                    }
                }
            }
        })
        .await
        .expect("turn must finish without credentials");
        assert!(counted.pages.lock().unwrap().is_empty());
        assert_eq!(counted.metadata_reads.load(Ordering::Relaxed), 0);
        assert_eq!(counted.summaries.load(Ordering::Relaxed), 2);
        let mut frames = 0;
        while session_events.try_recv().is_ok() {
            frames += 1;
        }
        assert!(frames > 0);
        assert_eq!(
            crate::host::replay::FRAME_SIZE_MEASUREMENTS.with(std::cell::Cell::get),
            frames,
            "each live frame is measured once before publication"
        );
        gateway.shutdown().await;
    }
}
