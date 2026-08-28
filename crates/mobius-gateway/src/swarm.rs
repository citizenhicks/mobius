//! Durable gateway-owned swarm membership and message-board state.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mobius::backend::checkpoint::CheckpointStore;
use mobius::middleware::swarm::SwarmBackend;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, MutexGuard, mpsc};
use uuid::Uuid;

use crate::wire::{SwarmMemberRecord, SwarmMessageRecord, SwarmRecord};
use crate::{Error, Result};

const STATE_SCOPE: &str = "gateway";
const STATE_KEY: &str = "swarms.v1";
const MAX_HANDLE_BYTES: usize = 64;
const MAX_ID_BYTES: usize = 512;
const MAX_TITLE_BYTES: usize = 256;
const MAX_MESSAGE_BYTES: usize = 24_000;
const MAX_ACKNOWLEDGED_ENTRIES: usize = 256;
const MAX_PENDING_DELIVERIES_PER_RECIPIENT: usize = 256;
const MAX_PAGE_ENTRIES: usize = 256;
const MAX_SWARM_MEMBERS: usize = 100;
// ponytail: one 8 MiB catalog; split board storage and wire pages if real usage reaches it.
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOOL_READ_BYTES: usize = 32_000;
const MAX_TOOL_READ_BODY_BYTES: usize = 4_000;

/// One conversation participating in a swarm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmMember {
    /// Visible gateway session identifier.
    pub session_id: String,
    /// Unique, mentionable handle within this swarm.
    pub handle: String,
    /// Time this membership was created, in Unix milliseconds.
    pub joined_at_ms: i64,
}

/// Compact durable swarm metadata and its current roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmSummary {
    /// Stable generated swarm identifier.
    pub id: String,
    /// User-visible swarm title.
    pub title: String,
    /// Session identifier of the swarm leader.
    pub leader_session_id: String,
    /// Current members ordered by handle.
    pub members: Vec<SwarmMember>,
    /// Latest board sequence assigned in this swarm.
    pub latest_sequence: u64,
    /// Time the swarm was created, in Unix milliseconds.
    pub created_at_ms: i64,
    /// Time its roster or board was last changed, in Unix milliseconds.
    pub updated_at_ms: i64,
}

/// A swarm snapshot resolved for one participating session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmSnapshot {
    /// The swarm and current roster.
    pub swarm: SwarmSummary,
    /// This session's immutable handle in the swarm.
    pub handle: String,
}

/// One durable swarm board entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardEntry {
    /// Stable generated message identifier.
    pub id: String,
    /// Monotonic sequence within the swarm board.
    pub sequence: u64,
    /// Time the entry was created, in Unix milliseconds.
    pub created_at_ms: i64,
    /// Author identity as it existed when the message was posted.
    pub author: SwarmMember,
    /// Message text.
    pub text: String,
    /// Session identifiers resolved from the entry's `@handle` mentions.
    pub mentioned_recipient_session_ids: Vec<String>,
    /// Mentioned sessions which have not acknowledged delivery.
    pub pending_recipient_session_ids: Vec<String>,
}

/// The durable entry created by a post and the conversations to notify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmPost {
    /// Newly-created durable board entry.
    pub entry: BoardEntry,
    /// Resolved recipient sessions, ready for the gateway delivery bridge.
    pub resolved_recipient_session_ids: Vec<String>,
}

/// One newest-first page of swarm board entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardPage {
    /// Newest-first entries in this page.
    pub entries: Vec<BoardEntry>,
    /// Sequence to pass as `before_sequence` for the next older page.
    pub next_before_sequence: Option<u64>,
}

/// One message awaiting delivery to a particular conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingDelivery {
    /// Swarm containing the message.
    pub swarm_id: String,
    /// User-visible title snapshotted from the current swarm.
    pub swarm_title: String,
    /// Durable message awaiting acknowledgement.
    pub entry: BoardEntry,
}

/// Wake-up signal consumed by the gateway's swarm delivery worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwarmDelivery {
    /// Durable board contents changed and connected clients need a fresh catalog.
    Changed,
    /// Gateway capacity changed and durable pending recipients should be retried.
    RetryPending,
    /// A target has at least one durable board message awaiting delivery.
    Pending { target_session_id: String },
    /// A target durably recorded one delivered peer message.
    Acknowledged {
        target_session_id: String,
        message_id: String,
    },
}

