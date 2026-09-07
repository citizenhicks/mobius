use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use super::AgentRecord;
use super::AgentStatus;
use super::OnPersistFailure;
use super::Shared;
use super::Stage;
use super::ensure_concurrency_available;
use super::unknown_target;
use crate::Error;
use crate::Result;
use crate::agent::AgentSender;
use crate::protocol::{MessageSubmission, Op};
use serde::Deserialize;
use serde::Serialize;
use tokio::time::Instant;
use tokio::time::timeout_at;

#[derive(Clone, Serialize, Deserialize)]
pub(in crate::middleware::subagents) struct CompletionUpdate {
    pub(super) id: String,
    pub(super) recipient: String,
    pub(super) agent: String,
    pub(super) status: String,
    pub(super) text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) reported_message_ids: Vec<String>,
}

impl CompletionUpdate {
    pub(in crate::middleware::subagents) fn internal_kind(&self) -> String {
        format!("subagent_update:{}", self.id)
    }

    pub(in crate::middleware::subagents) fn render(
        &self,
        delivered_message_ids: &BTreeSet<String>,
    ) -> String {
        let text = self.text.as_deref().filter(|_| {
            self.reported_message_ids
                .iter()
                .all(|id| !delivered_message_ids.contains(id))
        });
        format!(
            "<subagent_update agent=\"{}\" status=\"{}\">\n{}\n</subagent_update>",
            self.agent,
            self.status,
            text.unwrap_or_default()
        )
    }
}

pub(in crate::middleware::subagents) struct Wake {
    pub(in crate::middleware::subagents) record: AgentRecord,
    pub(in crate::middleware::subagents) sender: Option<AgentSender>,
    pub(in crate::middleware::subagents) previous: AgentStatus,
}

impl Shared {
    pub(in crate::middleware::subagents) async fn receive_updates(
        &self,
        root_id: &str,
        recipient: &str,
        acknowledged: &BTreeSet<String>,
    ) -> Result<Vec<CompletionUpdate>> {
        if !acknowledged.is_empty() {
            let root = self.root(root_id).await?;
            let has_acknowledged = root.state.lock().await.tree.updates.iter().any(|update| {
                update.recipient == recipient && acknowledged.contains(update.id.as_str())
            });
            if has_acknowledged {
                self.mutate_root(root_id, |root| {
                    root.tree.updates.retain(|update| {
                        update.recipient != recipient || !acknowledged.contains(update.id.as_str())
                    });
                    Ok(())
                })
                .await?;
            }
        }
        let root = self.root(root_id).await?;
        Ok(root
            .state
            .lock()
            .await
            .tree
            .updates
            .iter()
            .filter(|update| update.recipient == recipient)
            .cloned()
            .collect())
    }

    pub(in crate::middleware::subagents) async fn send_message(
        &self,
        root_id: &str,
        from: &str,
        target: &str,
        message: MessageSubmission,
    ) -> Result<Option<Wake>> {
        if from == target {
            return Err(Error::Tool("an agent cannot message itself".into()));
        }
        let root_slot = self.root(root_id).await?;
        let _writer = root_slot.writer.lock().await;
        if !self
            .roots
            .lock()
            .await
            .get(root_id)
            .is_some_and(|current| Arc::ptr_eq(current, &root_slot))
        {
            return Err(Error::Unknown(format!("agent tree `{root_id}`")));
        }
        if target == "/root" {
            let mut root = root_slot.state.lock().await;
            let sender = root
                .root_sender
                .as_ref()
                .and_then(|sender| sender.upgrade())
                .ok_or_else(|| Error::Stopped("agent `/root` is not running".into()))?;
            let reports_to_parent = root
                .tree
                .agents
                .get(from)
                .is_some_and(|entry| entry.parent == target);
            if reports_to_parent {
                sender.submit(Op::Message {
                    message: message.clone(),
                })?;
                root.parent_reports
                    .entry(from.into())
                    .or_default()
                    .push(message);
            } else {
                sender.submit(Op::Message { message })?;
            }
            return Ok(None);
        }
        let mut root = root_slot.state.lock().await;
        let status = root
            .tree
            .agents
            .get(target)
            .ok_or_else(|| unknown_target(target))?
            .status
            .clone();
        match &status {
            AgentStatus::PendingInit => {
                Err(Error::Busy(format!("agent `{target}` is initializing")))
            }
            AgentStatus::Running => {
                let reports_to_parent = root
                    .tree
                    .agents
                    .get(from)
                    .is_some_and(|entry| entry.parent == target);
                let sender = root
                    .senders
                    .get(target)
                    .cloned()
                    .ok_or_else(|| Error::Stopped("agent runtime is unavailable".into()))?;
                if reports_to_parent {
                    sender.submit(Op::Message {
                        message: message.clone(),
                    })?;
                    root.parent_reports
                        .entry(from.into())
                        .or_default()
                        .push(message);
                } else {
                    sender.submit(Op::Message { message })?;
                }
                Ok(None)
            }
            AgentStatus::Errored => Err(Error::Stopped(format!(
                "agent `{target}` is {}",
                status.label()
            ))),
            AgentStatus::Interrupted | AgentStatus::Completed => {
                drop(root);
                let max_concurrency = self.max_concurrency;
                self.commit_locked_root(
                    root_id,
                    &root_slot,
                    |root| {
                        ensure_concurrency_available(&root.tree, max_concurrency)?;
                        let entry = root
                            .tree
                            .agents
                            .get_mut(target)
                            .ok_or_else(|| unknown_target(target))?;
                        let record = entry.clone();
                        entry.status = AgentStatus::PendingInit;
                        entry.last_message = None;
                        let sender = root.senders.get(target).cloned();
                        Ok(Stage::Changed(Some(Wake {
                            record,
                            sender,
                            previous: status,
                        })))
                    },
                    OnPersistFailure::Abort,
                )
                .await
                .map(Stage::into_output)
            }
        }
    }

    pub(in crate::middleware::subagents) async fn wait(
        &self,
        root_id: &str,
        recipient: &str,
        duration: Duration,
    ) -> Result<Vec<String>> {
        let deadline = Instant::now() + duration;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (sources, active) = self.pending_sources(root_id, recipient).await?;
            if !sources.is_empty() {
                return Ok(sources);
            }
            if !active {
                return Ok(Vec::new());
            }
            if timeout_at(deadline, notified).await.is_err() {
                return Ok(Vec::new());
            }
        }
    }

    async fn pending_sources(&self, root_id: &str, recipient: &str) -> Result<(Vec<String>, bool)> {
        let root = self.root(root_id).await?;
        let root = root.state.lock().await;
        let sources = root
            .tree
            .updates
            .iter()
            .filter(|update| update.recipient == recipient)
            .map(|update| update.agent.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let active = root
            .tree
            .agents
            .iter()
            .any(|(path, agent)| path != recipient && agent.status.is_active());
        Ok((sources, active))
    }
}
