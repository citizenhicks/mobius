use crate::SessionId;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::Notify;

use crate::Error;
use crate::Result;
use crate::agent::{AgentSender, WeakAgentSender};
use crate::backend::checkpoint::CheckpointStore;
use crate::backend::checkpoint::event_turn_page;
use crate::middleware::RuntimeContext;
use crate::protocol::EventMsg;
use crate::protocol::FrontendAction;
use crate::protocol::FrontendActionListItem;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendEvent;
use crate::protocol::FrontendListItemState;
use crate::protocol::FrontendPickerOption;
use crate::protocol::FrontendPreviewEvent;
use crate::protocol::FrontendSlot;
use crate::protocol::FrontendSymbol;
use crate::protocol::FrontendTone;
use crate::protocol::FrontendWidget;
use crate::protocol::FrontendWidgetContent;
use crate::protocol::MessageAuthor;
use crate::protocol::MessageSubmission;
use crate::protocol::Op;

mod coordination;
mod monitor;

pub(super) use coordination::CompletionUpdate;
pub(super) use coordination::{Wake, WakeTarget};
pub(super) use monitor::monitor_agent;

const STATE_KEY: &str = "subagents.v2";
const MAX_PENDING_UPDATES: usize = 256;
pub(super) const MAX_PREVIEW_PAGE_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_MESSAGE_BYTES: usize = 24_000;

pub(super) struct Shared {
    roots: Mutex<BTreeMap<SessionId, Arc<RootSlot>>>,
    changed: Notify,
    max_concurrency: usize,
    max_agents: usize,
}

struct RootSlot {
    state: Mutex<Root>,
    writer: Mutex<()>,
    closing: AtomicBool,
    executions: Mutex<Vec<Weak<Execution>>>,
}

struct Execution {
    finished: AtomicBool,
    changed: Notify,
}

pub(super) struct ExecutionGuard(Arc<Execution>);

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.0.finished.store(true, Ordering::Release);
        self.0.changed.notify_one();
    }
}

#[derive(Clone)]
struct Root {
    checkpoints: Arc<dyn CheckpointStore>,
    frontend: crate::middleware::FrontendEventSink,
    tree: Tree,
    root_sender: Option<WeakAgentSender>,
    senders: BTreeMap<String, AgentSender>,
    parent_reports: BTreeMap<String, Vec<ParentReport>>,
}

/// A child's message to its parent, kept to deduplicate its completion report.
#[derive(Clone)]
struct ParentReport {
    message_id: Arc<str>,
    text: String,
}

