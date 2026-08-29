use std::sync::Arc;

use uuid::Uuid;

use super::AgentStatus;
use super::MAX_MESSAGE_BYTES;
use super::MAX_PENDING_UPDATES;
use super::OnPersistFailure;
use super::Shared;
use super::Stage;
use super::coordination::CompletionUpdate;
use super::subagent_error_notice;
use crate::Error;
use crate::Result;
use crate::agent::AgentEvents;
use crate::protocol::EventMsg;
use crate::protocol::Op;
use crate::protocol::ReviewDecision;
use crate::truncate_utf8;

impl Shared {
    async fn turn_started(&self, root_id: &str, path: &str, turn_id: String) -> Result<()> {
        self.mutate_root(root_id, |root| {
            let entry = root
                .tree
                .agents
                .get_mut(path)
                .ok_or_else(|| Error::Unknown(format!("agent `{path}`")))?;
            entry.status = AgentStatus::Running;
            entry.active_turn_id = Some(turn_id);
            entry.last_message = None;
            Ok(())
        })
        .await
    }

    async fn approval_pending(&self, root_id: &str, path: &str, turn_id: String) -> Result<()> {
        self.mutate_root(root_id, |root| {
            let entry = root
                .tree
                .agents
                .get_mut(path)
                .ok_or_else(|| Error::Unknown(format!("agent `{path}`")))?;
            entry.status = AgentStatus::Running;
            entry.active_turn_id = Some(turn_id);
            Ok(())
        })
        .await
    }

    async fn message(&self, root_id: &str, path: &str, message: String) -> Result<()> {
        self.mutate_root(root_id, |root| {
            let entry = root
                .tree
                .agents
                .get_mut(path)
                .ok_or_else(|| Error::Unknown(format!("agent `{path}`")))?;
            entry.last_message = Some(bounded(message));
            Ok(())
        })
        .await
    }

    pub(super) async fn finished(
        &self,
        root_id: &str,
        path: &str,
        status: AgentStatus,
        message: Option<String>,
    ) -> Result<()> {
        let message = message.map(bounded);
        let repair_path = path.to_string();
        let stage = self
            .commit_root(
                root_id,
                |root| {
                    let Some(entry) = root.tree.agents.get_mut(path) else {
                        return Ok(Stage::Unchanged(()));
                    };
                    if !entry.status.is_active() {
                        return Ok(Stage::Unchanged(()));
                    }
                    entry.status = status.clone();
                    entry.active_turn_id = None;
                    entry.last_message.clone_from(&message);
                    let parent = entry.parent.clone();
                    root.senders.remove(path);
                    push_finished(root, path, parent, &status, message);
                    Ok(Stage::Changed(()))
                },
                OnPersistFailure::RepairRetry(Box::new(move |candidate, error| {
                    let failure = bounded(format!("subagent state persistence failed: {error}"));
                    if let Some(entry) = candidate.tree.agents.get_mut(repair_path.as_str()) {
                        entry.status = AgentStatus::Errored;
                        entry.last_message = Some(failure.clone());
                    }
                    if let Some(update) = candidate
                        .tree
                        .updates
                        .iter_mut()
                        .rev()
                        .find(|update| update.agent == repair_path)
                    {
                        update.status = AgentStatus::Errored.label().into();
                        update.text = Some(failure.clone());
                    }
                    (
                        format!("{repair_path} state persistence retry failed"),
                        subagent_error_notice(failure),
                    )
                })),
            )
            .await?;
        if matches!(stage, Stage::Changed(())) {
            self.changed.notify_waiters();
        }
        Ok(())
    }

    async fn fail_monitor(
        &self,
        root_id: &str,
        path: &str,
        error: impl std::fmt::Display,
    ) -> Result<()> {
        self.finished(
            root_id,
            path,
            AgentStatus::Errored,
            Some(format!("subagent monitor failed: {error}")),
        )
        .await
    }

    async fn active(&self, root_id: &str, path: &str) -> bool {
        let Ok(root) = self.root(root_id).await else {
            return false;
        };
        root.state
            .lock()
            .await
            .tree
            .agents
            .get(path)
            .is_some_and(|entry| entry.status.is_active())
    }
}

fn push_finished(
    root: &mut super::Root,
    path: &str,
    parent: String,
    status: &AgentStatus,
    message: Option<String>,
) {
    if root.tree.updates.len() >= MAX_PENDING_UPDATES {
        root.tree.updates.pop_front();
    }
    root.tree.updates.push_back(CompletionUpdate {
        id: Uuid::new_v4().to_string(),
        recipient: parent,
        agent: path.into(),
        status: status.label().into(),
        text: message,
    });
}

pub(in crate::middleware::subagents) async fn monitor_agent(
    shared: Arc<Shared>,
    root_id: String,
    path: String,
    mut events: AgentEvents,
) -> Result<()> {
    let mut last_message = None;
    while let Some(event) = events.recv().await {
        let update = match event.msg {
            EventMsg::TurnStarted(turn) => {
                last_message = None;
                shared.turn_started(&root_id, &path, turn.turn_id).await
            }
            EventMsg::AssistantMessage(message) => {
                let message = message
                    .content
                    .iter()
                    .filter(|content| {
                        content.phase == crate::protocol::ModelStepContentPhase::FinalAnswer
                    })
                    .map(|content| content.text.as_str())
                    .collect::<String>();
                if message.is_empty() {
                    Ok(())
                } else {
                    last_message = Some(message.clone());
                    shared.message(&root_id, &path, message).await
                }
            }
            EventMsg::ExecApprovalRequest(request) => {
                async {
                    shared
                        .approval_pending(&root_id, &path, request.turn_id)
                        .await?;
                    let sender = shared.sender(&root_id, &path).await?;
                    sender
                        .submit(Op::ExecApproval {
                            id: request.id,
                            decision: ReviewDecision::Denied {
                                rejection: "headless subagents cannot approve mutations".into(),
                            },
                        })
                        .map(|_| ())
                }
                .await
            }
            EventMsg::TurnComplete(_) => {
                let message = last_message.clone();
                return shared
                    .finished(&root_id, &path, AgentStatus::Completed, message)
                    .await;
            }
            EventMsg::TurnAborted(turn) => {
                return shared
                    .finished(&root_id, &path, AgentStatus::Interrupted, Some(turn.reason))
                    .await;
            }
            EventMsg::Error(error) => {
                return shared
                    .finished(&root_id, &path, AgentStatus::Errored, Some(error.message))
                    .await;
            }
            _ => Ok(()),
        };
        if let Err(error) = update {
            return shared.fail_monitor(&root_id, &path, error).await;
        }
    }
    if shared.active(&root_id, &path).await {
        shared
            .finished(
                &root_id,
                &path,
                AgentStatus::Errored,
                Some("agent disconnected".into()),
            )
            .await?;
    }
    Ok(())
}

fn bounded(mut value: String) -> String {
    if value.len() <= MAX_MESSAGE_BYTES {
        return value;
    }
    value.truncate(truncate_utf8(&value, MAX_MESSAGE_BYTES - '…'.len_utf8()).len());
    value.push('…');
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_messages_include_the_ellipsis_within_the_byte_limit() {
        let message = format!("{}é", "x".repeat(MAX_MESSAGE_BYTES));

        let bounded = bounded(message);

        assert!(bounded.len() <= MAX_MESSAGE_BYTES);
        assert!(bounded.ends_with('…'));
    }
}
