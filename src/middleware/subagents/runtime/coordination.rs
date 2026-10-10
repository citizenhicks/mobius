use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use super::AgentStatus;
use super::OnPersistFailure;
use super::Shared;
use super::Stage;
use super::ensure_concurrency_available;
use super::unknown_target;
use crate::Error;
use crate::Result;
use crate::agent::AgentSender;
use crate::protocol::{MessageSubmission, Submission};
use serde::Deserialize;
use serde::Serialize;
use tokio::time::Instant;
use tokio::time::timeout_at;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::middleware::subagents) struct CompletionUpdate {
    pub(super) id: String,
    pub(super) recipient: String,
    pub(super) agent: String,
    pub(super) status: String,
    pub(super) text: Option<String>,
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
    pub(in crate::middleware::subagents) message: MessageSubmission,
    pub(in crate::middleware::subagents) target: WakeTarget,
    pub(in crate::middleware::subagents) previous: AgentStatus,
}

pub(in crate::middleware::subagents) enum WakeTarget {
    Live(AgentSender),
    Resume {
        session_id: String,
        depth: u8,
        model: String,
    },
}

impl Shared {
    pub(in crate::middleware::subagents) async fn receive_updates(
        &self,
        root_id: &str,
        recipient: &str,
        acknowledged: &BTreeSet<&str>,
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
            let report = reports_to_parent
                .then(|| super::ParentReport::of(&message))
                .flatten();
            let admission = sender.send_with_admission(Submission::message(message))?;
            if let Some(report) = report {
                root.parent_reports
                    .entry(from.into())
                    .or_default()
                    .push(report);
            }
            drop(root);
            drop(_writer);
            admission.wait().await?;
            return Ok(None);
        }
        let mut root = root_slot.state.lock().await;
        let status = root
            .tree
            .agents
            .get(target)
            .ok_or_else(|| unknown_target(target))?
            .status;
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
                    .ok_or_else(|| Error::Stopped("agent runtime is unavailable".into()))?;
                let report = reports_to_parent
                    .then(|| super::ParentReport::of(&message))
                    .flatten();
                let admission = sender.send_with_admission(Submission::message(message))?;
                if let Some(report) = report {
                    root.parent_reports
                        .entry(from.into())
                        .or_default()
                        .push(report);
                }
                drop(root);
                drop(_writer);
                admission.wait().await?;
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
                        let wake_target = match root.senders.get(target) {
                            Some(sender) => WakeTarget::Live(sender.clone()),
                            None => WakeTarget::Resume {
                                session_id: entry.session_id.clone(),
                                depth: entry.depth,
                                model: entry.model.clone(),
                            },
                        };
                        entry.status = AgentStatus::PendingInit;
                        entry.last_message = None;
                        Ok(Stage::Changed(Some(Wake {
                            message,
                            target: wake_target,
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
            .map(|update| &update.agent)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .cloned()
            .collect();
        let active = root
            .tree
            .agents
            .iter()
            .any(|(path, agent)| path != recipient && agent.status.is_active());
        Ok((sources, active))
    }
}