impl ParentReport {
    fn of(message: &MessageSubmission) -> Option<Self> {
        match &message.author {
            // The submission is sent on; keep only the two fields deduplication reads.
            MessageAuthor::Source { message_id, .. } => Some(Self {
                message_id: Arc::clone(message_id),
                text: message.text.clone(), // rules-allow: the submission is sent on while the report is kept
            }),
            MessageAuthor::User => None,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tree {
    agents: BTreeMap<String, AgentRecord>,
    updates: VecDeque<CompletionUpdate>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentRecord {
    pub(super) parent: Arc<str>,
    pub(super) session_id: SessionId,
    pub(super) depth: u8,
    pub(super) model: Arc<str>,
    spawn_context: String,
    active_turn_id: Option<Arc<str>>,
    status: AgentStatus,
    last_message: Option<Arc<str>>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AgentStatus {
    PendingInit,
    Running,
    Interrupted,
    Completed,
    Errored,
}

pub(super) struct PreviewPage {
    pub(super) state: FrontendListItemState,
    pub(super) subtitle: String,
    pub(super) page_id: String,
    pub(super) events: Vec<FrontendPreviewEvent>,
    pub(super) next: Option<u64>,
}

pub(super) struct AgentPresentation {
    pub(super) model: Arc<str>,
    pub(super) spawn_context: String,
}

impl AgentStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::PendingInit => "pending_init",
            Self::Running => "running",
            Self::Interrupted => "interrupted",
            Self::Completed => "completed",
            Self::Errored => "errored",
        }
    }

    const fn list_state(self) -> FrontendListItemState {
        match self {
            Self::PendingInit => FrontendListItemState::Pending,
            Self::Running => FrontendListItemState::InProgress,
            Self::Completed => FrontendListItemState::Completed,
            Self::Interrupted => FrontendListItemState::Interrupted,
            Self::Errored => FrontendListItemState::Failed,
        }
    }

    const fn is_active(self) -> bool {
        matches!(self, Self::PendingInit | Self::Running)
    }

    const fn presentation_order(self) -> u8 {
        match self {
            Self::PendingInit | Self::Running => 0,
            Self::Interrupted | Self::Completed => 1,
            Self::Errored => 2,
        }
    }
}

impl Shared {
    pub(super) fn new(max_concurrency: usize, max_agents: usize) -> Self {
        Self {
            roots: Mutex::default(),
            changed: Notify::new(),
            max_concurrency,
            max_agents,
        }
    }

    pub(super) async fn session_start(&self, context: &RuntimeContext) -> Result<()> {
        let identity = super::AgentIdentity::read(&context.session_id, &context.metadata)?;
        let root_id = identity.root_session_id;
        let existing = self.roots.lock().await.get(&root_id).map(Arc::clone);
        if let Some(root) = existing {
            let mut root = root.state.lock().await;
            if identity.depth == 0 {
                root.root_sender = Some(context.sender.clone()); // rules-allow: the root keeps a handle from the borrowed runtime
                root.frontend = Arc::clone(&context.frontend);
                if !root.tree.agents.is_empty() {
                    emit_status(&root)?;
                }
            }
            return Ok(());
        }
        if identity.depth > 0 {
            return Err(Error::Config(
                "a subagent must be resumed through its parent".into(),
            ));
        }
        let mut tree: Tree = context
            .checkpoints
            .load_state(&root_id, STATE_KEY)
            .await?
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        validate_tree(&tree, self.max_agents)?;
        let mut changed = false;
        for entry in tree.agents.values_mut() {
            if entry.status.is_active() {
                entry.status = AgentStatus::Interrupted;
                entry.active_turn_id = None;
                changed = true;
            }
        }
        let root = Root {
            checkpoints: Arc::clone(&context.checkpoints),
            frontend: Arc::clone(&context.frontend),
            tree,
            root_sender: Some(context.sender.clone()), // rules-allow: the root keeps a handle from the borrowed runtime
            senders: BTreeMap::new(),
            parent_reports: BTreeMap::new(),
        };
        if changed {
            persist(&root_id, &root).await?;
        }
        if !root.tree.agents.is_empty() {
            emit_status(&root)?;
        }
        self.roots.lock().await.entry(root_id).or_insert_with(|| {
            Arc::new(RootSlot {
                state: Mutex::new(root),
                writer: Mutex::new(()),
                closing: AtomicBool::new(false),
                executions: Mutex::default(),
            })
        });
        Ok(())
    }

    pub(super) async fn remove_root(&self, root_id: &SessionId) -> Result<()> {
        let root = self.roots.lock().await.get(root_id).map(Arc::clone);
        let Some(root) = root else {
            self.changed.notify_waiters();
            return Ok(());
        };
        root.closing.store(true, Ordering::Release);
        let writer = root.writer.lock().await;
        let result = self
            .commit_locked_root(
                root_id,
                &root,
                |root| {
                    let mut changed = false;
                    for entry in root.tree.agents.values_mut() {
                        if entry.status.is_active() {
                            entry.status = AgentStatus::Interrupted;
                            entry.active_turn_id = None;
                            changed = true;
                        }
                    }
                    Ok(if changed {
                        Stage::Changed(())
                    } else {
                        Stage::Unchanged(())
                    })
                },
                OnPersistFailure::CommitWithStatus,
            )
            .await;
        let mut roots = self.roots.lock().await;
        if roots
            .get(root_id)
            .is_some_and(|current| Arc::ptr_eq(current, &root))
        {
            roots.remove(root_id);
        }
        drop(roots);
        root.state.lock().await.senders.clear();
        drop(writer);
        let executions = std::mem::take(&mut *root.executions.lock().await);
        for execution in executions.iter().filter_map(Weak::upgrade) {
            loop {
                let changed = execution.changed.notified();
                if execution.finished.load(Ordering::Acquire) {
                    break;
                }
                changed.await;
            }
        }
        self.changed.notify_waiters();
        match result {
            Ok(_) | Err(Error::Unknown(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(super) async fn track_execution(&self, root_id: &SessionId) -> Result<ExecutionGuard> {
        let root = self.root(root_id).await?;
        let mut executions = root.executions.lock().await;
        if root.closing.load(Ordering::Acquire) {
            return Err(Error::Config("the parent agent is stopping".into()));
        }
        executions.retain(|execution| {
            execution
                .upgrade()
                .is_some_and(|execution| !execution.finished.load(Ordering::Acquire))
        });
        let execution = Arc::new(Execution {
            finished: AtomicBool::new(false),
            changed: Notify::new(),
        });
        executions.push(Arc::downgrade(&execution));
        drop(executions);
        Ok(ExecutionGuard(execution))
    }

    pub(super) async fn has_active_children(&self, root_id: &SessionId) -> Result<bool> {
        let root = self.root(root_id).await?;
        let root = root.state.lock().await;
        Ok(active_count(&root.tree) > 0)
    }

    pub(super) async fn remove_sender(&self, root_id: &SessionId, path: &str) {
        if let Ok(root) = self.root(root_id).await {
            root.state.lock().await.senders.remove(path);
        }
    }

    pub(super) async fn reserve(
        &self,
        root_id: &SessionId,
        path: &str,
        parent: &str,
        session_id: SessionId,
        depth: u8,
        presentation: AgentPresentation,
    ) -> Result<()> {
        let max_agents = self.max_agents;
        let max_concurrency = self.max_concurrency;
        self.mutate_root(root_id, |root| {
            if root.tree.agents.contains_key(path) {
                return Err(Error::Tool(format!("agent `{path}` already exists")));
            }
            if root.tree.agents.len() >= max_agents - 1 {
                return Err(Error::Stopped(format!(
                    "subagent limit {max_agents} (including root) reached"
                )));
            }
            ensure_concurrency_available(&root.tree, max_concurrency)?;
            root.tree.agents.insert(
                path.into(),
                AgentRecord {
                    parent: parent.into(),
                    session_id,
                    depth,
                    model: presentation.model,
                    spawn_context: presentation.spawn_context,
                    active_turn_id: None,
                    status: AgentStatus::PendingInit,
                    last_message: None,
                },
            );
            Ok(())
        })
        .await
    }

    pub(super) async fn remove(&self, root_id: &SessionId, path: &str) -> Result<()> {
        self.cleanup_root(root_id, |root| {
            root.tree.agents.remove(path);
            root.senders.remove(path);
            root.parent_reports.remove(path);
            Ok(())
        })
        .await
    }

    pub(super) async fn attach(
        &self,
        root_id: &SessionId,
        path: &str,
        sender: AgentSender,
        model: Option<Arc<str>>,
    ) -> Result<()> {
        self.mutate_root(root_id, |root| {
            let entry = root
                .tree
                .agents
                .get_mut(path)
                .ok_or_else(|| Error::Unknown(format!("agent `{path}`")))?;
            if let Some(model) = model {
                entry.model = model;
            }
            entry.status = AgentStatus::Running;
            root.senders.insert(path.into(), sender);
            Ok(())
        })
        .await
    }

    pub(super) async fn rollback(
        &self,
        root_id: &SessionId,
        path: &str,
        status: AgentStatus,
    ) -> Result<()> {
        self.cleanup_root(root_id, |root| {
            if let Some(entry) = root.tree.agents.get_mut(path) {
                entry.status = status;
            }
            root.senders.remove(path);
            root.parent_reports.remove(path);
            Ok(())
        })
        .await
    }

    pub(super) async fn interrupt(&self, root_id: &SessionId, target: &str) -> Result<String> {
        if target == "/root" {
            return Err(Error::Tool("the root agent cannot interrupt itself".into()));
        }
        let root = self.root(root_id).await?;
        let root = root.state.lock().await;
        let entry = root
            .tree
            .agents
            .get(target)
            .ok_or_else(|| unknown_target(target))?;
        if let Some(sender) = root.senders.get(target) {
            let turn_id = entry.active_turn_id.as_ref().ok_or_else(|| {
                Error::Tool(format!("agent `{target}` has no active turn to interrupt"))
            })?;
            sender.submit(Op::Interrupt {
                turn_id: Arc::clone(turn_id),
            })?;
        }
        let status = entry.status.label();
        drop(root);
        Ok(status.into())
    }

    async fn submit(&self, root_id: &SessionId, path: &str, op: Op) -> Result<()> {
        let root = self.root(root_id).await?;
        let state = root.state.lock().await;
        state
            .senders
            .get(path)
            .ok_or_else(|| Error::Stopped("agent runtime is unavailable".into()))?
            .submit(op)?;
        drop(state);
        Ok(())
    }

    pub(super) async fn list(
        &self,
        root_id: &SessionId,
        prefix: Option<&str>,
    ) -> Result<Vec<Value>> {
        let root = self.root(root_id).await?;
        let root = root.state.lock().await;
        Ok(root
            .tree
            .agents
            .iter()
            .filter(|(path, _)| prefix.is_none_or(|prefix| path.starts_with(prefix)))
            .map(|(path, entry)| {
                serde_json::json!({
                    "task_name": path,
                    "status": entry.status.label(),
                    "model": entry.model,
                    "last_message": entry.last_message.as_deref()
                })
            })
            .collect())
    }

    pub(super) async fn resume_options(
        &self,
        root_id: &SessionId,
    ) -> Result<Vec<FrontendPickerOption>> {
        let root = self.root(root_id).await?;
        let root = root.state.lock().await;
        Ok(picker_options(&root.tree))
    }

    pub(super) async fn preview(
        &self,
        root_id: &SessionId,
        path: &str,
        before_sequence: Option<u64>,
    ) -> Result<PreviewPage> {
        let (checkpoints, session_id, state, subtitle, terminal_error) = {
            let root = self.root(root_id).await?;
            let root = root.state.lock().await;
            let entry = root
                .tree
                .agents
                .get(path)
                .ok_or_else(|| unknown_target(path))?;
            (
                Arc::clone(&root.checkpoints),
                entry.session_id.share(),
                entry.status.list_state(),
                entry.spawn_context.clone(), // rules-allow: the page loads after the state lock is released
                if before_sequence.is_none() && matches!(&entry.status, AgentStatus::Errored) {
                    entry
                        .last_message
                        .as_ref()
                        .filter(|message| !message.is_empty())
                        .map(Arc::clone)
                } else {
                    None
                },
            )
        };
        let page = event_turn_page(checkpoints.as_ref(), &session_id, before_sequence).await?;
        let next = page.next_before_sequence;
        let mut events = page
            .into_chronological()
            .into_iter()
            .map(|record| FrontendPreviewEvent {
                sequence: record.sequence,
                submission_id: record.event.submission_id,
                recorded_at_ms: record.recorded_at_ms,
                event: record.event.msg,
            })
            .collect::<Vec<_>>();
        if let Some(message) = terminal_error {
            let last = events.last();
            events.push(FrontendPreviewEvent {
                sequence: last.map_or(0, |event| event.sequence.saturating_add(1)),
                submission_id: None,
                recorded_at_ms: last.map_or(0, |event| event.recorded_at_ms),
                event: EventMsg::Frontend(subagent_error_notice(message.to_string())),
            });
        }
        if serde_json::to_vec(&events)?.len() > MAX_PREVIEW_PAGE_BYTES {
            return Err(Error::Checkpoint(format!(
                "one subagent turn exceeds the {MAX_PREVIEW_PAGE_BYTES}-byte preview limit"
            )));
        }
        Ok(PreviewPage {
            state,
            subtitle,
            page_id: before_sequence.map_or_else(
                || format!("{path}:latest"),
                |before| format!("{path}:before-sequence-{before}"),
            ),
            events,
            next,
        })
    }

    async fn root(&self, root_id: &SessionId) -> Result<Arc<RootSlot>> {
        self.roots
            .lock()
            .await
            .get(root_id)
            .map(Arc::clone)
            .ok_or_else(|| Error::Unknown(format!("agent tree `{root_id}`")))
    }

    /// Strict mutation: the durable write commits before runtime state changes.
    async fn mutate_root<T>(
        &self,
        root_id: &SessionId,
        mutate: impl FnOnce(&mut Root) -> Result<T>,
    ) -> Result<T> {
        self.commit_root(
            root_id,
            |root| mutate(root).map(Stage::Changed),
            OnPersistFailure::Abort,
        )
        .await
        .map(Stage::into_output)
    }

    /// Best-effort cleanup: runtime state commits even when the durable write fails.
    async fn cleanup_root<T>(
        &self,
        root_id: &SessionId,
        cleanup: impl FnOnce(&mut Root) -> Result<T>,
    ) -> Result<T> {
        self.commit_root(
            root_id,
            |root| cleanup(root).map(Stage::Changed),
            OnPersistFailure::CommitWithStatus,
        )
        .await
        .map(Stage::into_output)
    }

    /// Serializes one root mutation: clone, mutate, persist, then commit in memory.
    /// The writer lock orders mutations; the state lock alone never guards a write.
    async fn commit_root<T>(
        &self,
        root_id: &SessionId,
        mutate: impl FnOnce(&mut Root) -> Result<Stage<T>>,
        on_failure: OnPersistFailure,
    ) -> Result<Stage<T>> {
        let root = self.root(root_id).await?;
        let _writer = root.writer.lock().await;
        self.commit_locked_root(root_id, &root, mutate, on_failure)
            .await
    }

    // The caller holds the writer through this commit and any root eviction.
    async fn commit_locked_root<T>(
        &self,
        root_id: &SessionId,
        root: &Arc<RootSlot>,
        mutate: impl FnOnce(&mut Root) -> Result<Stage<T>>,
        on_failure: OnPersistFailure,
    ) -> Result<Stage<T>> {
        if !self
            .roots
            .lock()
            .await
            .get(root_id)
            .is_some_and(|current| Arc::ptr_eq(current, root))
        {
            return Err(Error::Unknown(format!("agent tree `{root_id}`")));
        }
        // Persistence can fail; stage an independent candidate so readers keep the last committed tree.
        let mut candidate = root.state.lock().await.clone(); // rules-allow: the staged candidate must not alias committed state
        let output = match mutate(&mut candidate)? {
            Stage::Unchanged(output) => return Ok(Stage::Unchanged(output)),
            Stage::Changed(output) => output,
        };
        let error = match persist(root_id, &candidate).await {
            Ok(()) => {
                let (frontend, status) = install(root, candidate).await;
                frontend(status)?;
                return Ok(Stage::Changed(output));
            }
            Err(error) => error,
        };
        match on_failure {
            OnPersistFailure::Abort => Err(error),
            OnPersistFailure::CommitWithStatus => {
                let (frontend, status) = install(root, candidate).await;
                if let Err(delivery) = frontend(status) {
                    return Err(Error::Stopped(format!(
                        "{error}; frontend status delivery failed: {delivery}"
                    )));
                }
                Err(error)
            }
            OnPersistFailure::RepairRetry(repair) => {
                let (retry_message, failure_event) = repair(&mut candidate, &error);
                let retry = persist(root_id, &candidate).await;
                let (frontend, status) = install(root, candidate).await;
                frontend(status)?;
                frontend(failure_event)?;
                if let Err(retry_error) = retry {
                    frontend(subagent_error_notice(format!(
                        "{retry_message}: {retry_error}"
                    )))?;
                }
                Ok(Stage::Changed(output))
            }
        }
    }
}

/// Commits `candidate` as the root's state and returns its sink and status event.
async fn install(
    root: &RootSlot,
    candidate: Root,
) -> (crate::middleware::FrontendEventSink, FrontendEvent) {
    let frontend = Arc::clone(&candidate.frontend);
    let status = status_event(&candidate.tree);
    *root.state.lock().await = candidate;
    (frontend, status)
}

fn unknown_target(target: &str) -> Error {
    Error::Unknown(format!(
        "subagent `{target}` in this chat's task tree; use `list_agents` for canonical task paths"
    ))
}

fn subagent_error_notice(message: String) -> FrontendEvent {
    FrontendEvent::Render {
        capability: super::MANIFEST.id.into(),
        block: FrontendBlock {
            update: crate::protocol::FrontendBlockUpdate::Replace,
            state: crate::protocol::FrontendBlockState::Complete,
            role: crate::protocol::FrontendBlockRole::Notice,
            title: super::text::DEFINITION.error_title.as_str().into(),
            text: message,
            symbol: Some(FrontendSymbol::Agent),
            format: crate::protocol::FrontendBlockFormat::PlainText,
            tone: FrontendTone::Error,
            ..Default::default()
        },
    }
}

/// One staged root mutation handed to `Shared::commit_root`.
enum Stage<T> {
    /// No durable write is needed; return the output without persisting.
    Unchanged(T),
    /// Persist first, then commit runtime state.
    Changed(T),
}

impl<T> Stage<T> {
    fn into_output(self) -> T {
        match self {
            Self::Unchanged(output) | Self::Changed(output) => output,
        }
    }
}

/// Repairs runtime state after a failed durable write; returns the message
/// surfaced when the retry also fails.
type PersistRepair = Box<dyn FnOnce(&mut Root, &Error) -> (String, FrontendEvent) + Send>;

/// How `Shared::commit_root` reacts when the durable write fails.
enum OnPersistFailure {
    /// Leave runtime state untouched and return the error.
    Abort,
    /// Commit runtime state, surface its status widget, and return the error.
    CommitWithStatus,
    /// Repair runtime state, retry the write once, and commit regardless.
    RepairRetry(PersistRepair),
}

fn validate_tree(tree: &Tree, max_agents: usize) -> Result<()> {
    if tree.agents.len() >= max_agents
        || tree.updates.len() > MAX_PENDING_UPDATES
        || tree.agents.values().any(|entry| {
            entry
                .last_message
                .as_ref()
                .is_some_and(|message| message.len() > MAX_MESSAGE_BYTES)
        })
    {
        return Err(Error::Config(
            "subagent checkpoint exceeds safety limits".into(),
        ));
    }
    Ok(())
}

fn active_count(tree: &Tree) -> usize {
    tree.agents
        .values()
        .filter(|entry| entry.status.is_active())
        .count()
}

fn ensure_concurrency_available(tree: &Tree, max_concurrency: usize) -> Result<()> {
    if active_count(tree) >= max_concurrency - 1 {
        return Err(Error::Stopped(format!(
            "subagent concurrency limit {max_concurrency} (including root) reached"
        )));
    }
    Ok(())
}

fn status_widget(tree: &Tree) -> FrontendWidget {
    let active = active_count(tree);
    let failed = tree
        .agents
        .values()
        .any(|agent| matches!(agent.status, AgentStatus::Errored));
    let count = tree.agents.len();
    let label = if count == 1 {
        &super::text::DEFINITION.status_label_one
    } else {
        &super::text::DEFINITION.status_label_other
    };
    FrontendWidget {
        id: "status".into(),
        count: Some(count),
        priority: crate::protocol::FrontendPriority::Secondary,
        slot: FrontendSlot::ComposerFooter,
        text: label.replace("{count}", &count.to_string()),
        tone: if failed {
            FrontendTone::Error
        } else if active > 0 {
            FrontendTone::Success
        } else {
            FrontendTone::Neutral
        },
        symbol: Some(FrontendSymbol::Agent),
        icon_only: false,
        progress: None,
        content: Some(FrontendWidgetContent::ActionList {
            title: super::text::DEFINITION.status_title.as_str().into(),
            items: agent_items(tree),
            actions: Vec::new(),
        }),
        action: None,
    }
}

fn agent_items(tree: &Tree) -> Vec<FrontendActionListItem> {
    let mut agents = tree.agents.iter().collect::<Vec<_>>();
    agents.sort_by_key(|(_, entry)| entry.status.presentation_order());
    agents
        .into_iter()
        .map(|(path, entry)| FrontendActionListItem {
            id: path.as_str().into(),
            text: path.rsplit('/').next().unwrap_or(path).into(),
            state: entry.status.list_state(),
            actions: vec![FrontendAction {
                id: super::Command::Open.as_str().into(),
                label: super::text::DEFINITION.render_open.as_str().into(),
                symbol: if entry.status.is_active() {
                    FrontendSymbol::Progress
                } else {
                    FrontendSymbol::Agent
                },
                // Action tone marks destructive intent; failure is the item's state.
                tone: match entry.status {
                    AgentStatus::Interrupted => FrontendTone::Warning,
                    _ => FrontendTone::Neutral,
                },
                op: Op::command(
                    super::MANIFEST.id,
                    super::Command::Open.as_str(),
                    path.clone(), // rules-allow: the action owns its path from the borrowed tree
                ),
                input_from_label: false,
                editor: None,
            }],
        })
        .collect()
}

fn picker_options(tree: &Tree) -> Vec<FrontendPickerOption> {
    let mut agents = tree.agents.iter().collect::<Vec<_>>();
    agents.sort_by_key(|(_, entry)| entry.status.presentation_order());
    agents
        .into_iter()
        .map(|(path, entry)| FrontendPickerOption {
            label: path.rsplit('/').next().unwrap_or(path).into(),
            description: entry.status.label().into(),
            detail: entry.model.to_string(),
            symbol: Some(if entry.status.is_active() {
                FrontendSymbol::Progress
            } else {
                FrontendSymbol::Agent
            }),
            shows_detail: false,
            op: Op::command(
                super::MANIFEST.id,
                super::Command::Open.as_str(),
                path.clone(), // rules-allow: the option owns its path from the borrowed tree
            ),
        })
        .collect()
}

fn status_event(tree: &Tree) -> FrontendEvent {
    if tree.agents.is_empty() {
        FrontendEvent::RemoveWidget {
            capability: super::MANIFEST.id.into(),
            id: "status".into(),
        }
    } else {
        FrontendEvent::Widget {
            capability: super::MANIFEST.id.into(),
            item: Arc::new(status_widget(tree)),
        }
    }
}

fn emit_status(root: &Root) -> Result<()> {
    (root.frontend)(status_event(&root.tree))
}

async fn persist(root_id: &SessionId, root: &Root) -> Result<()> {
    root.checkpoints
        .save_state(root_id, STATE_KEY, &serde_json::to_value(&root.tree)?)
        .await
}

#[cfg(test)]
mod tests;
