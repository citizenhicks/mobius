use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;

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
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendEvent;
use crate::protocol::FrontendPickerOption;
use crate::protocol::FrontendPreviewEvent;
use crate::protocol::FrontendSlot;
use crate::protocol::FrontendSymbol;
use crate::protocol::FrontendTone;
use crate::protocol::FrontendWidget;
use crate::protocol::FrontendWidgetContent;
use crate::protocol::MessageSubmission;
use crate::protocol::Op;

mod coordination;
mod monitor;

pub(super) use coordination::CompletionUpdate;
pub(super) use coordination::Followup;
pub(super) use monitor::monitor_agent;

const STATE_KEY: &str = "subagents.v2";
const MAX_PENDING_UPDATES: usize = 256;
pub(super) const MAX_PREVIEW_PAGE_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_MESSAGE_BYTES: usize = 24_000;

pub(super) struct Shared {
    roots: Mutex<BTreeMap<String, Arc<RootSlot>>>,
    changed: Notify,
    max_concurrency: usize,
    max_agents: usize,
}

struct RootSlot {
    state: Mutex<Root>,
    writer: Mutex<()>,
}

#[derive(Clone)]
struct Root {
    checkpoints: Arc<dyn CheckpointStore>,
    frontend: crate::middleware::FrontendEventSink,
    tree: Tree,
    root_sender: Option<WeakAgentSender>,
    senders: BTreeMap<String, AgentSender>,
    parent_reports: BTreeMap<String, Vec<MessageSubmission>>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Tree {
    agents: BTreeMap<String, AgentRecord>,
    updates: VecDeque<CompletionUpdate>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct AgentRecord {
    pub(super) parent: String,
    pub(super) session_id: String,
    pub(super) depth: u8,
    pub(super) model: String,
    spawn_context: String,
    active_turn_id: Option<String>,
    status: AgentStatus,
    last_message: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AgentStatus {
    PendingInit,
    Running,
    Interrupted,
    Completed,
    Errored,
}

pub(super) struct PreviewPage {
    pub(super) subtitle: String,
    pub(super) page_id: String,
    pub(super) events: Vec<FrontendPreviewEvent>,
    pub(super) next: Option<u64>,
}

pub(super) struct AgentPresentation {
    pub(super) model: String,
    pub(super) spawn_context: String,
}

impl AgentStatus {
    fn label(&self) -> &'static str {
        match self {
            Self::PendingInit => "pending_init",
            Self::Running => "running",
            Self::Interrupted => "interrupted",
            Self::Completed => "completed",
            Self::Errored => "errored",
        }
    }

    fn is_active(&self) -> bool {
        matches!(self, Self::PendingInit | Self::Running)
    }
}

impl Shared {
    pub(super) fn new(max_concurrency: usize, max_agents: usize) -> Result<Self> {
        if max_concurrency < 2 {
            return Err(Error::Config(
                "subagent max concurrency must be at least 2 (including root)".into(),
            ));
        }
        if max_agents < max_concurrency {
            return Err(Error::Config(
                "subagent max agents must be at least max concurrency".into(),
            ));
        }
        Ok(Self {
            roots: Mutex::default(),
            changed: Notify::new(),
            max_concurrency,
            max_agents,
        })
    }

    pub(super) async fn session_start(&self, context: RuntimeContext) -> Result<()> {
        let identity = super::AgentIdentity::read(&context.session_id, &context.metadata)?;
        let root_id = identity.root_session_id;
        let existing = self.roots.lock().await.get(&root_id).cloned();
        if let Some(root) = existing {
            let mut root = root.state.lock().await;
            if identity.depth == 0 {
                root.root_sender = Some(context.sender);
                root.frontend = context.frontend;
                if !root.tree.agents.is_empty() {
                    emit_status(&root)?;
                }
            }
            return Ok(());
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
            checkpoints: context.checkpoints,
            frontend: context.frontend,
            tree,
            root_sender: (identity.depth == 0).then_some(context.sender),
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
            })
        });
        Ok(())
    }

    pub(super) async fn remove_root(&self, root_id: &str) {
        self.roots.lock().await.remove(root_id);
        self.changed.notify_waiters();
    }

    pub(super) async fn remove_sender(&self, root_id: &str, path: &str) {
        if let Ok(root) = self.root(root_id).await {
            root.state.lock().await.senders.remove(path);
        }
    }

    pub(super) async fn reserve(
        &self,
        root_id: &str,
        path: &str,
        parent: &str,
        session_id: String,
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

    pub(super) async fn remove(&self, root_id: &str, path: &str) -> Result<()> {
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
        root_id: &str,
        path: &str,
        sender: AgentSender,
        model: Option<String>,
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
        root_id: &str,
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

    pub(super) async fn interrupt(&self, root_id: &str, target: &str) -> Result<String> {
        if target == "/root" {
            return Err(Error::Tool("the root agent cannot interrupt itself".into()));
        }
        let (sender, turn_id, status) = {
            let root = self.root(root_id).await?;
            let root = root.state.lock().await;
            let entry = root
                .tree
                .agents
                .get(target)
                .ok_or_else(|| Error::Unknown(format!("agent `{target}`")))?;
            (
                root.senders.get(target).cloned(),
                entry.active_turn_id.clone(),
                entry.status.label(),
            )
        };
        match (sender, turn_id) {
            (Some(sender), Some(turn_id)) => {
                sender.submit(Op::Interrupt { turn_id })?;
            }
            (Some(_), None) => {
                return Err(Error::Tool(format!(
                    "agent `{target}` has no active turn to interrupt"
                )));
            }
            (None, _) => {}
        }
        Ok(status.into())
    }

    async fn sender(&self, root_id: &str, path: &str) -> Result<AgentSender> {
        self.root(root_id)
            .await?
            .state
            .lock()
            .await
            .senders
            .get(path)
            .cloned()
            .ok_or_else(|| Error::Stopped("agent runtime is unavailable".into()))
    }

    pub(super) async fn list(&self, root_id: &str, prefix: Option<&str>) -> Result<Vec<Value>> {
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

    pub(super) async fn resume_options(&self, root_id: &str) -> Result<Vec<FrontendPickerOption>> {
        let root = self.root(root_id).await?;
        let root = root.state.lock().await;
        Ok(picker_options(&root.tree))
    }

    pub(super) async fn preview(
        &self,
        root_id: &str,
        path: &str,
        before_sequence: Option<u64>,
    ) -> Result<PreviewPage> {
        let (checkpoints, session_id, subtitle, terminal_error) = {
            let root = self.root(root_id).await?;
            let root = root.state.lock().await;
            let entry = root
                .tree
                .agents
                .get(path)
                .ok_or_else(|| Error::Unknown(format!("agent `{path}`")))?;
            (
                Arc::clone(&root.checkpoints),
                entry.session_id.clone(),
                entry.spawn_context.clone(),
                if before_sequence.is_none() && matches!(&entry.status, AgentStatus::Errored) {
                    entry
                        .last_message
                        .clone()
                        .filter(|message| !message.is_empty())
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
                recorded_at_ms: record.recorded_at_ms,
                event: record.event.msg,
            })
            .collect::<Vec<_>>();
        if let Some(message) = terminal_error {
            events.push(FrontendPreviewEvent {
                recorded_at_ms: events.last().map_or(0, |event| event.recorded_at_ms),
                event: EventMsg::Frontend(subagent_error_notice(message)),
            });
        }
        if serde_json::to_vec(&events)?.len() > MAX_PREVIEW_PAGE_BYTES {
            return Err(Error::Checkpoint(format!(
                "one subagent turn exceeds the {MAX_PREVIEW_PAGE_BYTES}-byte preview limit"
            )));
        }
        Ok(PreviewPage {
            subtitle,
            page_id: before_sequence.map_or_else(
                || format!("{path}:latest"),
                |before| format!("{path}:before-sequence-{before}"),
            ),
            events,
            next,
        })
    }

    async fn root(&self, root_id: &str) -> Result<Arc<RootSlot>> {
        self.roots
            .lock()
            .await
            .get(root_id)
            .cloned()
            .ok_or_else(|| Error::Unknown(format!("agent tree `{root_id}`")))
    }

    /// Strict mutation: the durable write commits before runtime state changes.
    async fn mutate_root<T>(
        &self,
        root_id: &str,
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
        root_id: &str,
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
        root_id: &str,
        mutate: impl FnOnce(&mut Root) -> Result<Stage<T>>,
        on_failure: OnPersistFailure,
    ) -> Result<Stage<T>> {
        let root = self.root(root_id).await?;
        let _writer = root.writer.lock().await;
        let (mut candidate, output) = {
            let current = root.state.lock().await;
            let mut candidate = current.clone();
            match mutate(&mut candidate)? {
                Stage::Unchanged(output) => return Ok(Stage::Unchanged(output)),
                Stage::Changed(output) => (candidate, output),
            }
        };
        let error = match persist(root_id, &candidate).await {
            Ok(()) => {
                let frontend = Arc::clone(&candidate.frontend);
                let status = status_event(&candidate.tree);
                *root.state.lock().await = candidate;
                frontend(status)?;
                return Ok(Stage::Changed(output));
            }
            Err(error) => error,
        };
        match on_failure {
            OnPersistFailure::Abort => Err(error),
            OnPersistFailure::CommitWithStatus => {
                let frontend = Arc::clone(&candidate.frontend);
                let status = status_event(&candidate.tree);
                *root.state.lock().await = candidate;
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
                let frontend = Arc::clone(&candidate.frontend);
                let status = status_event(&candidate.tree);
                *root.state.lock().await = candidate;
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

fn subagent_error_notice(message: String) -> FrontendEvent {
    FrontendEvent::Render {
        capability: "subagents".into(),
        block: FrontendBlock {
            id: None,
            group: None,
            update: crate::protocol::FrontendBlockUpdate::Replace,
            state: crate::protocol::FrontendBlockState::Complete,
            role: crate::protocol::FrontendBlockRole::Notice,
            title: "Subagent error".into(),
            text: message,
            symbol: Some(FrontendSymbol::Agent),
            files: Vec::new(),
            format: crate::protocol::FrontendBlockFormat::PlainText,
            tone: FrontendTone::Error,
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
    FrontendWidget {
        id: "status".into(),
        slot: FrontendSlot::ComposerFooter,
        text: tree.agents.len().to_string(),
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
        content: Some(FrontendWidgetContent::Picker {
            title: "Subagents".into(),
            options: picker_options(tree),
        }),
        action: None,
    }
}

fn picker_options(tree: &Tree) -> Vec<FrontendPickerOption> {
    tree.agents
        .iter()
        .map(|(path, entry)| FrontendPickerOption {
            label: path.rsplit('/').next().unwrap_or(path).into(),
            description: entry.status.label().into(),
            detail: entry.model.clone(),
            symbol: Some(FrontendSymbol::Agent),
            shows_detail: false,
            op: Op::CapabilityCommand {
                capability: "subagents".into(),
                command: "subagents".into(),
                arguments: path.clone(),
                input: None,
                target: None,
            },
        })
        .collect()
}

fn status_event(tree: &Tree) -> FrontendEvent {
    if tree.agents.is_empty() {
        FrontendEvent::RemoveWidget {
            capability: "subagents".into(),
            id: "status".into(),
        }
    } else {
        FrontendEvent::Widget {
            capability: "subagents".into(),
            item: status_widget(tree),
        }
    }
}

fn emit_status(root: &Root) -> Result<()> {
    (root.frontend)(status_event(&root.tree))
}

async fn persist(root_id: &str, root: &Root) -> Result<()> {
    root.checkpoints
        .save_state(root_id, STATE_KEY, &serde_json::to_value(&root.tree)?)
        .await
}

#[cfg(test)]
mod tests;
