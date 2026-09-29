//! Owner-scoped peer messages between a Bot's open, catalog-visible chats.

use std::sync::{Arc, Weak};

use mobius::BoxFuture;
use mobius::agent::validate_submission;
use mobius::middleware::sessions::{LiveChat, LiveChats};
use mobius::protocol::{MessageAuthor, MessageSubmission, Op, Submission};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::catalog::load_session_metadata;
use super::{GatewayState, HostHandle};
use crate::wire::SessionActivityState;

/// Reads resident chats without keeping the gateway registry alive.
pub(super) struct GatewayLiveChats(pub(super) Weak<Mutex<GatewayState>>);

struct OpenChat {
    chat: LiveChat,
    host: HostHandle,
}

struct OpenChats {
    sender: LiveChat,
    bot_id: String,
    chats: Vec<OpenChat>,
}

impl GatewayLiveChats {
    /// Returns the sender and its owner's other open chats. The registry lock is
    /// released before any chat or store is awaited.
    async fn open(&self, session_id: &str) -> mobius::Result<OpenChats> {
        let state = self
            .0
            .upgrade()
            .ok_or_else(|| mobius::Error::Stopped("the gateway host stopped".into()))?;
        let (hosts, checkpoints, activities) = {
            let state = state.lock().await;
            (
                state
                    .sessions
                    .values()
                    .filter(|host| host.is_alive() && host.session_id() != session_id)
                    .cloned()
                    .collect::<Vec<_>>(),
                Arc::clone(&state.checkpoints),
                Arc::clone(&state.activities),
            )
        };
        let sender = checkpoints
            .session_summary(session_id)
            .await?
            .filter(|summary| summary.catalog_visible)
            .ok_or_else(|| {
                mobius::Error::Tool("only listed chats can message other chats".into())
            })?;
        let mut metadata = load_session_metadata(&checkpoints)
            .await
            .map_err(|error| mobius::Error::Checkpoint(error.to_string()))?;
        let mut siblings = Vec::with_capacity(hosts.len());
        for host in hosts {
            let Some(summary) = checkpoints.session_summary(host.session_id()).await? else {
                continue;
            };
            let entry = metadata.remove(&summary.session_id);
            if summary.catalog_visible
                && summary.session_context.owner_id == sender.session_context.owner_id
                && !entry.as_ref().is_some_and(|entry| entry.hidden)
            {
                siblings.push((summary, entry.and_then(|entry| entry.title), host));
            }
        }
        let catalog = activities.lock().await;
        let chats = siblings
            .into_iter()
            .map(|(summary, title, host)| OpenChat {
                chat: LiveChat {
                    running: catalog
                        .activities
                        .get(&summary.session_id)
                        .is_some_and(|activity| activity.state != SessionActivityState::Idle),
                    title,
                    workspace: summary.session_context.workspace_label,
                    session_id: summary.session_id,
                },
                host,
            })
            .collect();
        Ok(OpenChats {
            sender: LiveChat {
                title: metadata.remove(session_id).and_then(|entry| entry.title),
                workspace: sender.session_context.workspace_label,
                session_id: sender.session_id,
                running: true,
            },
            bot_id: sender.session_context.owner_id,
            chats,
        })
    }
}

impl LiveChats for GatewayLiveChats {
    fn list<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, mobius::Result<Vec<LiveChat>>> {
        Box::pin(async move {
            let open = self.open(session_id).await?;
            Ok(open.chats.into_iter().map(|open| open.chat).collect())
        })
    }

    fn send<'a>(
        &'a self,
        session_id: &'a str,
        target: &'a str,
        text: String,
    ) -> BoxFuture<'a, mobius::Result<()>> {
        Box::pin(async move {
            let OpenChats {
                sender,
                bot_id,
                chats,
            } = self.open(session_id).await?;
            let mut matches = chats.into_iter().filter(|open| open.chat.is_target(target));
            let host = match (matches.next(), matches.next()) {
                (Some(open), None) => open.host,
                (Some(_), Some(_)) => {
                    return Err(mobius::Error::Tool(
                        "target names more than one open chat; use its full session ID".into(),
                    ));
                }
                (None, _) => {
                    return Err(mobius::Error::Tool(
                        "target is not another open chat of this Bot; call list_chats".into(),
                    ));
                }
            };
            let id = Uuid::new_v4();
            let handle = sender.handle();
            let submission = Submission {
                id: id.to_string(),
                op: Op::Message {
                    message: MessageSubmission {
                        author: MessageAuthor::Peer {
                            message_id: id.to_string(),
                            session_id: sender.session_id,
                            handle,
                            symbol: None,
                        },
                        text,
                        attachments: Vec::new(),
                        reply: None,
                        requested_delivery: None,
                        target_turn_id: None,
                    },
                },
            };
            validate_submission(&submission)?;
            // The recipient re-checks ownership in its own command order.
            host.deliver_peer(submission, bot_id)
                .await
                .map_err(|rejection| mobius::Error::Tool(rejection.message))
        })
    }
}
