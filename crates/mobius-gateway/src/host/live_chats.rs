//! Owner-scoped session access through the existing messaging tools.

use mobius::SessionId;
use std::sync::{Arc, Weak};

use mobius::BoxFuture;
use mobius::agent::ValidatedSubmission;
use mobius::middleware::sessions::{ChatTarget, LiveChat, LiveChats, PeerCommand};
use mobius::protocol::{MessageAuthor, MessageSubmission, Op, Submission};
use tokio::sync::Mutex;

use super::catalog::load_session_metadata;
use super::{GatewayState, HostAccess, HostHandle};
use crate::wire::SessionActivityState;

/// Reads resident chats without keeping the gateway registry alive.
pub(super) struct GatewayLiveChats(pub(super) Weak<Mutex<GatewayState>>, pub(super) HostAccess);

struct OpenChat {
    chat: LiveChat,
    host: Option<HostHandle>,
}

struct OpenChats {
    sender: LiveChat,
    bot_id: Arc<str>,
    chats: Vec<OpenChat>,
}

impl GatewayLiveChats {
    /// Returns the sender and its owner's other open chats. The registry lock is
    /// released before any chat or store is awaited.
    async fn open(&self, session_id: &SessionId) -> mobius::Result<OpenChats> {
        let state = self
            .0
            .upgrade()
            .ok_or_else(|| mobius::Error::Stopped("the gateway host stopped".into()))?;
        let (mut hosts, checkpoints, activities) = {
            let state = state.lock().await;
            (
                state
                    .sessions
                    .values()
                    .filter(|host| host.is_alive() && host.session_id() != session_id)
                    .cloned() // rules-allow: the chat handles are used after the state lock is released
                    .map(|host| (host.session_id().share(), host))
                    .collect::<std::collections::HashMap<_, _>>(),
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
        let summaries = if session_id.as_str()
            == crate::bots::conversation_session_id(&sender.session_context.owner_id)
        {
            super::gateway_session_summaries(&checkpoints)
                .await
                .map_err(|error| mobius::Error::Checkpoint(error.to_string()))?
        } else {
            let mut summaries = Vec::with_capacity(hosts.len());
            for id in hosts.keys() {
                summaries.extend(checkpoints.session_summary(id).await?);
            }
            summaries
        };
        let mut siblings = Vec::new();
        for summary in summaries {
            let entry = metadata.remove(&*summary.session_id);
            if &summary.session_id != session_id
                && summary.catalog_visible
                && summary.parent_session_id.is_none()
                && summary.session_context.owner_id == sender.session_context.owner_id
            {
                let host = hosts.remove(&*summary.session_id);
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
                    turn_id: catalog
                        .activities
                        .get(&summary.session_id)
                        .and_then(|activity| activity.turn_id.as_ref().map(Arc::clone)),
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
                turn_id: catalog
                    .activities
                    .get(session_id)
                    .and_then(|activity| activity.turn_id.as_ref().map(Arc::clone)),
            },
            bot_id: sender.session_context.owner_id,
            chats,
        })
    }
}

impl LiveChats for GatewayLiveChats {
    fn list<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> BoxFuture<'a, mobius::Result<Vec<LiveChat>>> {
        Box::pin(async move {
            let open = self.open(session_id).await?;
            Ok(open.chats.into_iter().map(|open| open.chat).collect())
        })
    }

    fn send<'a>(
        &'a self,
        session_id: &'a SessionId,
        command: PeerCommand<'a>,
        initiating_author: &'a MessageAuthor,
        command_id: &'a str,
    ) -> BoxFuture<'a, mobius::Result<SessionId>> {
        Box::pin(async move {
            let OpenChats {
                sender,
                bot_id,
                chats,
            } = self.open(session_id).await?;
            let id: Arc<str> = format!("peer-{session_id}-{command_id}").into();
            if matches!(
                command,
                PeerCommand::Interrupt { .. }
                    | PeerCommand::Message {
                        target: ChatTarget::Workspace(_),
                        ..
                    }
            ) && !matches!(initiating_author, MessageAuthor::User)
            {
                return Err(mobius::Error::Tool(
                    "creating or interrupting a chat requires a user turn".into(),
                ));
            }
            let (target, op) = match command {
                PeerCommand::Message {
                    target,
                    text,
                    delivery,
                } => {
                    let (cause_id, ancestry) = initiating_author.causal_origin();
                    let handle = sender.handle();
                    let message = MessageSubmission {
                        author: MessageAuthor::Source {
                            cause_id,
                            ancestry,
                            message_id: Arc::clone(&id),
                            source: mobius::protocol::MessageSource::Session {
                                session_id: sender.session_id,
                            },
                            handle: handle.into(),
                            symbol: None,
                        },
                        text,
                        attachments: Vec::new(),
                        reply: None,
                        requested_delivery: Some(delivery),
                        target_turn_id: None,
                    };
                    (target, Op::Message { message })
                }
                PeerCommand::Interrupt { target, turn_id } => (
                    ChatTarget::Existing(target),
                    Op::Interrupt {
                        turn_id: turn_id.into(),
                    },
                ),
            };
            let submission = ValidatedSubmission::new(Submission { id, op })?;
            let host = match target {
                ChatTarget::Workspace(workspace) => (self.1)()
                    .map_err(|error| mobius::Error::Tool(error.to_string()))?
                    .create_session(workspace, &bot_id)
                    .await
                    .map_err(|error| mobius::Error::Tool(error.message.to_string()))?,
                ChatTarget::Existing(target) => {
                    let mut matches = chats.into_iter().filter(|open| open.chat.is_target(target));
                    let found = (matches.next(), matches.next());
                    match found {
                        (Some(open), None) => match open.host {
                            Some(host) => host,
                            None => (self.1)()
                                .map_err(|error| mobius::Error::Tool(error.to_string()))?
                                .open_session(&open.chat.session_id)
                                .await
                                .map_err(|error| mobius::Error::Tool(error.message.to_string()))?,
                        },
                        (Some(_), Some(_)) => {
                            return Err(mobius::Error::Tool(
                                "target names more than one chat; use its full session ID".into(),
                            ));
                        }
                        (None, _) => {
                            return Err(mobius::Error::Tool(
                                "target is not another available chat of this Bot; call list_chats"
                                    .into(),
                            ));
                        }
                    }
                }
            };
            let target = host.session_id().share();
            // The recipient re-checks ownership in its own command order.
            host.deliver_validated_source(submission, bot_id)
                .await
                .map_err(|rejection| mobius::Error::Tool(rejection.message.to_string()))?;
            Ok(target)
        })
    }
}