/// Cloneable access to the gateway's durable swarm catalog.
#[derive(Clone)]
pub struct SwarmStore {
    checkpoints: Arc<dyn CheckpointStore>,
    state: Arc<Mutex<Option<Catalog>>>,
    deliveries: mpsc::UnboundedSender<SwarmDelivery>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    swarms: BTreeMap<String, StoredSwarm>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSwarm {
    title: String,
    leader_session_id: String,
    members: BTreeMap<String, SwarmMember>,
    latest_sequence: u64,
    board: VecDeque<BoardEntry>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

impl SwarmStore {
    /// Creates a lazily-loaded store and its single gateway delivery queue.
    #[must_use]
    pub(crate) fn new(
        checkpoints: Arc<dyn CheckpointStore>,
    ) -> (Self, mpsc::UnboundedReceiver<SwarmDelivery>) {
        let (deliveries, receiver) = mpsc::unbounded_channel();
        (
            Self {
                checkpoints,
                state: Arc::new(Mutex::new(None)),
                deliveries,
            },
            receiver,
        )
    }

    /// Returns every swarm ordered by its stable identifier.
    pub async fn summaries(&self) -> Result<Vec<SwarmSummary>> {
        let state = self.lock_loaded().await?;
        Ok(state
            .as_ref()
            .expect("swarm catalog loaded")
            .swarms
            .iter()
            .map(|(id, swarm)| swarm.summary(id))
            .collect())
    }

    pub(crate) async fn records(&self) -> Result<Vec<SwarmRecord>> {
        let state = self.lock_loaded().await?;
        Ok(state
            .as_ref()
            .expect("swarm catalog loaded")
            .swarms
            .iter()
            .map(|(id, swarm)| {
                let mut messages = swarm
                    .board
                    .iter()
                    .rev()
                    .take(MAX_PAGE_ENTRIES)
                    .map(|entry| SwarmMessageRecord {
                        id: entry.id.clone(),
                        sequence: entry.sequence,
                        author_session_id: entry.author.session_id.clone(),
                        author_handle: entry.author.handle.clone(),
                        body: entry.text.clone(),
                        created_at_ms: entry.created_at_ms,
                    })
                    .collect::<Vec<_>>();
                messages.reverse();
                SwarmRecord {
                    id: id.clone(),
                    title: swarm.title.clone(),
                    leader_session_id: swarm.leader_session_id.clone(),
                    members: swarm
                        .members
                        .values()
                        .map(|member| SwarmMemberRecord {
                            session_id: member.session_id.clone(),
                            handle: member.handle.clone(),
                        })
                        .collect(),
                    messages,
                    updated_at_ms: swarm.updated_at_ms,
                }
            })
            .collect())
    }

    /// Creates a neutrally named swarm with gateway-generated member handles.
    pub(crate) async fn create(
        &self,
        leader_session_id: String,
        member_session_ids: Vec<String>,
    ) -> Result<SwarmSummary> {
        validate_swarm_members(&leader_session_id, &member_session_ids)?;

        self.mutate(move |catalog| {
            for session_id in &member_session_ids {
                ensure_session_available(catalog, session_id)?;
            }
            let id = Uuid::new_v4();
            let title = format!(
                "Swarm {}",
                id.simple().to_string().chars().take(8).collect::<String>()
            );
            let mut members = BTreeMap::new();
            for session_id in member_session_ids {
                let member = generated_member(session_id, &members);
                members.insert(member.handle.clone(), member);
            }
            let now = unix_ms();
            let swarm = StoredSwarm {
                title,
                leader_session_id,
                members,
                latest_sequence: 0,
                board: VecDeque::new(),
                created_at_ms: now,
                updated_at_ms: now,
            };
            let id = id.to_string();
            let summary = swarm.summary(&id);
            catalog.swarms.insert(id, swarm);
            Ok(summary)
        })
        .await
    }

    /// Adds one session under a new gateway-generated handle.
    pub(crate) async fn join(&self, swarm_id: &str, session_id: String) -> Result<SwarmSummary> {
        validate_swarm_id(swarm_id)?;
        validate_session_id(&session_id)?;
        let swarm_id = swarm_id.to_owned();
        self.mutate(move |catalog| {
            ensure_session_available(catalog, &session_id)?;
            let swarm = catalog
                .swarms
                .get_mut(&swarm_id)
                .ok_or_else(|| config(format!("unknown swarm `{swarm_id}`")))?;
            if swarm.members.len() >= MAX_SWARM_MEMBERS {
                return Err(config(format!(
                    "a swarm supports at most {MAX_SWARM_MEMBERS} members"
                )));
            }
            let member = generated_member(session_id, &swarm.members);
            swarm.members.insert(member.handle.clone(), member);
            swarm.updated_at_ms = unix_ms();
            Ok(swarm.summary(&swarm_id))
        })
        .await
    }

    /// Removes a non-leader session from a swarm.
    ///
    /// A leader must explicitly disband its swarm. A one-member roster remains valid.
    pub(crate) async fn leave(&self, swarm_id: &str, session_id: &str) -> Result<SwarmSummary> {
        validate_swarm_id(swarm_id)?;
        validate_session_id(session_id)?;
        let swarm_id = swarm_id.to_owned();
        let session_id = session_id.to_owned();
        let acknowledged_target = session_id.clone();
        let (summary, acknowledged_messages) = self
            .mutate(move |catalog| {
                let swarm = catalog
                    .swarms
                    .get_mut(&swarm_id)
                    .ok_or_else(|| config(format!("unknown swarm `{swarm_id}`")))?;
                if swarm.leader_session_id == session_id {
                    return Err(config("swarm leader must disband instead of leaving"));
                }
                let handle = swarm
                    .members
                    .iter()
                    .find_map(|(handle, member)| {
                        (member.session_id == session_id).then(|| handle.clone())
                    })
                    .ok_or_else(|| {
                        config(format!("session `{session_id}` is not in this swarm"))
                    })?;
                swarm.members.remove(&handle);
                let mut acknowledged_messages = Vec::new();
                for entry in &mut swarm.board {
                    if entry
                        .pending_recipient_session_ids
                        .iter()
                        .any(|pending| pending == &session_id)
                    {
                        acknowledged_messages.push(entry.id.clone());
                    }
                    entry
                        .pending_recipient_session_ids
                        .retain(|pending| pending != &session_id);
                }
                trim_acknowledged(&mut swarm.board);
                swarm.updated_at_ms = unix_ms();
                Ok((swarm.summary(&swarm_id), acknowledged_messages))
            })
            .await?;
        for message_id in acknowledged_messages {
            self.notify_acknowledged(&message_id, &acknowledged_target);
        }
        Ok(summary)
    }

    /// Permanently removes a swarm and its board.
    pub(crate) async fn disband(&self, swarm_id: &str) -> Result<SwarmSummary> {
        validate_swarm_id(swarm_id)?;
        let swarm_id = swarm_id.to_owned();
        let (summary, pending_deliveries) = self
            .mutate(move |catalog| {
                let swarm = catalog
                    .swarms
                    .remove(&swarm_id)
                    .ok_or_else(|| config(format!("unknown swarm `{swarm_id}`")))?;
                let pending_deliveries = swarm
                    .board
                    .iter()
                    .flat_map(|entry| {
                        entry
                            .pending_recipient_session_ids
                            .iter()
                            .map(|target| (entry.id.clone(), target.clone()))
                    })
                    .collect::<BTreeSet<_>>();
                Ok((swarm.summary(&swarm_id), pending_deliveries))
            })
            .await?;
        for (message_id, target_session_id) in pending_deliveries {
            self.notify_acknowledged(&message_id, &target_session_id);
        }
        Ok(summary)
    }

    /// Resolves the swarm and handle for one participating session.
    pub async fn snapshot_for_session(&self, session_id: &str) -> Result<Option<SwarmSnapshot>> {
        validate_session_id(session_id)?;
        let state = self.lock_loaded().await?;
        let catalog = state.as_ref().expect("swarm catalog loaded");
        Ok(catalog.swarms.iter().find_map(|(id, swarm)| {
            swarm
                .members
                .values()
                .find(|member| member.session_id == session_id)
                .map(|member| SwarmSnapshot {
                    swarm: swarm.summary(id),
                    handle: member.handle.clone(),
                })
        }))
    }

    /// Posts one durable board entry and resolves all `@handle` recipients.
    pub async fn post(&self, sender_session_id: &str, text: String) -> Result<SwarmPost> {
        validate_session_id(sender_session_id)?;
        validate_message(&text)?;
        let sender_session_id = sender_session_id.to_owned();
        let post = self
            .mutate(move |catalog| {
                let swarm_id =
                    swarm_id_for_session(catalog, &sender_session_id).ok_or_else(|| {
                        config(format!("session `{sender_session_id}` is not in a swarm"))
                    })?;
                let swarm = catalog
                    .swarms
                    .get_mut(&swarm_id)
                    .expect("resolved swarm exists");
                let author = swarm
                    .members
                    .values()
                    .find(|member| member.session_id == sender_session_id)
                    .cloned()
                    .expect("resolved swarm contains sender");

                let handles = mentioned_handles(&text);
                let unknown = handles
                    .iter()
                    .filter(|handle| !swarm.members.contains_key(*handle))
                    .cloned()
                    .collect::<Vec<_>>();
                if !unknown.is_empty() {
                    return Err(config(format!(
                        "unknown swarm mention{}: {}",
                        if unknown.len() == 1 { "" } else { "s" },
                        unknown
                            .iter()
                            .map(|handle| format!("@{handle}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }

                let recipients = handles
                    .iter()
                    .map(|handle| {
                        swarm
                            .members
                            .get(handle)
                            .expect("mentions validated against roster")
                            .session_id
                            .clone()
                    })
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                if recipients
                    .iter()
                    .any(|session| session == &sender_session_id)
                {
                    return Err(config("a swarm message cannot mention its author"));
                }
                if let Some(recipient) = recipients.iter().find(|recipient| {
                    swarm
                        .board
                        .iter()
                        .filter(|entry| {
                            entry
                                .pending_recipient_session_ids
                                .iter()
                                .any(|pending| pending == *recipient)
                        })
                        .count()
                        >= MAX_PENDING_DELIVERIES_PER_RECIPIENT
                }) {
                    return Err(config(format!(
                        "session `{recipient}` has {MAX_PENDING_DELIVERIES_PER_RECIPIENT} pending swarm messages"
                    )));
                }
                let sequence = swarm
                    .latest_sequence
                    .checked_add(1)
                    .ok_or_else(|| config("swarm board sequence exhausted"))?;
                let entry = BoardEntry {
                    id: Uuid::new_v4().to_string(),
                    sequence,
                    created_at_ms: unix_ms(),
                    author,
                    text,
                    mentioned_recipient_session_ids: recipients.clone(),
                    pending_recipient_session_ids: recipients.clone(),
                };
                swarm.latest_sequence = sequence;
                swarm.updated_at_ms = entry.created_at_ms;
                swarm.board.push_back(entry.clone());
                trim_acknowledged(&mut swarm.board);
                Ok(SwarmPost {
                    entry,
                    resolved_recipient_session_ids: recipients,
                })
            })
            .await?;
        let _ = self.deliveries.send(SwarmDelivery::Changed);
        for target_session_id in &post.resolved_recipient_session_ids {
            self.notify_pending(target_session_id);
        }
        Ok(post)
    }

    /// Loads one newest-first board page before an optional sequence cursor.
    pub async fn board_page(
        &self,
        swarm_id: &str,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> Result<BoardPage> {
        validate_swarm_id(swarm_id)?;
        if limit == 0 || limit > MAX_PAGE_ENTRIES {
            return Err(config(format!(
                "board page limit must be 1-{MAX_PAGE_ENTRIES}"
            )));
        }
        let state = self.lock_loaded().await?;
        let swarm = state
            .as_ref()
            .expect("swarm catalog loaded")
            .swarms
            .get(swarm_id)
            .ok_or_else(|| config(format!("unknown swarm `{swarm_id}`")))?;
        let mut matches = swarm
            .board
            .iter()
            .rev()
            .filter(|entry| before_sequence.is_none_or(|before| entry.sequence < before));
        let entries = matches.by_ref().take(limit).cloned().collect::<Vec<_>>();
        let next_before_sequence = matches
            .next()
            .and_then(|_| entries.last().map(|entry| entry.sequence));
        Ok(BoardPage {
            entries,
            next_before_sequence,
        })
    }

    /// Returns messages still awaiting one target session's acknowledgement.
    pub async fn pending_deliveries(
        &self,
        target_session_id: &str,
    ) -> Result<Vec<PendingDelivery>> {
        validate_session_id(target_session_id)?;
        let state = self.lock_loaded().await?;
        let catalog = state.as_ref().expect("swarm catalog loaded");
        Ok(catalog
            .swarms
            .iter()
            .flat_map(|(swarm_id, swarm)| {
                swarm
                    .board
                    .iter()
                    .filter(|entry| {
                        entry
                            .pending_recipient_session_ids
                            .iter()
                            .any(|pending| pending == target_session_id)
                    })
                    .map(|entry| PendingDelivery {
                        swarm_id: swarm_id.clone(),
                        swarm_title: swarm.title.clone(),
                        entry: entry.clone(),
                    })
            })
            .collect())
    }

    /// Returns each conversation with at least one durable pending mention.
    pub(crate) async fn pending_recipient_session_ids(&self) -> Result<Vec<String>> {
        let state = self.lock_loaded().await?;
        Ok(state
            .as_ref()
            .expect("swarm catalog loaded")
            .swarms
            .values()
            .flat_map(|swarm| &swarm.board)
            .flat_map(|entry| &entry.pending_recipient_session_ids)
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }

    pub(crate) fn notify_pending(&self, target_session_id: &str) {
        let _ = self.deliveries.send(SwarmDelivery::Pending {
            target_session_id: target_session_id.to_owned(),
        });
    }

    pub(crate) fn retry_pending(&self) {
        let _ = self.deliveries.send(SwarmDelivery::RetryPending);
    }

    pub(crate) fn notify_acknowledged(&self, message_id: &str, target_session_id: &str) {
        let _ = self.deliveries.send(SwarmDelivery::Acknowledged {
            target_session_id: target_session_id.to_owned(),
            message_id: message_id.to_owned(),
        });
    }

    /// Acknowledges one target's message delivery.
    ///
    /// Returns `false` when that target already acknowledged the entry.
    pub async fn acknowledge(&self, message_id: &str, target_session_id: &str) -> Result<bool> {
        validate_message_id(message_id)?;
        validate_session_id(target_session_id)?;
        let message_id = message_id.to_owned();
        let target_session_id = target_session_id.to_owned();
        let acknowledged_message = message_id.clone();
        let acknowledged_target = target_session_id.clone();
        let was_pending = self
            .mutate(move |catalog| {
                let entry = catalog
                    .swarms
                    .values_mut()
                    .find_map(|swarm| swarm.board.iter_mut().find(|entry| entry.id == message_id))
                    .ok_or_else(|| config(format!("unknown swarm message `{message_id}`")))?;
                if !entry
                    .mentioned_recipient_session_ids
                    .iter()
                    .any(|recipient| recipient == &target_session_id)
                {
                    return Err(config(format!(
                        "session `{target_session_id}` is not a recipient of message `{message_id}`"
                    )));
                }
                let was_pending = entry
                    .pending_recipient_session_ids
                    .iter()
                    .any(|pending| pending == &target_session_id);
                entry
                    .pending_recipient_session_ids
                    .retain(|pending| pending != &target_session_id);
                for swarm in catalog.swarms.values_mut() {
                    trim_acknowledged(&mut swarm.board);
                }
                Ok(was_pending)
            })
            .await?;
        self.notify_acknowledged(&acknowledged_message, &acknowledged_target);
        Ok(was_pending)
    }

    async fn mutate<T>(&self, mutation: impl FnOnce(&mut Catalog) -> Result<T>) -> Result<T> {
        let mut state = self.lock_loaded().await?;
        let mut candidate = state.as_ref().expect("swarm catalog loaded").clone();
        let output = mutation(&mut candidate)?;
        validate_catalog(&candidate)?;
        self.checkpoints
            .save_state(STATE_SCOPE, STATE_KEY, &serde_json::to_value(&candidate)?)
            .await?;
        *state = Some(candidate);
        Ok(output)
    }

    async fn lock_loaded(&self) -> Result<MutexGuard<'_, Option<Catalog>>> {
        let mut state = self.state.lock().await;
        if state.is_none() {
            let catalog = self
                .checkpoints
                .load_state(STATE_SCOPE, STATE_KEY)
                .await?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default();
            validate_catalog(&catalog)?;
            *state = Some(catalog);
        }
        Ok(state)
    }
}

pub(crate) fn validate_swarm_members(
    leader_session_id: &str,
    member_session_ids: &[String],
) -> Result<()> {
    validate_session_id(leader_session_id)?;
    if member_session_ids.len() < 2 {
        return Err(config("a swarm requires at least two members"));
    }
    if member_session_ids.len() > MAX_SWARM_MEMBERS {
        return Err(config(format!(
            "a swarm supports at most {MAX_SWARM_MEMBERS} members"
        )));
    }
    let mut unique_sessions = BTreeSet::new();
    for session_id in member_session_ids {
        validate_session_id(session_id)?;
        if !unique_sessions.insert(session_id.as_str()) {
            return Err(config(format!(
                "session `{session_id}` appears more than once"
            )));
        }
    }
    if !unique_sessions.contains(leader_session_id) {
        return Err(config("swarm leader must be a member"));
    }
    Ok(())
}

impl SwarmBackend for SwarmStore {
    fn active<'a>(&'a self, session_id: &'a str) -> mobius::BoxFuture<'a, mobius::Result<bool>> {
        Box::pin(async move {
            self.snapshot_for_session(session_id)
                .await
                .map(|snapshot| snapshot.is_some())
                .map_err(mobius_error)
        })
    }

    fn roster<'a>(&'a self, session_id: &'a str) -> mobius::BoxFuture<'a, mobius::Result<String>> {
        Box::pin(async move {
            let snapshot = self
                .snapshot_for_session(session_id)
                .await
                .map_err(mobius_error)?
                .ok_or_else(|| mobius::Error::Config("this chat is not in a swarm".into()))?;
            serde_json::to_string(&snapshot).map_err(Into::into)
        })
    }

    fn read<'a>(&'a self, session_id: &'a str) -> mobius::BoxFuture<'a, mobius::Result<String>> {
        Box::pin(async move {
            let snapshot = self
                .snapshot_for_session(session_id)
                .await
                .map_err(mobius_error)?
                .ok_or_else(|| mobius::Error::Config("this chat is not in a swarm".into()))?;
            let page = self
                .board_page(&snapshot.swarm.id, None, MAX_PAGE_ENTRIES)
                .await
                .map_err(mobius_error)?;
            let mut entries = Vec::new();
            let mut has_older = page.next_before_sequence.is_some();
            for entry in page.entries {
                let end = entry
                    .text
                    .floor_char_boundary(MAX_TOOL_READ_BODY_BYTES.min(entry.text.len()));
                let body = &entry.text[..end];
                entries.push(serde_json::json!({
                    "id": entry.id,
                    "sequence": entry.sequence,
                    "created_at_ms": entry.created_at_ms,
                    "author_session_id": entry.author.session_id,
                    "author_handle": entry.author.handle,
                    "body": body,
                    "body_truncated": body.len() != entry.text.len(),
                }));
                if serde_json::to_vec(&serde_json::json!({
                    "entries": &entries,
                    "has_older": has_older,
                }))?
                .len()
                    > MAX_TOOL_READ_BYTES
                {
                    entries.pop();
                    has_older = true;
                    break;
                }
            }
            serde_json::to_string(&serde_json::json!({
                "entries": entries,
                "has_older": has_older,
            }))
            .map_err(Into::into)
        })
    }

    fn post<'a>(
        &'a self,
        session_id: &'a str,
        message: String,
    ) -> mobius::BoxFuture<'a, mobius::Result<String>> {
        Box::pin(async move {
            let post = SwarmStore::post(self, session_id, message)
                .await
                .map_err(mobius_error)?;
            serde_json::to_string(&post).map_err(Into::into)
        })
    }
}

impl StoredSwarm {
    fn summary(&self, id: &str) -> SwarmSummary {
        SwarmSummary {
            id: id.to_owned(),
            title: self.title.clone(),
            leader_session_id: self.leader_session_id.clone(),
            members: self.members.values().cloned().collect(),
            latest_sequence: self.latest_sequence,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
        }
    }
}

fn generated_member(session_id: String, members: &BTreeMap<String, SwarmMember>) -> SwarmMember {
    let handle = loop {
        let suffix = Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect::<String>();
        let handle = format!("agent_{suffix}");
        if !members.contains_key(&handle) {
            break handle;
        }
    };
    SwarmMember {
        session_id,
        handle,
        joined_at_ms: unix_ms(),
    }
}

fn ensure_session_available(catalog: &Catalog, session_id: &str) -> Result<()> {
    if catalog.swarms.values().any(|swarm| {
        swarm
            .members
            .values()
            .any(|member| member.session_id == session_id)
    }) {
        return Err(config(format!(
            "session `{session_id}` already belongs to a swarm"
        )));
    }
    Ok(())
}

fn swarm_id_for_session(catalog: &Catalog, session_id: &str) -> Option<String> {
    catalog.swarms.iter().find_map(|(id, swarm)| {
        swarm
            .members
            .values()
            .any(|member| member.session_id == session_id)
            .then(|| id.clone())
    })
}

fn trim_acknowledged(board: &mut VecDeque<BoardEntry>) {
    while board
        .iter()
        .filter(|entry| entry.pending_recipient_session_ids.is_empty())
        .count()
        > MAX_ACKNOWLEDGED_ENTRIES
    {
        let index = board
            .iter()
            .position(|entry| entry.pending_recipient_session_ids.is_empty())
            .expect("acknowledged entry count is positive");
        board.remove(index);
    }
}

fn mentioned_handles(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut handles = BTreeSet::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'@'
            || index
                .checked_sub(1)
                .is_some_and(|previous| is_mention_byte(bytes[previous]))
        {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && is_mention_byte(bytes[end]) {
            end += 1;
        }
        if start < end {
            handles.insert(text[start..end].to_owned());
        }
        index = end.max(index + 1);
    }
    handles
}

const fn is_mention_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn validate_catalog(catalog: &Catalog) -> Result<()> {
    let mut sessions = BTreeSet::new();
    let mut message_ids = BTreeSet::new();
    for (id, swarm) in &catalog.swarms {
        let mut pending_counts = BTreeMap::<&str, usize>::new();
        validate_swarm_id(id)?;
        validate_title(&swarm.title)?;
        validate_session_id(&swarm.leader_session_id)?;
        if swarm.members.len() > MAX_SWARM_MEMBERS {
            return Err(config(format!(
                "swarm `{id}` has more than {MAX_SWARM_MEMBERS} members"
            )));
        }
        if swarm.created_at_ms < 0 || swarm.updated_at_ms < swarm.created_at_ms {
            return Err(config(format!("swarm `{id}` has invalid timestamps")));
        }
        if !swarm
            .members
            .values()
            .any(|member| member.session_id == swarm.leader_session_id)
        {
            return Err(config(format!("swarm `{id}` leader is not a member")));
        }
        for (handle, member) in &swarm.members {
            validate_member(member)?;
            if handle != &member.handle {
                return Err(config(format!(
                    "swarm `{id}` member handle does not match its key"
                )));
            }
            if !sessions.insert(member.session_id.clone()) {
                return Err(config(format!(
                    "session `{}` belongs to more than one swarm",
                    member.session_id
                )));
            }
        }
        let mut previous_sequence = 0;
        for entry in &swarm.board {
            validate_message_id(&entry.id)?;
            validate_member(&entry.author)?;
            validate_message(&entry.text)?;
            if entry.created_at_ms < swarm.created_at_ms
                || entry.created_at_ms > swarm.updated_at_ms
            {
                return Err(config(format!(
                    "swarm message `{}` has an invalid timestamp",
                    entry.id
                )));
            }
            if !message_ids.insert(entry.id.clone()) {
                return Err(config(format!(
                    "swarm message `{}` appears more than once",
                    entry.id
                )));
            }
            if entry.sequence <= previous_sequence || entry.sequence > swarm.latest_sequence {
                return Err(config(format!("swarm `{id}` has invalid board sequences")));
            }
            previous_sequence = entry.sequence;
            validate_recipient_ids(&entry.mentioned_recipient_session_ids)?;
            validate_recipient_ids(&entry.pending_recipient_session_ids)?;
            for recipient in &entry.pending_recipient_session_ids {
                let count = pending_counts.entry(recipient).or_default();
                *count += 1;
                if *count > MAX_PENDING_DELIVERIES_PER_RECIPIENT {
                    return Err(config(format!(
                        "session `{recipient}` has too many pending swarm messages"
                    )));
                }
            }
            if entry.pending_recipient_session_ids.iter().any(|pending| {
                !entry
                    .mentioned_recipient_session_ids
                    .iter()
                    .any(|mentioned| mentioned == pending)
            }) {
                return Err(config(format!(
                    "swarm message `{}` has a pending non-recipient",
                    entry.id
                )));
            }
        }
        if swarm
            .board
            .back()
            .is_some_and(|entry| entry.sequence != swarm.latest_sequence)
            || (swarm.board.is_empty() && swarm.latest_sequence != 0)
            || swarm
                .board
                .iter()
                .filter(|entry| entry.pending_recipient_session_ids.is_empty())
                .count()
                > MAX_ACKNOWLEDGED_ENTRIES
        {
            return Err(config(format!("swarm `{id}` board state is invalid")));
        }
    }
    if serde_json::to_vec(catalog)?.len() > MAX_CATALOG_BYTES {
        return Err(config(format!(
            "swarm catalog exceeds {MAX_CATALOG_BYTES} encoded bytes"
        )));
    }
    Ok(())
}

fn validate_recipient_ids(recipients: &[String]) -> Result<()> {
    let mut unique = BTreeSet::new();
    for recipient in recipients {
        validate_session_id(recipient)?;
        if !unique.insert(recipient) {
            return Err(config("swarm message recipient appears more than once"));
        }
    }
    Ok(())
}

fn validate_member(member: &SwarmMember) -> Result<()> {
    validate_session_id(&member.session_id)?;
    validate_handle(&member.handle)?;
    if member.joined_at_ms < 0 {
        return Err(config("swarm member join time cannot be negative"));
    }
    Ok(())
}

fn validate_handle(handle: &str) -> Result<()> {
    if handle.is_empty()
        || handle.len() > MAX_HANDLE_BYTES
        || !handle
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(config(
            "swarm handle must contain 1-64 lowercase letters, digits, or underscores",
        ));
    }
    Ok(())
}

fn validate_title(title: &str) -> Result<()> {
    if title.trim().is_empty() || title.len() > MAX_TITLE_BYTES {
        return Err(config(format!(
            "swarm title must contain 1-{MAX_TITLE_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn validate_message(text: &str) -> Result<()> {
    if text.trim().is_empty() || text.len() > MAX_MESSAGE_BYTES {
        return Err(config(format!(
            "swarm message must contain 1-{MAX_MESSAGE_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty() || session_id.len() > MAX_ID_BYTES {
        return Err(config(format!(
            "session id must contain 1-{MAX_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_swarm_id(id: &str) -> Result<()> {
    Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| config("invalid swarm id"))
}

fn validate_message_id(id: &str) -> Result<()> {
    Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| config("invalid swarm message id"))
}

fn config(message: impl Into<String>) -> Error {
    Error::Config(message.into())
}

fn mobius_error(error: Error) -> mobius::Error {
    mobius::Error::Config(error.to_string())
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use mobius::backend::checkpoint::sqlite::SqliteCheckpoint;

    use super::*;

    fn store() -> (
        tempfile::TempDir,
        Arc<dyn CheckpointStore>,
        SwarmStore,
        mpsc::UnboundedReceiver<SwarmDelivery>,
    ) {
        let directory = tempfile::tempdir().expect("workspace");
        let checkpoints: Arc<dyn CheckpointStore> = Arc::new(
            SqliteCheckpoint::new(directory.path().join("checkpoints.sqlite3"))
                .expect("checkpoints"),
        );
        let (store, deliveries) = SwarmStore::new(Arc::clone(&checkpoints));
        (directory, checkpoints, store, deliveries)
    }

    async fn create_swarm(store: &SwarmStore) -> SwarmSummary {
        store
            .create("leader".into(), vec!["leader".into(), "reviewer".into()])
            .await
            .expect("create swarm")
    }

    #[tokio::test]
    async fn membership_is_unique_and_lazily_reloads() {
        let (_directory, checkpoints, store, _deliveries) = store();
        let created = create_swarm(&store).await;
        let created_title = created.title.clone();
        assert!(created.created_at_ms > 0);
        assert_eq!(created.created_at_ms, created.updated_at_ms);

        let duplicate = store
            .create("leader".into(), vec!["leader".into(), "third".into()])
            .await
            .expect_err("session cannot join two swarms");
        assert!(duplicate.to_string().contains("already belongs"));

        let (reloaded, _deliveries) = SwarmStore::new(checkpoints);
        assert_eq!(reloaded.summaries().await.expect("reload"), vec![created]);
        let snapshot = reloaded
            .snapshot_for_session("reviewer")
            .await
            .expect("snapshot")
            .expect("membership");
        assert!(snapshot.handle.starts_with("agent_"));
        assert_eq!(snapshot.swarm.title, created_title);
    }

    #[tokio::test]
    async fn joining_cannot_exceed_the_member_limit() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        let members = (0..MAX_SWARM_MEMBERS)
            .map(|index| format!("member-{index}"))
            .collect::<Vec<_>>();
        let swarm = store
            .create(members[0].clone(), members)
            .await
            .expect("full swarm");

        let error = store
            .join(&swarm.id, "overflow".into())
            .await
            .expect_err("member limit");

        assert!(error.to_string().contains("at most 100"));
    }

    #[tokio::test]
    async fn mentions_are_resolved_delivered_and_acknowledged() {
        let (_directory, checkpoints, store, mut deliveries) = store();
        let swarm = create_swarm(&store).await;
        let leader_handle = swarm
            .members
            .iter()
            .find(|member| member.session_id == "leader")
            .expect("leader")
            .handle
            .clone();
        let reviewer_handle = swarm
            .members
            .iter()
            .find(|member| member.session_id == "reviewer")
            .expect("reviewer")
            .handle
            .clone();

        let unknown = store
            .post("leader", "Can @missing check this?".into())
            .await
            .expect_err("unknown mention");
        assert!(unknown.to_string().contains("@missing"));

        let body = format!("Can @{reviewer_handle} check this?");
        let post = store.post("leader", body.clone()).await.expect("post");
        assert_eq!(deliveries.recv().await, Some(SwarmDelivery::Changed));
        assert_eq!(
            deliveries.recv().await,
            Some(SwarmDelivery::Pending {
                target_session_id: "reviewer".into()
            })
        );
        assert_eq!(post.entry.sequence, 1);
        assert!(post.entry.created_at_ms >= swarm.created_at_ms);
        assert_eq!(post.entry.author.handle, leader_handle);
        assert_eq!(post.resolved_recipient_session_ids, ["reviewer"]);
        let records = store.records().await.expect("wire records");
        assert_eq!(records[0].messages[0].body, body);
        assert!(
            SwarmBackend::active(&store, "reviewer")
                .await
                .expect("active")
        );
        assert_eq!(
            store
                .pending_deliveries("reviewer")
                .await
                .expect("pending")
                .len(),
            1
        );

        assert!(
            store
                .acknowledge(&post.entry.id, "reviewer")
                .await
                .expect("acknowledge")
        );
        assert_eq!(
            deliveries.recv().await,
            Some(SwarmDelivery::Acknowledged {
                target_session_id: "reviewer".into(),
                message_id: post.entry.id.clone(),
            })
        );
        assert!(
            !store
                .acknowledge(&post.entry.id, "reviewer")
                .await
                .expect("idempotent acknowledge")
        );
        assert_eq!(
            deliveries.recv().await,
            Some(SwarmDelivery::Acknowledged {
                target_session_id: "reviewer".into(),
                message_id: post.entry.id.clone(),
            })
        );
        assert!(
            store
                .pending_deliveries("reviewer")
                .await
                .expect("pending")
                .is_empty()
        );

        let (reloaded, _deliveries) = SwarmStore::new(checkpoints);
        let page = reloaded
            .board_page(&swarm.id, None, 10)
            .await
            .expect("board page");
        assert_eq!(page.entries[0].id, post.entry.id);
        assert!(page.entries[0].pending_recipient_session_ids.is_empty());
    }

    #[tokio::test]
    async fn leave_and_disband_settle_removed_pending_deliveries() {
        let (_directory, _checkpoints, store, mut deliveries) = store();
        let swarm = store
            .create(
                "leader".into(),
                vec!["leader".into(), "observer".into(), "reviewer".into()],
            )
            .await
            .expect("create swarm");
        let handle = |session_id: &str| {
            swarm
                .members
                .iter()
                .find(|member| member.session_id == session_id)
                .expect("member")
                .handle
                .clone()
        };
        let post = store
            .post(
                "leader",
                format!(
                    "@{} @{} please review",
                    handle("observer"),
                    handle("reviewer")
                ),
            )
            .await
            .expect("post");
        for _ in 0..3 {
            deliveries.recv().await.expect("initial delivery signal");
        }

        store
            .leave(&swarm.id, "reviewer")
            .await
            .expect("leave swarm");
        store.disband(&swarm.id).await.expect("disband swarm");

        assert_eq!(
            [deliveries.recv().await, deliveries.recv().await],
            [
                Some(SwarmDelivery::Acknowledged {
                    target_session_id: "reviewer".into(),
                    message_id: post.entry.id.clone(),
                }),
                Some(SwarmDelivery::Acknowledged {
                    target_session_id: "observer".into(),
                    message_id: post.entry.id,
                }),
            ]
        );
    }

    #[tokio::test]
    async fn board_only_posts_signal_catalog_changes_without_peer_delivery() {
        let (_directory, _checkpoints, store, mut deliveries) = store();
        create_swarm(&store).await;

        store
            .post("leader", "Shared status update".into())
            .await
            .expect("board-only post");

        assert_eq!(deliveries.recv().await, Some(SwarmDelivery::Changed));
        assert!(matches!(
            deliveries.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn model_board_read_stays_valid_json_below_the_tool_output_cap() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        create_swarm(&store).await;
        store
            .post("leader", "\0".repeat(MAX_MESSAGE_BYTES))
            .await
            .expect("escape-heavy post");

        let output = SwarmBackend::read(&store, "leader")
            .await
            .expect("model board read");
        let output: serde_json::Value = serde_json::from_str(&output).expect("valid JSON output");

        assert!(serde_json::to_vec(&output).expect("encoded output").len() <= MAX_TOOL_READ_BYTES);
        assert_eq!(output["entries"][0]["body_truncated"], true);
        assert_eq!(output["has_older"], false);
    }

    #[tokio::test]
    async fn retention_never_evicts_pending_entries() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        let reviewer_handle = swarm
            .members
            .iter()
            .find(|member| member.session_id == "reviewer")
            .expect("reviewer")
            .handle
            .clone();
        let pending = store
            .post("leader", format!("Please check @{reviewer_handle}"))
            .await
            .expect("pending post");
        for sequence in 0..=MAX_ACKNOWLEDGED_ENTRIES {
            store
                .post("leader", format!("board update {sequence}"))
                .await
                .expect("board post");
        }

        let page = store
            .board_page(&swarm.id, None, MAX_PAGE_ENTRIES)
            .await
            .expect("board page");
        assert_eq!(page.entries.len(), MAX_ACKNOWLEDGED_ENTRIES);
        assert!(page.next_before_sequence.is_some());

        let pending_delivery = store
            .pending_deliveries("reviewer")
            .await
            .expect("pending delivery");
        assert_eq!(pending_delivery.len(), 1);
        assert_eq!(pending_delivery[0].entry.id, pending.entry.id);
    }

    #[tokio::test]
    async fn posting_backpressures_a_recipient_with_a_full_pending_queue() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        let reviewer_handle = swarm
            .members
            .iter()
            .find(|member| member.session_id == "reviewer")
            .expect("reviewer")
            .handle
            .clone();
        for sequence in 0..MAX_PENDING_DELIVERIES_PER_RECIPIENT {
            store
                .post("leader", format!("@{reviewer_handle} review {sequence}"))
                .await
                .expect("pending post within limit");
        }

        let error = store
            .post("leader", format!("@{reviewer_handle} one too many"))
            .await
            .expect_err("pending delivery limit");

        assert!(error.to_string().contains("pending swarm messages"));
        assert_eq!(
            store
                .pending_deliveries("reviewer")
                .await
                .expect("pending deliveries")
                .len(),
            MAX_PENDING_DELIVERIES_PER_RECIPIENT
        );
    }

    #[test]
    fn catalog_encoded_size_is_bounded_before_persistence_or_broadcast() {
        let now = unix_ms();
        let author = SwarmMember {
            session_id: "leader".into(),
            handle: "agent_leader".into(),
            joined_at_ms: now,
        };
        let board = (1..=64)
            .map(|sequence| BoardEntry {
                id: Uuid::new_v4().to_string(),
                sequence,
                created_at_ms: now,
                author: author.clone(),
                text: "\0".repeat(MAX_MESSAGE_BYTES),
                mentioned_recipient_session_ids: Vec::new(),
                pending_recipient_session_ids: Vec::new(),
            })
            .collect();
        let catalog = Catalog {
            swarms: BTreeMap::from([(
                Uuid::new_v4().to_string(),
                StoredSwarm {
                    title: "Bounded swarm".into(),
                    leader_session_id: author.session_id.clone(),
                    members: BTreeMap::from([(author.handle.clone(), author)]),
                    latest_sequence: 64,
                    board,
                    created_at_ms: now,
                    updated_at_ms: now,
                },
            )]),
        };

        assert!(
            validate_catalog(&catalog)
                .expect_err("oversized catalog")
                .to_string()
                .contains("encoded bytes")
        );
    }

    #[test]
    fn mention_parser_ignores_email_boundaries_and_deduplicates() {
        assert_eq!(
            mentioned_handles("@one mail@two @one; @three"),
            BTreeSet::from(["one".into(), "three".into()])
        );
    }
}
