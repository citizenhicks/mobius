//! Bot-owned durable swarm membership, collaboration limits, and message-board state.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use mobius::backend::checkpoint::CheckpointStore;
use mobius::middleware::bots::BotsBackend;
use mobius::protocol::MAX_MESSAGE_BYTES;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::sync::{Mutex, MutexGuard, OwnedMutexGuard, mpsc};
use uuid::Uuid;

use crate::bots::BotStore;
use crate::config::GatewayConfig;
use crate::wire::{
    RoutineRun, RoutineRunStatus, RoutineSchedule, SwarmMemberRecord, SwarmMessageRecord,
    SwarmRecord,
};
use crate::{Error, Result};

const STATE_SCOPE: &str = "gateway";
const STATE_KEY: &str = "bots.swarms.v4";
const USER_AUTHOR_ID: &str = "user";
const USER_HANDLE: &str = "user";
const MAX_HANDLE_BYTES: usize = 64;
const MAX_ID_BYTES: usize = 512;
const MAX_TITLE_BYTES: usize = 256;
const MAX_ACKNOWLEDGED_ENTRIES: usize = 256;
const MAX_PENDING_DELIVERIES_PER_RECIPIENT: usize = 256;
const MAX_PAGE_ENTRIES: usize = 256;
const MAX_SWARM_MEMBERS: usize = 100;
// ponytail: one 8 MiB catalog; split board storage and wire pages if real usage reaches it.
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOOL_READ_BYTES: usize = 32_000;
const MAX_TOOL_READ_TEXT_BYTES: usize = 4_000;
const MAX_REPLY_DEPTH: u8 = 3;
const MAX_TERMINAL_REPLY_DEPTH: u8 = MAX_REPLY_DEPTH + 1;

/// One Bot participating in a swarm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmMember {
    /// Stable gateway Bot identifier.
    pub bot_id: String,
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
    /// Bot identifier of the swarm leader.
    pub leader_bot_id: String,
    /// Current members ordered by handle.
    pub members: Vec<SwarmMember>,
    /// Latest board sequence assigned in this swarm.
    pub latest_sequence: u64,
    /// Time the swarm was created, in Unix milliseconds.
    pub created_at_ms: i64,
    /// Time its roster or board was last changed, in Unix milliseconds.
    pub updated_at_ms: i64,
}

/// A swarm snapshot resolved for one participating Bot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmSnapshot {
    /// The swarm and current roster.
    pub swarm: SwarmSummary,
    /// This Bot's current catalog handle.
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
    /// Conversation from which the author posted this entry.
    pub source_session_id: String,
    /// Stable source event when this entry projects a lifecycle event.
    pub source_event_id: Option<String>,
    /// Message text.
    pub text: String,
    /// Bot identifiers resolved from the entry's `@handle` mentions.
    pub mentioned_recipient_bot_ids: Vec<String>,
    /// Mentioned Bots which have not acknowledged delivery.
    pub pending_recipient_bot_ids: Vec<String>,
    /// Durable Swarm participant conversation assigned to each recipient.
    pub assigned_recipient_session_ids: BTreeMap<String, String>,
    /// Parent board message when this entry is a peer reply.
    pub in_reply_to_message_id: Option<String>,
    /// Number of peer-to-peer hops from the initiating post.
    pub reply_depth: u8,
}

/// The durable entry created by a post and the conversations to notify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmPost {
    /// Newly-created durable board entry.
    pub entry: BoardEntry,
    /// Resolved recipient Bots, ready for the gateway delivery bridge.
    pub resolved_recipient_bot_ids: Vec<String>,
}

/// One newest-first page of swarm board entries.
#[cfg(test)]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardPage {
    /// Newest-first entries in this page.
    pub entries: Vec<BoardEntry>,
    /// Sequence to pass as `before_sequence` for the next older page.
    pub next_before_sequence: Option<u64>,
}

/// One message awaiting delivery to a target Bot.
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

/// Terminal state projected from one hidden Bot conversation into Swarm Chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwarmRunOutcome {
    Succeeded { summary: String },
    Failed { message: String },
}

/// One durable user-attention request and its causal routing data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserAttention {
    pub(crate) entry: BoardEntry,
    pub(crate) leader_bot_id: String,
    pub(crate) root_source_session_id: String,
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) assigned_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BotSwarmRemoval {
    pub(crate) swarm_id: String,
    pub(crate) disbanded: bool,
}

struct Settlement {
    message_id: String,
    target_bot_id: String,
    pending_bot_id: Option<String>,
    user_attention_message_id: Option<String>,
}

/// Wake-up signal consumed by the gateway's swarm delivery worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwarmDelivery {
    /// Durable board contents changed and connected clients need a fresh catalog.
    Changed,
    /// Bot creation changed both authoritative catalogs.
    CatalogChanged,
    /// Gateway capacity changed and durable pending recipients should be retried.
    RetryPending,
    /// A target Bot has at least one durable board message awaiting delivery.
    Pending { target_bot_id: String },
    /// One board post explicitly asks for authenticated user attention.
    UserAttention { message_id: String },
    /// One user-attention message was durably recorded in its visible conversation.
    UserAttentionAcknowledged { message_id: String },
    /// A target Bot durably recorded one delivered peer message.
    Acknowledged {
        target_bot_id: String,
        message_id: String,
    },
    /// A target rejected an attempted delivery before accepting it into its queue.
    Rejected {
        target_bot_id: String,
        message_id: String,
    },
    /// A target consumed a queued message and may accept a previously rejected delivery.
    CapacityAvailable { target_bot_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
pub(crate) enum AcknowledgeOutcome {
    Acknowledged,
    AlreadyAcknowledged,
    MessageGone,
}

/// Cloneable access to the gateway's durable swarm catalog.
#[derive(Clone)]
pub struct SwarmStore {
    checkpoints: Arc<dyn CheckpointStore>,
    bots: Arc<BotStore>,
    gateway: Arc<StdMutex<GatewayConfig>>,
    state: Arc<Mutex<Option<Catalog>>>,
    delivery_gate: Arc<Mutex<()>>,
    deliveries: mpsc::UnboundedSender<SwarmDelivery>,
}

pub(crate) struct SwarmDeliveryClaim {
    store: SwarmStore,
    delivery: PendingDelivery,
    session_id: String,
    target_bot_id: String,
    gate: OwnedMutexGuard<()>,
}

impl SwarmDeliveryClaim {
    pub(crate) fn delivery(&self) -> &PendingDelivery {
        &self.delivery
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) async fn accept<T>(self, acceptance: impl Future<Output = T>) -> Result<Option<T>> {
        let Self {
            store,
            delivery,
            target_bot_id,
            gate,
            ..
        } = self;
        if !store
            .delivery_is_pending(&delivery.swarm_id, &delivery.entry.id, &target_bot_id)
            .await?
        {
            return Ok(None);
        }
        let output = acceptance.await;
        drop(gate);
        Ok(Some(output))
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    swarms: BTreeMap<String, StoredSwarm>,
    message_workspaces: BTreeMap<String, PathBuf>,
    pending_user_attention_message_ids: BTreeSet<String>,
    assigned_user_attention_session_ids: BTreeMap<String, String>,
    projected_routine_run_ids: BTreeSet<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSwarm {
    title: String,
    leader_bot_id: String,
    members: BTreeMap<String, StoredMember>,
    latest_sequence: u64,
    board: VecDeque<BoardEntry>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredMember {
    joined_at_ms: i64,
}

impl SwarmStore {
    /// Creates a lazily-loaded store and its single gateway delivery queue.
    #[must_use]
    pub(crate) fn new(
        checkpoints: Arc<dyn CheckpointStore>,
        bots: Arc<BotStore>,
        gateway: Arc<StdMutex<GatewayConfig>>,
    ) -> (Self, mpsc::UnboundedReceiver<SwarmDelivery>) {
        let (deliveries, receiver) = mpsc::unbounded_channel();
        (
            Self {
                checkpoints,
                bots,
                gateway,
                state: Arc::new(Mutex::new(None)),
                delivery_gate: Arc::new(Mutex::new(())),
                deliveries,
            },
            receiver,
        )
    }

    /// Returns every swarm ordered by its stable identifier.
    #[cfg(test)]
    pub async fn summaries(&self) -> Result<Vec<SwarmSummary>> {
        let state = self.lock_loaded().await?;
        state
            .as_ref()
            .expect("swarm catalog loaded")
            .swarms
            .iter()
            .map(|(id, swarm)| summary(&self.bots, id, swarm))
            .collect()
    }

    pub(crate) async fn records(&self) -> Result<Vec<SwarmRecord>> {
        let state = self.lock_loaded().await?;
        state
            .as_ref()
            .expect("swarm catalog loaded")
            .swarms
            .iter()
            .map(|(id, swarm)| {
                let members = current_members(&self.bots, swarm)?;
                let mut messages = swarm
                    .board
                    .iter()
                    .rev()
                    .take(MAX_PAGE_ENTRIES)
                    .map(|entry| SwarmMessageRecord {
                        id: entry.id.clone(),
                        sequence: entry.sequence,
                        author_bot_id: entry.author.bot_id.clone(),
                        author_handle: entry.author.handle.clone(),
                        source_session_id: entry.source_session_id.clone(),
                        text: entry.text.clone(),
                        created_at_ms: entry.created_at_ms,
                        in_reply_to_message_id: entry.in_reply_to_message_id.clone(),
                        reply_depth: entry.reply_depth,
                    })
                    .collect::<Vec<_>>();
                messages.reverse();
                Ok(SwarmRecord {
                    id: id.clone(),
                    title: swarm.title.clone(),
                    leader_bot_id: swarm.leader_bot_id.clone(),
                    members: members
                        .into_iter()
                        .map(|member| SwarmMemberRecord {
                            bot_id: member.bot_id,
                            handle: member.handle,
                        })
                        .collect(),
                    messages,
                    updated_at_ms: swarm.updated_at_ms,
                })
            })
            .collect()
    }

    /// Creates a named swarm from Bot-catalog identities.
    pub(crate) async fn create(
        &self,
        title: String,
        leader_bot_id: String,
        member_bot_ids: Vec<String>,
    ) -> Result<SwarmSummary> {
        validate_swarm_members(&leader_bot_id, &member_bot_ids)?;
        validate_title(&title)?;
        for bot_id in &member_bot_ids {
            self.bots.bot(bot_id)?;
        }

        let bots = Arc::clone(&self.bots);
        self.mutate(move |catalog| {
            for bot_id in &member_bot_ids {
                ensure_bot_available(catalog, bot_id)?;
            }
            let id = Uuid::new_v4();
            let now = unix_ms();
            let members = member_bot_ids
                .into_iter()
                .map(|bot_id| (bot_id, StoredMember { joined_at_ms: now }))
                .collect();
            let swarm = StoredSwarm {
                title,
                leader_bot_id,
                members,
                latest_sequence: 0,
                board: VecDeque::new(),
                created_at_ms: now,
                updated_at_ms: now,
            };
            let id = id.to_string();
            let summary = summary(&bots, &id, &swarm)?;
            catalog.swarms.insert(id, swarm);
            Ok(summary)
        })
        .await
    }

    /// Adds one Bot using its catalog-owned handle.
    pub(crate) async fn join(&self, swarm_id: &str, bot_id: String) -> Result<SwarmSummary> {
        validate_swarm_id(swarm_id)?;
        validate_bot_id(&bot_id)?;
        self.bots.bot(&bot_id)?;
        let swarm_id = swarm_id.to_owned();
        let bots = Arc::clone(&self.bots);
        self.mutate(move |catalog| {
            ensure_bot_available(catalog, &bot_id)?;
            let swarm = catalog
                .swarms
                .get_mut(&swarm_id)
                .ok_or_else(|| config(format!("unknown swarm `{swarm_id}`")))?;
            if swarm.members.len() >= MAX_SWARM_MEMBERS {
                return Err(config(format!(
                    "a swarm supports at most {MAX_SWARM_MEMBERS} members"
                )));
            }
            let now = unix_ms();
            swarm
                .members
                .insert(bot_id, StoredMember { joined_at_ms: now });
            swarm.updated_at_ms = now;
            summary(&bots, &swarm_id, swarm)
        })
        .await
    }

    /// Renames a swarm.
    pub(crate) async fn rename(&self, swarm_id: &str, title: String) -> Result<SwarmSummary> {
        validate_swarm_id(swarm_id)?;
        validate_title(&title)?;
        let swarm_id = swarm_id.to_owned();
        let bots = Arc::clone(&self.bots);
        self.mutate(move |catalog| {
            let swarm = catalog
                .swarms
                .get_mut(&swarm_id)
                .ok_or_else(|| config(format!("unknown swarm `{swarm_id}`")))?;
            swarm.title = title;
            swarm.updated_at_ms = unix_ms();
            summary(&bots, &swarm_id, swarm)
        })
        .await
    }

    /// Removes a non-leader Bot from a swarm.
    ///
    /// A leader must explicitly disband its swarm. A one-member roster remains valid.
    pub(crate) async fn leave(&self, swarm_id: &str, bot_id: &str) -> Result<SwarmSummary> {
        validate_swarm_id(swarm_id)?;
        validate_bot_id(bot_id)?;
        let _delivery = self.delivery_gate.lock().await;
        let swarm_id = swarm_id.to_owned();
        let bot_id = bot_id.to_owned();
        let acknowledged_target = bot_id.clone();
        let bots = Arc::clone(&self.bots);
        let (summary, acknowledged_messages) = self
            .mutate(move |catalog| {
                let swarm = catalog
                    .swarms
                    .get_mut(&swarm_id)
                    .ok_or_else(|| config(format!("unknown swarm `{swarm_id}`")))?;
                if swarm.leader_bot_id == bot_id {
                    return Err(config("swarm leader must disband instead of leaving"));
                }
                if swarm.members.remove(&bot_id).is_none() {
                    return Err(config(format!("Bot `{bot_id}` is not in this swarm")));
                }
                let mut acknowledged_messages = Vec::new();
                for entry in &mut swarm.board {
                    if entry
                        .pending_recipient_bot_ids
                        .iter()
                        .any(|pending| pending == &bot_id)
                    {
                        acknowledged_messages.push(entry.id.clone());
                    }
                    entry
                        .pending_recipient_bot_ids
                        .retain(|pending| pending != &bot_id);
                }
                swarm.updated_at_ms = unix_ms();
                Ok((summary(&bots, &swarm_id, swarm)?, acknowledged_messages))
            })
            .await?;
        for message_id in acknowledged_messages {
            self.notify_acknowledged(&message_id, &acknowledged_target);
        }
        Ok(summary)
    }

    /// Atomically removes a Bot's messages and membership during cascade deletion.
    pub(crate) async fn remove_bot(&self, bot_id: &str) -> Result<Option<BotSwarmRemoval>> {
        validate_bot_id(bot_id)?;
        let _delivery = self.delivery_gate.lock().await;
        let Some((removal, acknowledged)) = self.remove_bot_from_catalog(bot_id).await? else {
            return Ok(None);
        };
        for (message_id, target_bot_id) in acknowledged {
            self.notify_acknowledged(&message_id, &target_bot_id);
        }
        let _ = self.deliveries.send(SwarmDelivery::Changed);
        Ok(Some(removal))
    }

    pub(crate) async fn planned_bot_removal(
        &self,
        bot_id: &str,
    ) -> Result<Option<BotSwarmRemoval>> {
        validate_bot_id(bot_id)?;
        let state = self.lock_loaded().await?;
        Ok(state
            .as_ref()
            .expect("swarm catalog loaded")
            .swarms
            .iter()
            .find_map(|(swarm_id, swarm)| {
                swarm.members.contains_key(bot_id).then(|| BotSwarmRemoval {
                    swarm_id: swarm_id.clone(),
                    disbanded: swarm.leader_bot_id == bot_id,
                })
            }))
    }

    /// Permanently removes a swarm and its board.
    pub(crate) async fn disband(&self, swarm_id: &str) -> Result<()> {
        validate_swarm_id(swarm_id)?;
        let _delivery = self.delivery_gate.lock().await;
        let swarm_id = swarm_id.to_owned();
        let pending_deliveries = self
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
                            .pending_recipient_bot_ids
                            .iter()
                            .map(|target| (entry.id.clone(), target.clone()))
                    })
                    .collect::<BTreeSet<_>>();
                Ok(pending_deliveries)
            })
            .await?;
        for (message_id, target_bot_id) in pending_deliveries {
            self.notify_acknowledged(&message_id, &target_bot_id);
        }
        Ok(())
    }

    /// Resolves the swarm and handle for one participating Bot.
    pub async fn snapshot_for_bot(&self, bot_id: &str) -> Result<Option<SwarmSnapshot>> {
        validate_bot_id(bot_id)?;
        let state = self.lock_loaded().await?;
        let catalog = state.as_ref().expect("swarm catalog loaded");
        let Some((id, swarm)) = catalog
            .swarms
            .iter()
            .find(|(_, swarm)| swarm.members.contains_key(bot_id))
        else {
            return Ok(None);
        };
        let member = current_member(
            &self.bots,
            bot_id,
            swarm
                .members
                .get(bot_id)
                .expect("resolved swarm contains Bot"),
        )?;
        Ok(Some(SwarmSnapshot {
            swarm: summary(&self.bots, id, swarm)?,
            handle: member.handle,
        }))
    }

    /// Reports whether a swarm currently exists.
    pub(crate) async fn contains_swarm(&self, swarm_id: &str) -> Result<bool> {
        validate_swarm_id(swarm_id)?;
        let state = self.lock_loaded().await?;
        Ok(state
            .as_ref()
            .expect("swarm catalog loaded")
            .swarms
            .contains_key(swarm_id))
    }

    async fn spawn_bot_inner(
        &self,
        bot_id: &str,
        name: String,
        description: String,
    ) -> Result<String> {
        let snapshot = self
            .snapshot_for_bot(bot_id)
            .await?
            .ok_or_else(|| config("only a Swarm leader can create a Bot"))?;
        if snapshot.swarm.leader_bot_id != bot_id {
            return Err(config("only a Swarm leader can create a Bot"));
        }
        let defaults = self
            .gateway
            .lock()
            .map_err(|_| config("gateway configuration lock is poisoned"))?
            .bot_defaults
            .clone()
            .ok_or_else(|| config("configure Bot defaults before creating a Bot"))?;
        let bot = self.bots.create_bot(&name, &description, defaults.config)?;
        if let Err(error) = self.join(&snapshot.swarm.id, bot.id.clone()).await {
            return match self.bots.rollback_created_bot(&bot.id, bot.config.revision) {
                Ok(_) => Err(error),
                Err(rollback) => Err(config(format!(
                    "{error}; rolling back the new Bot failed: {rollback}"
                ))),
            };
        }
        let _ = self.deliveries.send(SwarmDelivery::Changed);
        let _ = self.deliveries.send(SwarmDelivery::CatalogChanged);
        Ok(serde_json::to_string(&serde_json::json!({
            "bot_id": bot.id,
            "handle": bot.handle,
            "name": bot.name,
            "swarm_id": snapshot.swarm.id,
        }))?)
    }

    async fn create_routine_inner(
        &self,
        bot_id: &str,
        bot_handle: Option<String>,
        workspace: &Path,
        instructions: String,
        schedule: RoutineSchedule,
        ends_at: Option<i64>,
    ) -> Result<String> {
        let bots = self.bots.bots()?;
        let caller = bots
            .iter()
            .find(|bot| bot.id == bot_id)
            .cloned()
            .ok_or_else(|| config(format!("unknown Bot `{bot_id}`")))?;
        let target_handle = bot_handle.as_deref().unwrap_or(&caller.handle);
        let target_bot_id = if target_handle == caller.handle {
            caller.id
        } else {
            {
                let state = self.lock_loaded().await?;
                let catalog = state.as_ref().expect("swarm catalog loaded");
                let swarm = catalog
                    .swarms
                    .values()
                    .find(|swarm| swarm.members.contains_key(bot_id))
                    .filter(|swarm| swarm.leader_bot_id == bot_id)
                    .ok_or_else(|| config("only a Swarm leader may schedule another Bot"))?;
                swarm
                    .members
                    .keys()
                    .find_map(|member_bot_id| {
                        bots.iter()
                            .find(|bot| bot.id == *member_bot_id && bot.handle == target_handle)
                            .map(|bot| bot.id.clone())
                    })
                    .ok_or_else(|| {
                        config(format!(
                            "Bot `@{target_handle}` is not a current Swarm member"
                        ))
                    })?
            }
        };
        let target = self.bots.bot(&target_bot_id)?;
        let routine =
            self.bots
                .create_routine(&target.id, workspace, &instructions, schedule, ends_at)?;
        Ok(serde_json::to_string(&serde_json::json!({
            "routine_id": routine.id,
            "bot_id": target.id,
            "bot_handle": target.handle,
            "enabled": routine.enabled,
        }))?)
    }

    /// Returns this Bot's roster in the bounded model-tool format.
    pub(crate) async fn tool_roster(&self, bot_id: &str) -> Result<String> {
        let snapshot = self
            .snapshot_for_bot(bot_id)
            .await?
            .ok_or_else(|| config("this Bot is not in a swarm"))?;
        Ok(serde_json::to_string(&snapshot)?)
    }

    /// Returns this Bot's recent board in the bounded model-tool format.
    pub(crate) async fn tool_read(&self, bot_id: &str) -> Result<String> {
        validate_bot_id(bot_id)?;
        let state = self.lock_loaded().await?;
        let catalog = state.as_ref().expect("swarm catalog loaded");
        let swarm = catalog
            .swarms
            .values()
            .find(|swarm| swarm.members.contains_key(bot_id))
            .ok_or_else(|| config("this Bot is not in a swarm"))?;
        bounded_board_json(swarm)
    }

    pub(crate) async fn swarm_chat_context(
        &self,
        bot_id: &str,
        session_id: &str,
    ) -> Result<Option<String>> {
        validate_bot_id(bot_id)?;
        validate_delivery_session_id(session_id)?;
        let state = self.lock_loaded().await?;
        let catalog = state.as_ref().expect("swarm catalog loaded");
        let Some(swarm) = catalog
            .swarms
            .values()
            .find(|swarm| swarm.members.contains_key(bot_id))
        else {
            return Ok(None);
        };
        if !swarm.board.iter().any(|entry| {
            entry
                .assigned_recipient_session_ids
                .get(bot_id)
                .is_some_and(|assigned| assigned == session_id)
                && entry
                    .pending_recipient_bot_ids
                    .iter()
                    .any(|pending| pending == bot_id)
        }) {
            return Ok(None);
        }
        bounded_board_json(swarm).map(Some)
    }

    /// Reports whether one addressed Bot may extend a peer reply chain.
    pub async fn can_reply(&self, bot_id: &str, message_id: &str) -> Result<bool> {
        validate_bot_id(bot_id)?;
        validate_message_id(message_id)?;
        let state = self.lock_loaded().await?;
        let Some(entry) = state
            .as_ref()
            .expect("swarm catalog loaded")
            .swarms
            .values()
            .find_map(|swarm| swarm.board.iter().find(|entry| entry.id == message_id))
        else {
            return Ok(false);
        };
        Ok(entry.reply_depth < MAX_REPLY_DEPTH
            && entry
                .mentioned_recipient_bot_ids
                .iter()
                .any(|recipient| recipient == bot_id))
    }

    /// Posts one durable board entry and resolves all `@handle` recipients.
    pub async fn post(
        &self,
        sender_bot_id: &str,
        source_session_id: &str,
        text: String,
        in_reply_to_message_id: Option<String>,
    ) -> Result<SwarmPost> {
        validate_bot_id(sender_bot_id)?;
        validate_session_id(source_session_id)?;
        validate_message(&text)?;
        if let Some(message_id) = in_reply_to_message_id.as_deref() {
            validate_message_id(message_id)?;
        }
        let sender_bot_id = sender_bot_id.to_owned();
        let source_session_id = source_session_id.to_owned();
        let bots = Arc::clone(&self.bots);
        let (post, needs_user_attention) = self
            .mutate(move |catalog| {
                let swarm_id = swarm_id_for_bot(catalog, &sender_bot_id)
                    .ok_or_else(|| config(format!("Bot `{sender_bot_id}` is not in a swarm")))?;
                let reply_depth = match in_reply_to_message_id.as_deref() {
                    Some(message_id) => {
                        let parent = catalog
                            .swarms
                            .get(&swarm_id)
                            .and_then(|swarm| {
                                swarm.board.iter().find(|entry| entry.id == message_id)
                            })
                            .ok_or_else(|| {
                                config(format!("unknown swarm message `{message_id}`"))
                            })?;
                        if !parent
                            .mentioned_recipient_bot_ids
                            .iter()
                            .any(|recipient| recipient == &sender_bot_id)
                        {
                            return Err(config(format!(
                                "Bot `{sender_bot_id}` is not a recipient of message `{message_id}`"
                            )));
                        }
                        parent
                            .reply_depth
                            .checked_add(1)
                            .filter(|depth| *depth <= MAX_REPLY_DEPTH)
                            .ok_or_else(|| {
                                config(format!(
                                    "swarm reply chain reached its {MAX_REPLY_DEPTH}-hop limit"
                                ))
                            })?
                    }
                    None => 0,
                };
                let swarm = catalog
                    .swarms
                    .get_mut(&swarm_id)
                    .expect("resolved swarm exists");
                let members = current_members(&bots, swarm)?;
                let author = members
                    .iter()
                    .find(|member| member.bot_id == sender_bot_id)
                    .cloned()
                    .expect("resolved swarm contains sender");
                let roster = members
                    .into_iter()
                    .map(|member| (member.handle, member.bot_id))
                    .collect::<BTreeMap<_, _>>();

                let handles = mentioned_handles(&text);
                let unknown = handles
                    .iter()
                    .filter(|handle| handle.as_str() != USER_HANDLE && !roster.contains_key(*handle))
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

                let needs_user_attention = handles.contains(USER_HANDLE);
                let mut recipients = handles
                    .iter()
                    .filter(|handle| handle.as_str() != USER_HANDLE)
                    .map(|handle| {
                        roster
                            .get(handle)
                            .expect("mentions validated against roster")
                            .clone()
                    })
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                if needs_user_attention
                    && sender_bot_id != swarm.leader_bot_id
                    && !recipients.contains(&swarm.leader_bot_id)
                {
                    recipients.push(swarm.leader_bot_id.clone());
                    recipients.sort();
                }
                if recipients
                    .iter()
                    .any(|bot| bot == &sender_bot_id)
                {
                    return Err(config("a swarm message cannot mention its author"));
                }
                if let Some(recipient) = recipients.iter().find(|recipient| {
                    swarm
                        .board
                        .iter()
                        .filter(|entry| {
                            entry
                                .pending_recipient_bot_ids
                                .iter()
                                .any(|pending| pending == *recipient)
                        })
                        .count()
                        >= MAX_PENDING_DELIVERIES_PER_RECIPIENT
                }) {
                    return Err(config(format!(
                        "Bot `{recipient}` has {MAX_PENDING_DELIVERIES_PER_RECIPIENT} pending swarm messages"
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
                    source_session_id,
                    source_event_id: None,
                    text,
                    mentioned_recipient_bot_ids: recipients.clone(),
                    pending_recipient_bot_ids: recipients.clone(),
                    assigned_recipient_session_ids: BTreeMap::new(),
                    in_reply_to_message_id,
                    reply_depth,
                };
                swarm.latest_sequence = sequence;
                swarm.updated_at_ms = entry.created_at_ms;
                swarm.board.push_back(entry.clone());
                if needs_user_attention {
                    catalog
                        .pending_user_attention_message_ids
                        .insert(entry.id.clone());
                }
                Ok((
                    SwarmPost {
                        entry,
                        resolved_recipient_bot_ids: recipients,
                    },
                    needs_user_attention,
                ))
            })
            .await?;
        let _ = self.deliveries.send(SwarmDelivery::Changed);
        for target_bot_id in &post.resolved_recipient_bot_ids {
            self.notify_pending(target_bot_id);
        }
        if needs_user_attention {
            let _ = self.deliveries.send(SwarmDelivery::UserAttention {
                message_id: post.entry.id.clone(),
            });
        }
        Ok(post)
    }

    /// Posts an authenticated human message from an explicitly validated workspace.
    pub(crate) async fn post_user(
        &self,
        swarm_id: &str,
        workspace: PathBuf,
        text: String,
    ) -> Result<SwarmPost> {
        validate_swarm_id(swarm_id)?;
        validate_message(&text)?;
        if !workspace.is_absolute() {
            return Err(config("swarm message workspace must be absolute"));
        }
        let swarm_id = swarm_id.to_owned();
        let bots = Arc::clone(&self.bots);
        let post = self
            .mutate(move |catalog| {
                let swarm = catalog
                    .swarms
                    .get_mut(&swarm_id)
                    .ok_or_else(|| config(format!("unknown swarm `{swarm_id}`")))?;
                let members = current_members(&bots, swarm)?;
                let roster = members
                    .into_iter()
                    .map(|member| (member.handle, member.bot_id))
                    .collect::<BTreeMap<_, _>>();
                let handles = mentioned_handles(&text);
                if handles.contains(USER_HANDLE) {
                    return Err(config("a human swarm message cannot mention @user"));
                }
                let unknown = handles
                    .iter()
                    .filter(|handle| !roster.contains_key(*handle))
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
                let recipients = if handles.is_empty() {
                    vec![swarm.leader_bot_id.clone()]
                } else {
                    handles
                        .iter()
                        .map(|handle| {
                            roster
                                .get(handle)
                                .expect("mentions validated against roster")
                                .clone()
                        })
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>()
                };
                if let Some(recipient) = recipients.iter().find(|recipient| {
                    swarm
                        .board
                        .iter()
                        .filter(|entry| {
                            entry
                                .pending_recipient_bot_ids
                                .iter()
                                .any(|pending| pending == *recipient)
                        })
                        .count()
                        >= MAX_PENDING_DELIVERIES_PER_RECIPIENT
                }) {
                    return Err(config(format!(
                        "Bot `{recipient}` has {MAX_PENDING_DELIVERIES_PER_RECIPIENT} pending swarm messages"
                    )));
                }
                let sequence = swarm
                    .latest_sequence
                    .checked_add(1)
                    .ok_or_else(|| config("swarm board sequence exhausted"))?;
                let id = Uuid::new_v4().to_string();
                let now = unix_ms();
                let entry = BoardEntry {
                    id: id.clone(),
                    sequence,
                    created_at_ms: now,
                    author: SwarmMember {
                        bot_id: USER_AUTHOR_ID.into(),
                        handle: USER_HANDLE.into(),
                        joined_at_ms: now,
                    },
                    source_session_id: format!("swarm-user-{id}"),
                    source_event_id: None,
                    text,
                    mentioned_recipient_bot_ids: recipients.clone(),
                    pending_recipient_bot_ids: recipients.clone(),
                    assigned_recipient_session_ids: BTreeMap::new(),
                    in_reply_to_message_id: None,
                    reply_depth: 0,
                };
                swarm.latest_sequence = sequence;
                swarm.updated_at_ms = now;
                swarm.board.push_back(entry.clone());
                catalog.message_workspaces.insert(id, workspace);
                Ok(SwarmPost {
                    entry,
                    resolved_recipient_bot_ids: recipients,
                })
            })
            .await?;
        let _ = self.deliveries.send(SwarmDelivery::Changed);
        for target_bot_id in &post.resolved_recipient_bot_ids {
            self.notify_pending(target_bot_id);
        }
        Ok(post)
    }

    /// Loads one newest-first board page before an optional sequence cursor.
    #[cfg(test)]
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

    /// Returns messages still awaiting one target Bot's acknowledgement.
    #[cfg(test)]
    pub async fn pending_deliveries(&self, target_bot_id: &str) -> Result<Vec<PendingDelivery>> {
        validate_bot_id(target_bot_id)?;
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
                            .pending_recipient_bot_ids
                            .iter()
                            .any(|pending| pending == target_bot_id)
                    })
                    .map(|entry| PendingDelivery {
                        swarm_id: swarm_id.clone(),
                        swarm_title: swarm.title.clone(),
                        entry: entry.clone(),
                    })
            })
            .collect())
    }

    /// Claims the next eligible delivery until its target queue accepts or rejects it.
    pub(crate) async fn claim_next_delivery(
        &self,
        target_bot_id: &str,
    ) -> Result<Option<SwarmDeliveryClaim>> {
        validate_bot_id(target_bot_id)?;
        let gate = Arc::clone(&self.delivery_gate).lock_owned().await;
        let target_bot_id = target_bot_id.to_owned();
        let claimed_target_bot_id = target_bot_id.clone();
        let Some((delivery, session_id)) = self
            .mutate_if_some(move |catalog| {
                let Catalog { swarms, .. } = catalog;
                for (swarm_id, swarm) in swarms {
                    if !swarm.members.contains_key(&target_bot_id) {
                        continue;
                    }
                    let Some(entry) = swarm.board.iter_mut().find(|entry| {
                        entry
                            .pending_recipient_bot_ids
                            .iter()
                            .any(|pending| pending == &target_bot_id)
                    }) else {
                        continue;
                    };
                    let session_id = entry
                        .assigned_recipient_session_ids
                        .entry(target_bot_id.clone())
                        .or_insert_with(|| participant_session_id(swarm_id, &target_bot_id))
                        .clone();
                    return Ok(Some((
                        PendingDelivery {
                            swarm_id: swarm_id.clone(),
                            swarm_title: swarm.title.clone(),
                            entry: entry.clone(),
                        },
                        session_id,
                    )));
                }
                Ok(None)
            })
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(SwarmDeliveryClaim {
            store: self.clone(),
            delivery,
            session_id,
            target_bot_id: claimed_target_bot_id,
            gate,
        }))
    }

    /// Reports whether any unacknowledged message depends on one of these source chats.
    pub(crate) async fn has_pending_source_sessions(&self, session_ids: &[String]) -> Result<bool> {
        for session_id in session_ids {
            validate_session_id(session_id)?;
        }
        let session_ids = session_ids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let state = self.lock_loaded().await?;
        let catalog = state.as_ref().expect("swarm catalog loaded");
        if catalog
            .assigned_user_attention_session_ids
            .values()
            .any(|assigned| session_ids.contains(assigned.as_str()))
        {
            return Ok(true);
        }
        for message_id in &catalog.pending_user_attention_message_ids {
            let attention = user_attention(catalog, message_id)?;
            if session_ids.contains(attention.root_source_session_id.as_str()) {
                return Ok(true);
            }
        }
        Ok(catalog
            .swarms
            .values()
            .flat_map(|swarm| &swarm.board)
            .any(|entry| {
                !entry.pending_recipient_bot_ids.is_empty()
                    && session_ids.contains(entry.source_session_id.as_str())
            }))
    }

    /// Returns each Bot with at least one durable pending mention.
    pub(crate) async fn pending_recipient_bot_ids(&self) -> Result<Vec<String>> {
        let state = self.lock_loaded().await?;
        Ok(state
            .as_ref()
            .expect("swarm catalog loaded")
            .swarms
            .values()
            .flat_map(|swarm| &swarm.board)
            .flat_map(|entry| &entry.pending_recipient_bot_ids)
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }

    pub(crate) fn notify_pending(&self, target_bot_id: &str) {
        let _ = self.deliveries.send(SwarmDelivery::Pending {
            target_bot_id: target_bot_id.to_owned(),
        });
    }

    pub(crate) fn notify_user_attention(&self, message_id: &str) {
        let _ = self.deliveries.send(SwarmDelivery::UserAttention {
            message_id: message_id.to_owned(),
        });
    }

    pub(crate) fn retry_pending(&self) {
        let _ = self.deliveries.send(SwarmDelivery::RetryPending);
    }

    pub(crate) fn notify_acknowledged(&self, message_id: &str, target_bot_id: &str) {
        let _ = self.deliveries.send(SwarmDelivery::Acknowledged {
            target_bot_id: target_bot_id.to_owned(),
            message_id: message_id.to_owned(),
        });
    }

    pub(crate) fn notify_rejected(&self, message_id: &str, target_bot_id: &str) {
        let _ = self.deliveries.send(SwarmDelivery::Rejected {
            target_bot_id: target_bot_id.to_owned(),
            message_id: message_id.to_owned(),
        });
    }

    pub(crate) fn notify_capacity_available(&self, target_bot_id: &str) {
        let _ = self.deliveries.send(SwarmDelivery::CapacityAvailable {
            target_bot_id: target_bot_id.to_owned(),
        });
    }

    /// Acknowledges one target's message delivery.
    #[cfg(test)]
    pub(crate) async fn acknowledge(
        &self,
        message_id: &str,
        target_bot_id: &str,
    ) -> Result<AcknowledgeOutcome> {
        validate_message_id(message_id)?;
        validate_bot_id(target_bot_id)?;
        let message_id = message_id.to_owned();
        let target_bot_id = target_bot_id.to_owned();
        let acknowledged_message = message_id.clone();
        let acknowledged_target = target_bot_id.clone();
        let outcome = self
            .mutate_if_some(move |catalog| {
                let Some(entry) = catalog
                    .swarms
                    .values_mut()
                    .find_map(|swarm| swarm.board.iter_mut().find(|entry| entry.id == message_id))
                else {
                    return Ok(None);
                };
                if !entry
                    .mentioned_recipient_bot_ids
                    .iter()
                    .any(|recipient| recipient == &target_bot_id)
                {
                    return Err(config(format!(
                        "Bot `{target_bot_id}` is not a recipient of message `{message_id}`"
                    )));
                }
                let was_pending = entry
                    .pending_recipient_bot_ids
                    .iter()
                    .any(|pending| pending == &target_bot_id);
                entry
                    .pending_recipient_bot_ids
                    .retain(|pending| pending != &target_bot_id);
                Ok(Some(if was_pending {
                    AcknowledgeOutcome::Acknowledged
                } else {
                    AcknowledgeOutcome::AlreadyAcknowledged
                }))
            })
            .await?
            .unwrap_or(AcknowledgeOutcome::MessageGone);
        self.notify_acknowledged(&acknowledged_message, &acknowledged_target);
        Ok(outcome)
    }

    /// Atomically completes one hidden mention and appends its terminal board projection.
    pub(crate) async fn settle_delivery(
        &self,
        message_id: &str,
        session_id: &str,
        target_bot_id: &str,
        outcome: SwarmRunOutcome,
    ) -> Result<bool> {
        validate_delivery_session_id(session_id)?;
        validate_bot_id(target_bot_id)?;
        let message_id = message_id.to_owned();
        let session_id = session_id.to_owned();
        let target_bot_id = target_bot_id.to_owned();
        let bots = Arc::clone(&self.bots);
        let Some(settlement) = self
            .mutate_if_some(move |catalog| {
                let Some(swarm_id) = catalog.swarms.iter().find_map(|(id, swarm)| {
                    swarm
                        .board
                        .iter()
                        .any(|entry| {
                            entry.id == message_id
                                && entry
                                    .assigned_recipient_session_ids
                                    .get(&target_bot_id)
                                    .is_some_and(|assigned| assigned == &session_id)
                                && entry
                                    .pending_recipient_bot_ids
                                    .iter()
                                    .any(|pending| pending == &target_bot_id)
                        })
                        .then(|| id.clone())
                }) else {
                    return Ok(None);
                };
                let swarm = catalog
                    .swarms
                    .get_mut(&swarm_id)
                    .expect("resolved swarm exists");
                let source = swarm
                    .board
                    .iter()
                    .find(|entry| entry.id == message_id)
                    .cloned()
                    .expect("resolved swarm message exists");
                let author = current_member(
                    &bots,
                    &target_bot_id,
                    swarm
                        .members
                        .get(&target_bot_id)
                        .expect("pending delivery target remains a member"),
                )?;
                let leader_bot_id = swarm.leader_bot_id.clone();
                let wake_leader =
                    target_bot_id != leader_bot_id && source.reply_depth < MAX_REPLY_DEPTH;
                let text = outcome_text(&outcome);
                let recipients = if wake_leader {
                    vec![leader_bot_id]
                } else {
                    Vec::new()
                };
                let reply_depth = source
                    .reply_depth
                    .checked_add(1)
                    .filter(|depth| *depth <= MAX_TERMINAL_REPLY_DEPTH)
                    .ok_or_else(|| config("swarm terminal reply depth is invalid"))?;
                let sequence = swarm
                    .latest_sequence
                    .checked_add(1)
                    .ok_or_else(|| config("swarm board sequence exhausted"))?;
                let now = unix_ms();
                let entry = BoardEntry {
                    id: Uuid::new_v4().to_string(),
                    sequence,
                    created_at_ms: now,
                    author,
                    source_session_id: session_id,
                    source_event_id: None,
                    text,
                    mentioned_recipient_bot_ids: recipients.clone(),
                    pending_recipient_bot_ids: recipients.clone(),
                    assigned_recipient_session_ids: BTreeMap::new(),
                    in_reply_to_message_id: Some(source.id.clone()),
                    reply_depth,
                };
                let original = swarm
                    .board
                    .iter_mut()
                    .find(|entry| entry.id == message_id)
                    .expect("resolved swarm message exists");
                original
                    .pending_recipient_bot_ids
                    .retain(|pending| pending != &target_bot_id);
                swarm.latest_sequence = sequence;
                swarm.updated_at_ms = now;
                swarm.board.push_back(entry.clone());
                Ok(Some(Settlement {
                    message_id,
                    target_bot_id,
                    pending_bot_id: recipients.into_iter().next(),
                    user_attention_message_id: None,
                }))
            })
            .await?
        else {
            return Ok(false);
        };
        self.finish_settlement(settlement);
        Ok(true)
    }

    /// Idempotently appends one terminal routine outcome to the owning Swarm Chat.
    pub(crate) async fn project_routine_outcome(
        &self,
        run: &RoutineRun,
        summary: Option<String>,
        workspace: Option<PathBuf>,
    ) -> Result<bool> {
        validate_message_id(&run.id)?;
        let run = run.clone();
        let bots = Arc::clone(&self.bots);
        let projection = self
            .mutate(move |catalog| {
                if !catalog.projected_routine_run_ids.insert(run.id.clone()) {
                    return Ok(None);
                }
                let Some(swarm_id) = swarm_id_for_bot(catalog, &run.bot_id) else {
                    return Ok(None);
                };
                let swarm = catalog
                    .swarms
                    .get_mut(&swarm_id)
                    .expect("resolved swarm exists");
                let author = current_member(
                    &bots,
                    &run.bot_id,
                    swarm
                        .members
                        .get(&run.bot_id)
                        .expect("resolved swarm contains routine Bot"),
                )?;
                let leader = current_member(
                    &bots,
                    &swarm.leader_bot_id,
                    swarm
                        .members
                        .get(&swarm.leader_bot_id)
                        .expect("swarm leader remains a member"),
                )?;
                let wake_leader = run.bot_id != leader.bot_id;
                let needs_user_attention = run.status == RoutineRunStatus::Failed && !wake_leader;
                let detail = summary.or_else(|| run.message.clone());
                let text = routine_outcome_text(
                    &run,
                    detail.as_deref(),
                    wake_leader.then_some(leader.handle.as_str()),
                    needs_user_attention,
                );
                let recipients = if wake_leader {
                    vec![leader.bot_id.clone()]
                } else {
                    Vec::new()
                };
                let sequence = swarm
                    .latest_sequence
                    .checked_add(1)
                    .ok_or_else(|| config("swarm board sequence exhausted"))?;
                let now = unix_ms();
                let entry = BoardEntry {
                    id: Uuid::new_v4().to_string(),
                    sequence,
                    created_at_ms: now,
                    author,
                    source_session_id: run
                        .session_id
                        .clone()
                        .unwrap_or_else(|| format!("routine-run-{}", run.id)),
                    source_event_id: None,
                    text,
                    mentioned_recipient_bot_ids: recipients.clone(),
                    pending_recipient_bot_ids: recipients.clone(),
                    assigned_recipient_session_ids: BTreeMap::new(),
                    in_reply_to_message_id: None,
                    reply_depth: 0,
                };
                swarm.latest_sequence = sequence;
                swarm.updated_at_ms = now;
                swarm.board.push_back(entry.clone());
                if let Some(workspace) = workspace {
                    catalog
                        .message_workspaces
                        .insert(entry.id.clone(), workspace);
                }
                if needs_user_attention {
                    catalog
                        .pending_user_attention_message_ids
                        .insert(entry.id.clone());
                }
                Ok(Some(Settlement {
                    message_id: entry.id.clone(),
                    target_bot_id: run.bot_id,
                    pending_bot_id: recipients.into_iter().next(),
                    user_attention_message_id: needs_user_attention.then_some(entry.id),
                }))
            })
            .await?;
        let Some(projection) = projection else {
            return Ok(false);
        };
        let _ = self.deliveries.send(SwarmDelivery::Changed);
        if let Some(bot_id) = projection.pending_bot_id {
            self.notify_pending(&bot_id);
        }
        if let Some(message_id) = projection.user_attention_message_id {
            let _ = self
                .deliveries
                .send(SwarmDelivery::UserAttention { message_id });
        }
        Ok(true)
    }

    /// Idempotently projects one hidden conversation approval pause to Swarm Chat.
    pub(crate) async fn project_attention(
        &self,
        bot_id: &str,
        source_session_id: &str,
        source_message_id: Option<&str>,
        source_event_id: &str,
        message: &str,
    ) -> Result<bool> {
        validate_bot_id(bot_id)?;
        validate_session_id(source_session_id)?;
        if let Some(message_id) = source_message_id {
            validate_message_id(message_id)?;
        }
        validate_message_id(source_event_id)?;
        let bot_id = bot_id.to_owned();
        let source_session_id = source_session_id.to_owned();
        let source_message_id = source_message_id.map(str::to_owned);
        let source_event_id = source_event_id.to_owned();
        let message = message.to_owned();
        let bots = Arc::clone(&self.bots);
        let projection = self
            .mutate(move |catalog| {
                let Some(swarm_id) = swarm_id_for_bot(catalog, &bot_id) else {
                    return Ok(None);
                };
                let board = &catalog
                    .swarms
                    .get(&swarm_id)
                    .expect("resolved swarm exists")
                    .board;
                let participant_has_pending = board.iter().any(|entry| {
                    entry
                        .assigned_recipient_session_ids
                        .get(&bot_id)
                        .is_some_and(|assigned| assigned == &source_session_id)
                        && entry
                            .pending_recipient_bot_ids
                            .iter()
                            .any(|pending| pending == &bot_id)
                });
                let parent = source_message_id.as_deref().and_then(|message_id| {
                    board
                        .iter()
                        .find(|entry| {
                            entry.id == message_id
                                && entry
                                    .assigned_recipient_session_ids
                                    .get(&bot_id)
                                    .is_some_and(|assigned| assigned == &source_session_id)
                                && entry
                                    .pending_recipient_bot_ids
                                    .iter()
                                    .any(|pending| pending == &bot_id)
                        })
                        .cloned()
                });
                if participant_has_pending && parent.is_none() {
                    return Ok(None);
                }
                let swarm = catalog
                    .swarms
                    .get_mut(&swarm_id)
                    .expect("resolved swarm exists");
                if swarm.board.iter().any(|entry| {
                    entry.source_session_id == source_session_id
                        && entry.source_event_id.as_deref() == Some(source_event_id.as_str())
                }) {
                    return Ok(None);
                }
                let author = current_member(
                    &bots,
                    &bot_id,
                    swarm
                        .members
                        .get(&bot_id)
                        .expect("resolved swarm contains Bot"),
                )?;
                let leader = current_member(
                    &bots,
                    &swarm.leader_bot_id,
                    swarm
                        .members
                        .get(&swarm.leader_bot_id)
                        .expect("swarm leader remains a member"),
                )?;
                let can_route_bot = parent
                    .as_ref()
                    .is_none_or(|entry| entry.reply_depth < MAX_REPLY_DEPTH);
                let wake_leader = bot_id != leader.bot_id && can_route_bot;
                let recipients = if wake_leader {
                    vec![leader.bot_id.clone()]
                } else {
                    Vec::new()
                };
                let prefix = if wake_leader {
                    format!("@{} ", leader.handle)
                } else {
                    String::new()
                };
                let (in_reply_to_message_id, reply_depth) = parent.map_or_else(
                    || Ok((None, 0)),
                    |parent| {
                        parent
                            .reply_depth
                            .checked_add(1)
                            .filter(|depth| *depth <= MAX_TERMINAL_REPLY_DEPTH)
                            .map(|depth| (Some(parent.id), depth))
                            .ok_or_else(|| config("swarm attention reply depth is invalid"))
                    },
                )?;
                let sequence = swarm
                    .latest_sequence
                    .checked_add(1)
                    .ok_or_else(|| config("swarm board sequence exhausted"))?;
                let now = unix_ms();
                let entry = BoardEntry {
                    id: Uuid::new_v4().to_string(),
                    sequence,
                    created_at_ms: now,
                    author,
                    source_session_id,
                    source_event_id: Some(source_event_id),
                    text: bounded_message(&format!(
                        "{prefix}Needs user attention: {} @user",
                        neutralize_mentions(&message)
                    )),
                    mentioned_recipient_bot_ids: recipients.clone(),
                    pending_recipient_bot_ids: recipients.clone(),
                    assigned_recipient_session_ids: BTreeMap::new(),
                    in_reply_to_message_id,
                    reply_depth,
                };
                swarm.latest_sequence = sequence;
                swarm.updated_at_ms = now;
                swarm.board.push_back(entry.clone());
                catalog
                    .pending_user_attention_message_ids
                    .insert(entry.id.clone());
                Ok(Some((entry.id, recipients.into_iter().next())))
            })
            .await?;
        let Some((message_id, pending_bot_id)) = projection else {
            return Ok(false);
        };
        let _ = self.deliveries.send(SwarmDelivery::Changed);
        if let Some(bot_id) = pending_bot_id {
            self.notify_pending(&bot_id);
        }
        let _ = self
            .deliveries
            .send(SwarmDelivery::UserAttention { message_id });
        Ok(true)
    }

    /// Returns all durable attention requests awaiting delivery to the user.
    pub(crate) async fn pending_user_attention(&self) -> Result<Vec<UserAttention>> {
        let state = self.lock_loaded().await?;
        let catalog = state.as_ref().expect("swarm catalog loaded");
        catalog
            .pending_user_attention_message_ids
            .iter()
            .map(|message_id| user_attention(catalog, message_id))
            .collect()
    }

    /// Durably assigns an exact visible conversation before user-attention delivery.
    pub(crate) async fn assign_user_attention(
        &self,
        message_id: &str,
        proposed_session_id: &str,
    ) -> Result<UserAttention> {
        validate_message_id(message_id)?;
        validate_delivery_session_id(proposed_session_id)?;
        let message_id = message_id.to_owned();
        let proposed_session_id = proposed_session_id.to_owned();
        self.mutate(move |catalog| {
            if !catalog
                .pending_user_attention_message_ids
                .contains(&message_id)
            {
                return Err(config(format!(
                    "swarm message `{message_id}` is not awaiting user attention"
                )));
            }
            catalog
                .assigned_user_attention_session_ids
                .entry(message_id.clone())
                .or_insert(proposed_session_id);
            user_attention(catalog, &message_id)
        })
        .await
    }

    pub(crate) async fn acknowledge_user_attention(&self, message_id: &str) -> Result<bool> {
        validate_message_id(message_id)?;
        let message_id = message_id.to_owned();
        let Some(message_id) = self
            .mutate_if_some(move |catalog| {
                if !catalog
                    .pending_user_attention_message_ids
                    .contains(&message_id)
                {
                    return Ok(None);
                }
                catalog
                    .assigned_user_attention_session_ids
                    .remove(&message_id);
                catalog
                    .pending_user_attention_message_ids
                    .remove(&message_id);
                Ok(Some(message_id))
            })
            .await?
        else {
            return Ok(false);
        };
        let _ = self
            .deliveries
            .send(SwarmDelivery::UserAttentionAcknowledged { message_id });
        Ok(true)
    }

    fn finish_settlement(&self, settlement: Settlement) {
        let _ = self.deliveries.send(SwarmDelivery::Changed);
        self.notify_acknowledged(&settlement.message_id, &settlement.target_bot_id);
        if let Some(bot_id) = settlement.pending_bot_id {
            self.notify_pending(&bot_id);
        }
        if let Some(message_id) = settlement.user_attention_message_id {
            let _ = self
                .deliveries
                .send(SwarmDelivery::UserAttention { message_id });
        }
    }

    async fn delivery_is_pending(
        &self,
        swarm_id: &str,
        message_id: &str,
        target_bot_id: &str,
    ) -> Result<bool> {
        let state = self.lock_loaded().await?;
        let Some(swarm) = state
            .as_ref()
            .expect("swarm catalog loaded")
            .swarms
            .get(swarm_id)
        else {
            return Ok(false);
        };
        Ok(swarm.members.contains_key(target_bot_id)
            && swarm.board.iter().any(|entry| {
                entry.id == message_id
                    && entry
                        .pending_recipient_bot_ids
                        .iter()
                        .any(|pending| pending == target_bot_id)
            }))
    }

    async fn remove_bot_from_catalog(
        &self,
        bot_id: &str,
    ) -> Result<Option<(BotSwarmRemoval, BTreeSet<(String, String)>)>> {
        let mut state = self.lock_loaded_for_bot_removal().await?;
        let mut candidate = state.as_ref().expect("swarm catalog loaded").clone();
        let Some(output) = remove_bot_membership(&mut candidate, bot_id) else {
            validate_bot_references(&self.bots, &candidate)?;
            return Ok(None);
        };
        let live_routine_run_ids = self
            .bots
            .history(None)?
            .into_iter()
            .map(|run| run.id)
            .collect::<BTreeSet<_>>();
        prune_catalog(&mut candidate, &live_routine_run_ids);
        validate_catalog(&candidate)?;
        validate_bot_references(&self.bots, &candidate)?;
        self.checkpoints
            .save_state(STATE_SCOPE, STATE_KEY, &serde_json::to_value(&candidate)?)
            .await?;
        *state = Some(candidate);
        Ok(Some(output))
    }

    async fn mutate<T>(&self, mutation: impl FnOnce(&mut Catalog) -> Result<T>) -> Result<T> {
        Ok(self
            .mutate_if_some(|catalog| mutation(catalog).map(Some))
            .await?
            .expect("required mutation returns a value"))
    }

    async fn mutate_if_some<T>(
        &self,
        mutation: impl FnOnce(&mut Catalog) -> Result<Option<T>>,
    ) -> Result<Option<T>> {
        let mut state = self.lock_loaded().await?;
        let mut candidate = state.as_ref().expect("swarm catalog loaded").clone();
        let Some(output) = mutation(&mut candidate)? else {
            return Ok(None);
        };
        let live_routine_run_ids = self
            .bots
            .history(None)?
            .into_iter()
            .map(|run| run.id)
            .collect::<BTreeSet<_>>();
        prune_catalog(&mut candidate, &live_routine_run_ids);
        validate_catalog(&candidate)?;
        self.checkpoints
            .save_state(STATE_SCOPE, STATE_KEY, &serde_json::to_value(&candidate)?)
            .await?;
        *state = Some(candidate);
        Ok(Some(output))
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
            validate_bot_references(&self.bots, &catalog)?;
            *state = Some(catalog);
        }
        Ok(state)
    }

    async fn lock_loaded_for_bot_removal(&self) -> Result<MutexGuard<'_, Option<Catalog>>> {
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

fn remove_bot_membership(
    catalog: &mut Catalog,
    bot_id: &str,
) -> Option<(BotSwarmRemoval, BTreeSet<(String, String)>)> {
    let (swarm_id, leader) = catalog.swarms.iter().find_map(|(id, swarm)| {
        swarm
            .members
            .contains_key(bot_id)
            .then(|| (id.clone(), swarm.leader_bot_id == bot_id))
    })?;
    if leader {
        let swarm = catalog
            .swarms
            .remove(&swarm_id)
            .expect("resolved swarm exists");
        let acknowledged = swarm
            .board
            .iter()
            .flat_map(|entry| {
                entry
                    .pending_recipient_bot_ids
                    .iter()
                    .map(|target| (entry.id.clone(), target.clone()))
            })
            .collect();
        return Some((
            BotSwarmRemoval {
                swarm_id,
                disbanded: true,
            },
            acknowledged,
        ));
    }

    let swarm = catalog
        .swarms
        .get_mut(&swarm_id)
        .expect("resolved swarm exists");
    swarm.members.remove(bot_id);
    let mut removed = swarm
        .board
        .iter()
        .filter(|entry| entry.author.bot_id == bot_id)
        .map(|entry| entry.id.clone())
        .collect::<BTreeSet<_>>();
    loop {
        let previous = removed.len();
        let descendants = swarm
            .board
            .iter()
            .filter(|entry| {
                entry
                    .in_reply_to_message_id
                    .as_ref()
                    .is_some_and(|parent| removed.contains(parent))
            })
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        removed.extend(descendants);
        if removed.len() == previous {
            break;
        }
    }
    let mut acknowledged = BTreeSet::new();
    for entry in &mut swarm.board {
        if removed.contains(&entry.id) {
            acknowledged.extend(
                entry
                    .pending_recipient_bot_ids
                    .iter()
                    .map(|target| (entry.id.clone(), target.clone())),
            );
            continue;
        }
        if entry
            .pending_recipient_bot_ids
            .iter()
            .any(|pending| pending == bot_id)
        {
            acknowledged.insert((entry.id.clone(), bot_id.to_owned()));
        }
        entry
            .pending_recipient_bot_ids
            .retain(|pending| pending != bot_id);
        entry.assigned_recipient_session_ids.remove(bot_id);
    }
    swarm.board.retain(|entry| !removed.contains(&entry.id));
    swarm.updated_at_ms = unix_ms().max(swarm.created_at_ms);
    Some((
        BotSwarmRemoval {
            swarm_id,
            disbanded: false,
        },
        acknowledged,
    ))
}

pub(crate) fn validate_swarm_members(leader_bot_id: &str, member_bot_ids: &[String]) -> Result<()> {
    validate_bot_id(leader_bot_id)?;
    if member_bot_ids.len() < 2 {
        return Err(config("a swarm requires at least two members"));
    }
    if member_bot_ids.len() > MAX_SWARM_MEMBERS {
        return Err(config(format!(
            "a swarm supports at most {MAX_SWARM_MEMBERS} members"
        )));
    }
    let mut unique_bots = BTreeSet::new();
    for bot_id in member_bot_ids {
        validate_bot_id(bot_id)?;
        if !unique_bots.insert(bot_id.as_str()) {
            return Err(config(format!("Bot `{bot_id}` appears more than once")));
        }
    }
    if !unique_bots.contains(leader_bot_id) {
        return Err(config("swarm leader must be a member"));
    }
    Ok(())
}

impl BotsBackend for SwarmStore {
    fn active<'a>(&'a self, bot_id: &'a str) -> mobius::BoxFuture<'a, mobius::Result<bool>> {
        Box::pin(async move {
            self.snapshot_for_bot(bot_id)
                .await
                .map(|snapshot| snapshot.is_some())
                .map_err(mobius_error)
        })
    }

    fn scratchpad_scope<'a>(
        &'a self,
        bot_id: &'a str,
    ) -> mobius::BoxFuture<'a, mobius::Result<Option<String>>> {
        Box::pin(async move {
            self.snapshot_for_bot(bot_id)
                .await
                .map(|snapshot| snapshot.map(|snapshot| snapshot.swarm.id))
                .map_err(mobius_error)
        })
    }

    fn spawn_bot<'a>(
        &'a self,
        bot_id: &'a str,
        name: String,
        description: String,
    ) -> mobius::BoxFuture<'a, mobius::Result<String>> {
        Box::pin(async move {
            self.spawn_bot_inner(bot_id, name, description)
                .await
                .map_err(mobius_error)
        })
    }

    fn create_routine<'a>(
        &'a self,
        bot_id: &'a str,
        bot_handle: Option<String>,
        workspace: &'a Path,
        instructions: String,
        schedule: serde_json::Value,
        ends_at: Option<i64>,
    ) -> mobius::BoxFuture<'a, mobius::Result<String>> {
        Box::pin(async move {
            let schedule = serde_json::from_value(schedule).map_err(|error| {
                mobius_error(config(format!("invalid routine schedule: {error}")))
            })?;
            self.create_routine_inner(
                bot_id,
                bot_handle,
                workspace,
                instructions,
                schedule,
                ends_at,
            )
            .await
            .map_err(mobius_error)
        })
    }

    fn roster<'a>(&'a self, bot_id: &'a str) -> mobius::BoxFuture<'a, mobius::Result<String>> {
        Box::pin(async move { self.tool_roster(bot_id).await.map_err(mobius_error) })
    }

    fn read<'a>(&'a self, bot_id: &'a str) -> mobius::BoxFuture<'a, mobius::Result<String>> {
        Box::pin(async move { self.tool_read(bot_id).await.map_err(mobius_error) })
    }

    fn swarm_chat_context<'a>(
        &'a self,
        bot_id: &'a str,
        session_id: &'a str,
    ) -> mobius::BoxFuture<'a, mobius::Result<Option<String>>> {
        Box::pin(async move {
            SwarmStore::swarm_chat_context(self, bot_id, session_id)
                .await
                .map_err(mobius_error)
        })
    }

    fn can_reply<'a>(
        &'a self,
        bot_id: &'a str,
        message_id: &'a str,
    ) -> mobius::BoxFuture<'a, mobius::Result<bool>> {
        Box::pin(async move {
            SwarmStore::can_reply(self, bot_id, message_id)
                .await
                .map_err(mobius_error)
        })
    }

    fn post<'a>(
        &'a self,
        bot_id: &'a str,
        source_session_id: &'a str,
        text: String,
        in_reply_to_message_id: Option<String>,
    ) -> mobius::BoxFuture<'a, mobius::Result<String>> {
        Box::pin(async move {
            let post = SwarmStore::post(
                self,
                bot_id,
                source_session_id,
                text,
                in_reply_to_message_id,
            )
            .await
            .map_err(mobius_error)?;
            serde_json::to_string(&post).map_err(Into::into)
        })
    }
}

fn summary(bots: &BotStore, id: &str, swarm: &StoredSwarm) -> Result<SwarmSummary> {
    Ok(SwarmSummary {
        id: id.to_owned(),
        title: swarm.title.clone(),
        leader_bot_id: swarm.leader_bot_id.clone(),
        members: current_members(bots, swarm)?,
        latest_sequence: swarm.latest_sequence,
        created_at_ms: swarm.created_at_ms,
        updated_at_ms: swarm.updated_at_ms,
    })
}

fn current_members(bots: &BotStore, swarm: &StoredSwarm) -> Result<Vec<SwarmMember>> {
    let mut members = swarm
        .members
        .iter()
        .map(|(bot_id, stored)| current_member(bots, bot_id, stored))
        .collect::<Result<Vec<_>>>()?;
    members.sort_by(|left, right| {
        left.handle
            .cmp(&right.handle)
            .then_with(|| left.bot_id.cmp(&right.bot_id))
    });
    Ok(members)
}

fn current_member(bots: &BotStore, bot_id: &str, stored: &StoredMember) -> Result<SwarmMember> {
    let bot = bots.bot(bot_id)?;
    Ok(SwarmMember {
        bot_id: bot.id,
        handle: bot.handle,
        joined_at_ms: stored.joined_at_ms,
    })
}

fn validate_bot_references(bots: &BotStore, catalog: &Catalog) -> Result<()> {
    for bot_id in catalog
        .swarms
        .values()
        .flat_map(|swarm| swarm.members.keys())
    {
        bots.bot(bot_id)?;
    }
    Ok(())
}

fn ensure_bot_available(catalog: &Catalog, bot_id: &str) -> Result<()> {
    if catalog
        .swarms
        .values()
        .any(|swarm| swarm.members.contains_key(bot_id))
    {
        return Err(config(format!("Bot `{bot_id}` already belongs to a swarm")));
    }
    Ok(())
}

fn swarm_id_for_bot(catalog: &Catalog, bot_id: &str) -> Option<String> {
    catalog
        .swarms
        .iter()
        .find_map(|(id, swarm)| swarm.members.contains_key(bot_id).then(|| id.clone()))
}

fn trim_acknowledged(
    board: &mut VecDeque<BoardEntry>,
    pending_user_attention_message_ids: &BTreeSet<String>,
) {
    let protected_message_ids = protected_board_entries(board, pending_user_attention_message_ids);
    while board
        .iter()
        .filter(|entry| {
            entry.pending_recipient_bot_ids.is_empty() && !protected_message_ids.contains(&entry.id)
        })
        .count()
        > MAX_ACKNOWLEDGED_ENTRIES
    {
        let referenced = board
            .iter()
            .filter_map(|entry| entry.in_reply_to_message_id.as_deref())
            .collect::<BTreeSet<_>>();
        let index = board
            .iter()
            .position(|entry| {
                entry.pending_recipient_bot_ids.is_empty()
                    && !protected_message_ids.contains(&entry.id)
                    && !referenced.contains(entry.id.as_str())
            })
            .expect("a finite reply chain has an unreferenced leaf");
        board.remove(index);
    }
}

fn protected_board_entries(
    board: &VecDeque<BoardEntry>,
    pending_user_attention_message_ids: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut protected = board
        .iter()
        .filter(|entry| {
            !entry.pending_recipient_bot_ids.is_empty()
                || pending_user_attention_message_ids.contains(&entry.id)
        })
        .map(|entry| entry.id.clone())
        .collect::<BTreeSet<_>>();
    loop {
        let previous = protected.len();
        for entry in board {
            if protected.contains(&entry.id)
                && let Some(parent) = &entry.in_reply_to_message_id
            {
                protected.insert(parent.clone());
            }
        }
        if protected.len() == previous {
            return protected;
        }
    }
}

fn user_attention(catalog: &Catalog, message_id: &str) -> Result<UserAttention> {
    let (swarm, entry) = catalog
        .swarms
        .values()
        .find_map(|swarm| {
            swarm
                .board
                .iter()
                .find(|entry| entry.id == message_id)
                .map(|entry| (swarm, entry))
        })
        .ok_or_else(|| config(format!("unknown swarm message `{message_id}`")))?;
    let mut root = entry;
    while let Some(parent_id) = root.in_reply_to_message_id.as_deref() {
        root = swarm
            .board
            .iter()
            .find(|candidate| candidate.id == parent_id)
            .ok_or_else(|| config(format!("unknown swarm message `{parent_id}`")))?;
    }
    Ok(UserAttention {
        entry: entry.clone(),
        leader_bot_id: swarm.leader_bot_id.clone(),
        root_source_session_id: root.source_session_id.clone(),
        workspace: catalog.message_workspaces.get(&root.id).cloned(),
        assigned_session_id: catalog
            .assigned_user_attention_session_ids
            .get(message_id)
            .cloned(),
    })
}

fn outcome_text(outcome: &SwarmRunOutcome) -> String {
    match outcome {
        SwarmRunOutcome::Succeeded { summary } => bounded_message(&neutralize_mentions(summary)),
        SwarmRunOutcome::Failed { message } => {
            bounded_message(&format!("Task failed: {}", neutralize_mentions(message)))
        }
    }
}

fn routine_outcome_text(
    run: &RoutineRun,
    detail: Option<&str>,
    leader_handle: Option<&str>,
    needs_user_attention: bool,
) -> String {
    let prefix = leader_handle.map_or_else(String::new, |handle| format!("@{handle} "));
    let status = match run.status {
        RoutineRunStatus::Succeeded => "succeeded",
        RoutineRunStatus::Failed => "failed",
        RoutineRunStatus::Skipped => "was skipped",
        RoutineRunStatus::Running => "is still running",
    };
    let routine = run.routine_id.get(..8).unwrap_or(&run.routine_id);
    let attention = if needs_user_attention { " @user" } else { "" };
    bounded_message(&format!(
        "{prefix}Routine {routine} {status}{}{attention}",
        detail.map_or_else(String::new, |detail| format!(
            ": {}",
            neutralize_mentions(detail)
        ))
    ))
}

fn neutralize_mentions(text: &str) -> String {
    text.replace('@', "＠")
}

fn participant_session_id(swarm_id: &str, bot_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mobius-swarm-participant-v1\0");
    hash.update(swarm_id.as_bytes());
    hash.update(b"\0");
    hash.update(bot_id.as_bytes());
    let digest = hash.finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).to_string()
}

fn bounded_board_json(swarm: &StoredSwarm) -> Result<String> {
    let mut entries = Vec::new();
    let mut has_older = swarm.board.len() > MAX_PAGE_ENTRIES;
    for entry in swarm.board.iter().rev().take(MAX_PAGE_ENTRIES) {
        let end = entry
            .text
            .floor_char_boundary(MAX_TOOL_READ_TEXT_BYTES.min(entry.text.len()));
        let text = &entry.text[..end];
        entries.push(serde_json::json!({
            "id": entry.id,
            "sequence": entry.sequence,
            "created_at_ms": entry.created_at_ms,
            "author_bot_id": entry.author.bot_id,
            "author_handle": entry.author.handle,
            "source_session_id": entry.source_session_id,
            "text": text,
            "text_truncated": text.len() != entry.text.len(),
            "in_reply_to_message_id": entry.in_reply_to_message_id,
            "reply_depth": entry.reply_depth,
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
    Ok(serde_json::to_string(&serde_json::json!({
        "entries": entries,
        "has_older": has_older,
    }))?)
}

fn bounded_message(text: &str) -> String {
    if text.len() <= MAX_MESSAGE_BYTES {
        return text.into();
    }
    let mut end = MAX_MESSAGE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].into()
}

fn prune_catalog(catalog: &mut Catalog, live_routine_run_ids: &BTreeSet<String>) {
    for swarm in catalog.swarms.values_mut() {
        trim_acknowledged(
            &mut swarm.board,
            &catalog.pending_user_attention_message_ids,
        );
    }
    catalog
        .projected_routine_run_ids
        .retain(|run_id| live_routine_run_ids.contains(run_id));
    let live_message_ids = catalog
        .swarms
        .values()
        .flat_map(|swarm| swarm.board.iter().map(|entry| entry.id.clone()))
        .collect::<BTreeSet<_>>();
    catalog
        .message_workspaces
        .retain(|message_id, _| live_message_ids.contains(message_id));
    catalog
        .pending_user_attention_message_ids
        .retain(|message_id| live_message_ids.contains(message_id));
    catalog
        .assigned_user_attention_session_ids
        .retain(|message_id, _| live_message_ids.contains(message_id));
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
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

fn validate_catalog(catalog: &Catalog) -> Result<()> {
    let mut bots = BTreeSet::new();
    let mut message_ids = BTreeSet::new();
    let mut human_message_ids = BTreeSet::new();
    let mut user_attention_ids = BTreeSet::new();
    for (id, swarm) in &catalog.swarms {
        let mut pending_counts = BTreeMap::<&str, usize>::new();
        validate_swarm_id(id)?;
        validate_title(&swarm.title)?;
        validate_bot_id(&swarm.leader_bot_id)?;
        if swarm.members.len() > MAX_SWARM_MEMBERS {
            return Err(config(format!(
                "swarm `{id}` has more than {MAX_SWARM_MEMBERS} members"
            )));
        }
        if swarm.created_at_ms < 0 || swarm.updated_at_ms < swarm.created_at_ms {
            return Err(config(format!("swarm `{id}` has invalid timestamps")));
        }
        if !swarm.members.contains_key(&swarm.leader_bot_id) {
            return Err(config(format!("swarm `{id}` leader is not a member")));
        }
        for (bot_id, member) in &swarm.members {
            validate_bot_id(bot_id)?;
            if member.joined_at_ms < swarm.created_at_ms
                || member.joined_at_ms > swarm.updated_at_ms
            {
                return Err(config(format!(
                    "swarm `{id}` member has an invalid join time"
                )));
            }
            if !bots.insert(bot_id.clone()) {
                return Err(config(format!(
                    "Bot `{bot_id}` belongs to more than one swarm"
                )));
            }
        }
        let mut previous_sequence = 0;
        for entry in &swarm.board {
            validate_message_id(&entry.id)?;
            if entry.author.bot_id == USER_AUTHOR_ID {
                if entry.author.handle != USER_HANDLE {
                    return Err(config(
                        "human swarm messages must use the reserved user author",
                    ));
                }
                human_message_ids.insert(entry.id.clone());
            } else {
                validate_member(&entry.author)?;
                if entry.author.handle == USER_HANDLE {
                    return Err(config("the swarm handle `user` is reserved"));
                }
            }
            validate_session_id(&entry.source_session_id)?;
            if let Some(source_event_id) = &entry.source_event_id {
                validate_message_id(source_event_id)?;
            }
            validate_message(&entry.text)?;
            if mentioned_handles(&entry.text).contains(USER_HANDLE) {
                user_attention_ids.insert(entry.id.clone());
            }
            if entry.reply_depth > MAX_TERMINAL_REPLY_DEPTH {
                return Err(config(format!(
                    "swarm message `{}` exceeds the reply limit",
                    entry.id
                )));
            }
            match (&entry.in_reply_to_message_id, entry.reply_depth) {
                (None, 0) => {}
                (Some(parent_id), depth) if depth > 0 => {
                    validate_message_id(parent_id)?;
                    let Some(parent) = swarm.board.iter().find(|parent| parent.id == *parent_id)
                    else {
                        return Err(config(format!(
                            "swarm message `{}` has an unknown parent",
                            entry.id
                        )));
                    };
                    if parent.sequence >= entry.sequence
                        || parent.reply_depth.checked_add(1) != Some(depth)
                        || !parent
                            .mentioned_recipient_bot_ids
                            .iter()
                            .any(|recipient| recipient == &entry.author.bot_id)
                    {
                        return Err(config(format!(
                            "swarm message `{}` has an invalid reply chain",
                            entry.id
                        )));
                    }
                }
                _ => {
                    return Err(config(format!(
                        "swarm message `{}` has inconsistent reply metadata",
                        entry.id
                    )));
                }
            }
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
            validate_recipient_ids(&entry.mentioned_recipient_bot_ids)?;
            validate_recipient_ids(&entry.pending_recipient_bot_ids)?;
            if entry.reply_depth == MAX_TERMINAL_REPLY_DEPTH
                && (!entry.mentioned_recipient_bot_ids.is_empty()
                    || !entry.pending_recipient_bot_ids.is_empty()
                    || !entry.assigned_recipient_session_ids.is_empty())
            {
                return Err(config(format!(
                    "swarm message `{}` routes beyond the reply limit",
                    entry.id
                )));
            }
            for (recipient, session_id) in &entry.assigned_recipient_session_ids {
                validate_bot_id(recipient)?;
                validate_delivery_session_id(session_id)?;
                if !entry
                    .mentioned_recipient_bot_ids
                    .iter()
                    .any(|mentioned| mentioned == recipient)
                {
                    return Err(config(format!(
                        "swarm message `{}` assigned a conversation to a non-recipient",
                        entry.id
                    )));
                }
                if session_id != &participant_session_id(id, recipient) {
                    return Err(config(format!(
                        "swarm message `{}` has an invalid participant conversation",
                        entry.id
                    )));
                }
            }
            for recipient in &entry.pending_recipient_bot_ids {
                let count = pending_counts.entry(recipient).or_default();
                *count += 1;
                if *count > MAX_PENDING_DELIVERIES_PER_RECIPIENT {
                    return Err(config(format!(
                        "Bot `{recipient}` has too many pending swarm messages"
                    )));
                }
            }
            if entry.pending_recipient_bot_ids.iter().any(|pending| {
                !entry
                    .mentioned_recipient_bot_ids
                    .iter()
                    .any(|mentioned| mentioned == pending)
            }) {
                return Err(config(format!(
                    "swarm message `{}` has a pending non-recipient",
                    entry.id
                )));
            }
        }
        let protected_message_ids =
            protected_board_entries(&swarm.board, &catalog.pending_user_attention_message_ids);
        if swarm
            .board
            .iter()
            .filter(|entry| {
                entry.pending_recipient_bot_ids.is_empty()
                    && !protected_message_ids.contains(&entry.id)
            })
            .count()
            > MAX_ACKNOWLEDGED_ENTRIES
        {
            return Err(config(format!("swarm `{id}` board state is invalid")));
        }
    }
    if !human_message_ids
        .iter()
        .all(|message_id| catalog.message_workspaces.contains_key(message_id))
    {
        return Err(config("human swarm message workspaces are inconsistent"));
    }
    if !catalog
        .message_workspaces
        .keys()
        .all(|message_id| message_ids.contains(message_id))
    {
        return Err(config(
            "swarm message workspace references an invalid message",
        ));
    }
    for workspace in catalog.message_workspaces.values() {
        if !workspace.is_absolute() {
            return Err(config("human swarm message workspace must be absolute"));
        }
    }
    if !catalog
        .pending_user_attention_message_ids
        .iter()
        .all(|message_id| user_attention_ids.contains(message_id))
    {
        return Err(config(
            "pending user attention references an invalid swarm message",
        ));
    }
    for (message_id, session_id) in &catalog.assigned_user_attention_session_ids {
        if !catalog
            .pending_user_attention_message_ids
            .contains(message_id)
        {
            return Err(config("user attention conversation is not pending"));
        }
        validate_delivery_session_id(session_id)?;
    }
    for run_id in &catalog.projected_routine_run_ids {
        validate_message_id(run_id)?;
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
        validate_bot_id(recipient)?;
        if !unique.insert(recipient) {
            return Err(config("swarm message recipient appears more than once"));
        }
    }
    Ok(())
}

fn validate_member(member: &SwarmMember) -> Result<()> {
    validate_bot_id(&member.bot_id)?;
    validate_handle(&member.handle)?;
    if member.joined_at_ms < 0 {
        return Err(config("swarm member join time cannot be negative"));
    }
    Ok(())
}

fn validate_handle(handle: &str) -> Result<()> {
    if handle.is_empty()
        || handle.len() > MAX_HANDLE_BYTES
        || !handle.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(config(
            "swarm handle must contain 1-64 lowercase letters, digits, dashes, or underscores",
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

fn validate_bot_id(bot_id: &str) -> Result<()> {
    if bot_id.is_empty() || bot_id.len() > MAX_ID_BYTES {
        return Err(config(format!(
            "Bot id must contain 1-{MAX_ID_BYTES} bytes"
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

fn validate_delivery_session_id(id: &str) -> Result<()> {
    Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| config("invalid swarm delivery session id"))
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

    use crate::bots::BeginRun;
    use crate::wire::AgentComposition;

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
        let state_dir = directory.path().join("state");
        std::fs::create_dir(&state_dir).expect("state directory");
        let bots = Arc::new(BotStore::open(&state_dir).expect("Bot store"));
        for handle in ["leader", "reviewer", "observer", "third", "overflow"] {
            add_bot(&bots, handle);
        }
        let gateway = GatewayConfig::new(crate::config::DEFAULT_LISTEN, None)
            .expect("gateway config")
            .registering_provider(
                AgentComposition::default().provider,
                "Test".into(),
                Default::default(),
                Vec::new(),
                Vec::new(),
            )
            .expect("Bot defaults");
        let (store, deliveries) = SwarmStore::new(
            Arc::clone(&checkpoints),
            bots,
            Arc::new(StdMutex::new(gateway)),
        );
        (directory, checkpoints, store, deliveries)
    }

    fn add_bot(bots: &BotStore, handle: &str) -> String {
        bots.create_bot(
            handle,
            &format!("Test Bot {handle}"),
            AgentComposition::default(),
        )
        .expect("create Bot")
        .id
    }

    fn bot_id(store: &SwarmStore, handle: &str) -> String {
        store
            .bots
            .bots()
            .expect("Bots")
            .into_iter()
            .find(|bot| bot.handle == handle)
            .expect("Bot handle")
            .id
    }

    fn reload(
        checkpoints: Arc<dyn CheckpointStore>,
        store: &SwarmStore,
    ) -> (SwarmStore, mpsc::UnboundedReceiver<SwarmDelivery>) {
        SwarmStore::new(
            checkpoints,
            Arc::clone(&store.bots),
            Arc::clone(&store.gateway),
        )
    }

    async fn create_swarm(store: &SwarmStore) -> SwarmSummary {
        let leader = bot_id(store, "leader");
        store
            .create(
                "Review team".into(),
                leader.clone(),
                vec![leader, bot_id(store, "reviewer")],
            )
            .await
            .expect("create swarm")
    }

    async fn post(store: &SwarmStore, handle: &str, text: String) -> Result<SwarmPost> {
        store
            .post(
                &bot_id(store, handle),
                &format!("{handle}-thread"),
                text,
                None,
            )
            .await
    }

    #[tokio::test]
    async fn membership_is_unique_and_lazily_reloads() {
        let (_directory, checkpoints, store, _deliveries) = store();
        let created = create_swarm(&store).await;
        let created_title = created.title.clone();
        assert!(created.created_at_ms > 0);
        assert_eq!(created.created_at_ms, created.updated_at_ms);

        let leader = bot_id(&store, "leader");
        let duplicate = store
            .create(
                "Another".into(),
                leader.clone(),
                vec![leader, bot_id(&store, "third")],
            )
            .await
            .expect_err("Bot cannot join two swarms");
        assert!(duplicate.to_string().contains("already belongs"));

        let (reloaded, _deliveries) = reload(checkpoints, &store);
        assert_eq!(reloaded.summaries().await.expect("reload"), vec![created]);
        let snapshot = reloaded
            .snapshot_for_bot(&bot_id(&store, "reviewer"))
            .await
            .expect("snapshot")
            .expect("membership");
        assert_eq!(snapshot.handle, "reviewer");
        assert_eq!(snapshot.swarm.title, created_title);
    }

    #[tokio::test]
    async fn persisted_roster_is_keyed_only_by_bot_id() {
        let (_directory, checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        let expected = swarm
            .members
            .iter()
            .map(|member| member.bot_id.as_str())
            .collect::<BTreeSet<_>>();

        let state = checkpoints
            .load_state(STATE_SCOPE, STATE_KEY)
            .await
            .expect("load swarm state")
            .expect("persisted swarm state");
        let members = state["swarms"][swarm.id.as_str()]["members"]
            .as_object()
            .expect("member map");

        assert_eq!(
            members.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            expected
        );
        assert!(
            members
                .values()
                .all(|member| member.get("handle").is_none())
        );
    }

    #[tokio::test]
    async fn membership_queries_resolve_current_swarm_scope() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        let reviewer = bot_id(&store, "reviewer");

        assert_eq!(
            (
                store
                    .snapshot_for_bot(&reviewer)
                    .await
                    .expect("membership")
                    .is_some(),
                store
                    .contains_swarm(&swarm.id)
                    .await
                    .expect("existing swarm"),
                store
                    .contains_swarm(&Uuid::new_v4().to_string())
                    .await
                    .expect("missing swarm"),
                BotsBackend::scratchpad_scope(&store, &reviewer)
                    .await
                    .expect("scratchpad scope"),
            ),
            (true, true, false, Some(swarm.id))
        );
    }

    #[tokio::test]
    async fn leader_can_create_and_join_a_bot_from_gateway_defaults() {
        let (_directory, _checkpoints, store, mut deliveries) = store();
        let swarm = create_swarm(&store).await;
        let leader = bot_id(&store, "leader");

        let output = BotsBackend::spawn_bot(
            &store,
            &leader,
            "Researcher".into(),
            "Find reliable sources".into(),
        )
        .await
        .expect("spawn Bot");
        let output: serde_json::Value = serde_json::from_str(&output).expect("spawn output");
        let spawned = store
            .bots
            .bot(output["bot_id"].as_str().expect("Bot ID"))
            .expect("spawned Bot");
        let defaults = store
            .gateway
            .lock()
            .expect("gateway config")
            .bot_defaults
            .as_ref()
            .expect("Bot defaults")
            .config
            .clone();

        assert_eq!(
            (
                output["handle"].as_str(),
                output["name"].as_str(),
                output["swarm_id"].as_str(),
                spawned.config.config,
                store
                    .snapshot_for_bot(&spawned.id)
                    .await
                    .expect("membership")
                    .is_some(),
                deliveries.recv().await,
                deliveries.recv().await,
            ),
            (
                Some("researcher"),
                Some("Researcher"),
                Some(swarm.id.as_str()),
                defaults,
                true,
                Some(SwarmDelivery::Changed),
                Some(SwarmDelivery::CatalogChanged),
            )
        );
    }

    #[tokio::test]
    async fn nonleader_cannot_create_a_bot() {
        let (_directory, _checkpoints, store, mut deliveries) = store();
        create_swarm(&store).await;
        let reviewer = bot_id(&store, "reviewer");
        let before = store.bots.bots().expect("Bots").len();

        let error = BotsBackend::spawn_bot(
            &store,
            &reviewer,
            "Unauthorized".into(),
            "Must not persist".into(),
        )
        .await
        .expect_err("nonleader spawn");

        assert!(error.to_string().contains("only a Swarm leader"));
        assert_eq!(store.bots.bots().expect("Bots").len(), before);
        assert!(matches!(
            deliveries.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn bot_can_schedule_itself_without_a_swarm() {
        let (directory, _checkpoints, store, _deliveries) = store();
        let observer = bot_id(&store, "observer");

        let output = BotsBackend::create_routine(
            &store,
            &observer,
            None,
            directory.path(),
            "Check the project state.".into(),
            serde_json::json!({"kind": "interval", "every_seconds": 3600}),
            None,
        )
        .await
        .expect("create self routine");
        let output: serde_json::Value = serde_json::from_str(&output).expect("routine output");
        let routines = store
            .bots
            .routine_records(Some(&observer), unix_ms() / 1_000)
            .expect("self routines");

        assert_eq!(output["bot_handle"], "observer");
        assert_eq!(routines.len(), 1);
        assert_eq!(routines[0].instructions, "Check the project state.");
    }

    #[tokio::test]
    async fn leader_can_schedule_a_current_swarm_member() {
        let (directory, _checkpoints, store, _deliveries) = store();
        create_swarm(&store).await;
        let leader = bot_id(&store, "leader");
        let reviewer = bot_id(&store, "reviewer");

        BotsBackend::create_routine(
            &store,
            &leader,
            Some("reviewer".into()),
            directory.path(),
            "Review dependency releases.".into(),
            serde_json::json!({
                "kind": "cron",
                "expression": "0 9 * * 1",
                "time_zone": "Asia/Singapore"
            }),
            None,
        )
        .await
        .expect("create member routine");

        assert_eq!(
            store
                .bots
                .routine_records(Some(&reviewer), unix_ms() / 1_000)
                .expect("member routines")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn leader_cannot_schedule_a_bot_outside_its_swarm() {
        let (directory, _checkpoints, store, _deliveries) = store();
        create_swarm(&store).await;
        let leader = bot_id(&store, "leader");
        let observer = bot_id(&store, "observer");

        let error = BotsBackend::create_routine(
            &store,
            &leader,
            Some("observer".into()),
            directory.path(),
            "Unauthorized work.".into(),
            serde_json::json!({"kind": "interval", "every_seconds": 3600}),
            None,
        )
        .await
        .expect_err("nonmember routine");

        assert!(error.to_string().contains("not a current Swarm member"));
        assert!(
            store
                .bots
                .routine_records(Some(&observer), unix_ms() / 1_000)
                .expect("routines")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn nonleader_cannot_schedule_another_swarm_member() {
        let (directory, _checkpoints, store, _deliveries) = store();
        create_swarm(&store).await;
        let reviewer = bot_id(&store, "reviewer");

        let error = BotsBackend::create_routine(
            &store,
            &reviewer,
            Some("leader".into()),
            directory.path(),
            "Unauthorized work.".into(),
            serde_json::json!({"kind": "interval", "every_seconds": 3600}),
            None,
        )
        .await
        .expect_err("nonleader routine");

        assert!(error.to_string().contains("only a Swarm leader"));
        assert!(
            store
                .bots
                .routine_records(None, unix_ms() / 1_000)
                .expect("routines")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn failed_join_rolls_back_the_new_bot() {
        let (_directory, _checkpoints, store, mut deliveries) = store();
        let leader = bot_id(&store, "leader");
        let mut members = vec![leader.clone(), bot_id(&store, "reviewer")];
        members.extend(
            (0..MAX_SWARM_MEMBERS - members.len())
                .map(|index| add_bot(&store.bots, &format!("member-{index}"))),
        );
        store
            .create("Full team".into(), leader.clone(), members)
            .await
            .expect("full swarm");
        let before = store.bots.bots().expect("Bots").len();

        let error = BotsBackend::spawn_bot(
            &store,
            &leader,
            "Should rollback".into(),
            "The full roster rejects this Bot".into(),
        )
        .await
        .expect_err("full roster");

        assert!(error.to_string().contains("at most 100"));
        assert_eq!(store.bots.bots().expect("Bots").len(), before);
        assert!(matches!(
            deliveries.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn joining_cannot_exceed_the_member_limit() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        let members = (0..MAX_SWARM_MEMBERS)
            .map(|index| add_bot(&store.bots, &format!("member-{index}")))
            .collect::<Vec<_>>();
        let swarm = store
            .create("Full team".into(), members[0].clone(), members)
            .await
            .expect("full swarm");

        let error = store
            .join(&swarm.id, bot_id(&store, "overflow"))
            .await
            .expect_err("member limit");

        assert!(error.to_string().contains("at most 100"));
    }

    #[tokio::test]
    async fn rename_changes_the_durable_swarm_title() {
        let (_directory, checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;

        store
            .rename(&swarm.id, "Release crew".into())
            .await
            .expect("rename swarm");

        let (reloaded, _deliveries) = reload(checkpoints, &store);
        assert_eq!(
            reloaded.summaries().await.expect("reload")[0].title,
            "Release crew"
        );
    }

    #[tokio::test]
    async fn peer_replies_stop_at_the_private_hop_limit() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        create_swarm(&store).await;
        let leader = bot_id(&store, "leader");
        let reviewer = bot_id(&store, "reviewer");

        let first = post(&store, "leader", "@reviewer please review".into())
            .await
            .expect("initial post");
        let second = store
            .post(
                &reviewer,
                "reviewer-thread",
                "@leader found an issue".into(),
                Some(first.entry.id),
            )
            .await
            .expect("first reply");
        let third = store
            .post(
                &leader,
                "leader-reply-thread",
                "@reviewer please verify the fix".into(),
                Some(second.entry.id),
            )
            .await
            .expect("second reply");
        let fourth = store
            .post(
                &reviewer,
                "reviewer-final-thread",
                "@leader verified".into(),
                Some(third.entry.id),
            )
            .await
            .expect("third reply");

        assert!(
            !store
                .can_reply(&leader, &fourth.entry.id)
                .await
                .expect("reply policy")
        );
        assert!(
            store
                .post(
                    &leader,
                    "leader-too-deep",
                    "@reviewer another loop".into(),
                    Some(fourth.entry.id),
                )
                .await
                .expect_err("reply depth")
                .to_string()
                .contains("3-hop")
        );
    }

    #[tokio::test]
    async fn mentions_are_resolved_delivered_and_acknowledged() {
        let (_directory, checkpoints, store, mut deliveries) = store();
        let swarm = create_swarm(&store).await;
        let leader = bot_id(&store, "leader");
        let reviewer = bot_id(&store, "reviewer");
        let leader_handle = swarm
            .members
            .iter()
            .find(|member| member.bot_id == leader)
            .expect("leader")
            .handle
            .clone();
        let reviewer_handle = swarm
            .members
            .iter()
            .find(|member| member.bot_id == reviewer)
            .expect("reviewer")
            .handle
            .clone();

        let unknown = post(&store, "leader", "Can @missing check this?".into())
            .await
            .expect_err("unknown mention");
        assert!(unknown.to_string().contains("@missing"));

        let text = format!("Can @{reviewer_handle} check this?");
        let post = post(&store, "leader", text.clone()).await.expect("post");
        assert_eq!(deliveries.recv().await, Some(SwarmDelivery::Changed));
        assert_eq!(
            deliveries.recv().await,
            Some(SwarmDelivery::Pending {
                target_bot_id: reviewer.clone()
            })
        );
        assert_eq!(post.entry.sequence, 1);
        assert!(post.entry.created_at_ms >= swarm.created_at_ms);
        assert_eq!(post.entry.author.handle, leader_handle);
        assert_eq!(
            post.resolved_recipient_bot_ids,
            std::slice::from_ref(&reviewer)
        );
        let records = store.records().await.expect("wire records");
        assert_eq!(records[0].messages[0].text, text);
        assert!(
            store
                .snapshot_for_bot(&reviewer)
                .await
                .expect("active")
                .is_some()
        );
        assert_eq!(
            store
                .pending_deliveries(&reviewer)
                .await
                .expect("pending")
                .len(),
            1
        );

        assert_eq!(
            store
                .acknowledge(&post.entry.id, &reviewer)
                .await
                .expect("acknowledge"),
            AcknowledgeOutcome::Acknowledged
        );
        assert_eq!(
            deliveries.recv().await,
            Some(SwarmDelivery::Acknowledged {
                target_bot_id: reviewer.clone(),
                message_id: post.entry.id.clone(),
            })
        );
        assert_eq!(
            store
                .acknowledge(&post.entry.id, &reviewer)
                .await
                .expect("idempotent acknowledge"),
            AcknowledgeOutcome::AlreadyAcknowledged
        );
        assert_eq!(
            deliveries.recv().await,
            Some(SwarmDelivery::Acknowledged {
                target_bot_id: reviewer.clone(),
                message_id: post.entry.id.clone(),
            })
        );
        assert!(
            store
                .pending_deliveries(&reviewer)
                .await
                .expect("pending")
                .is_empty()
        );

        let (reloaded, _deliveries) = reload(checkpoints, &store);
        let page = reloaded
            .board_page(&swarm.id, None, 10)
            .await
            .expect("board page");
        assert_eq!(page.entries[0].id, post.entry.id);
        assert!(page.entries[0].pending_recipient_bot_ids.is_empty());
    }

    #[tokio::test]
    async fn delivery_claim_reuses_one_durable_participant_conversation() {
        let (_directory, checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        let first = post(&store, "leader", "@reviewer please review".into())
            .await
            .expect("post");
        let leader = bot_id(&store, "leader");
        let reviewer = bot_id(&store, "reviewer");

        let claim = store
            .claim_next_delivery(&reviewer)
            .await
            .expect("claim delivery")
            .expect("pending delivery");
        let session_id = claim.session_id().to_owned();
        Uuid::parse_str(&session_id).expect("generated session UUID");
        assert_eq!(session_id, participant_session_id(&swarm.id, &reviewer));
        assert!(
            store
                .swarm_chat_context(&reviewer, &session_id)
                .await
                .expect("active participant context")
                .is_some()
        );
        assert!(
            store
                .swarm_chat_context(&reviewer, &Uuid::new_v4().to_string())
                .await
                .expect("unrelated conversation context")
                .is_none()
        );
        drop(claim);
        let repeated = store
            .claim_next_delivery(&reviewer)
            .await
            .expect("repeat claim")
            .expect("pending delivery");
        assert_eq!(repeated.session_id(), session_id);
        drop(repeated);
        assert!(
            store
                .claim_next_delivery(&leader)
                .await
                .expect("non-recipient claim")
                .is_none()
        );
        store
            .settle_delivery(
                &first.entry.id,
                &session_id,
                &reviewer,
                SwarmRunOutcome::Succeeded {
                    summary: "First review complete".into(),
                },
            )
            .await
            .expect("settle first delivery");
        assert!(
            store
                .swarm_chat_context(&reviewer, &session_id)
                .await
                .expect("settled participant context")
                .is_none()
        );
        let second_post = post(&store, "leader", "@reviewer please review again".into())
            .await
            .expect("second post");
        let second = store
            .claim_next_delivery(&reviewer)
            .await
            .expect("second claim")
            .expect("second pending delivery");
        assert_eq!(second.session_id(), session_id);
        assert_eq!(second.delivery().entry.id, second_post.entry.id);
        drop(second);
        assert!(
            !store
                .settle_delivery(
                    "input-client-request",
                    &session_id,
                    &reviewer,
                    SwarmRunOutcome::Succeeded {
                        summary: "unrelated user turn".into(),
                    },
                )
                .await
                .expect("ignore unrelated user turn")
        );
        assert!(
            !store
                .settle_delivery(
                    &first.entry.id,
                    &session_id,
                    &reviewer,
                    SwarmRunOutcome::Succeeded {
                        summary: "stale replay".into(),
                    },
                )
                .await
                .expect("ignore stale settlement")
        );
        assert_eq!(
            store
                .pending_deliveries(&reviewer)
                .await
                .expect("second delivery remains pending")[0]
                .entry
                .id,
            second_post.entry.id
        );
        assert!(
            store
                .swarm_chat_context(&reviewer, &session_id)
                .await
                .expect("reused participant context")
                .is_some()
        );

        let (reloaded, _deliveries) = reload(checkpoints, &store);
        let reloaded = reloaded
            .claim_next_delivery(&reviewer)
            .await
            .expect("reloaded claim")
            .expect("pending delivery");
        assert_eq!(reloaded.session_id(), session_id);
    }

    #[tokio::test]
    async fn claimed_delivery_serializes_leave_until_queue_acceptance() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        post(&store, "leader", "@reviewer please review".into())
            .await
            .expect("post");
        let reviewer = bot_id(&store, "reviewer");
        let claim = store
            .claim_next_delivery(&reviewer)
            .await
            .expect("claim delivery")
            .expect("pending delivery");
        let leaving = store.leave(&swarm.id, &reviewer);
        tokio::pin!(leaving);
        tokio::select! {
            biased;
            result = &mut leaving => panic!("leave settled before queue acceptance: {result:?}"),
            () = std::future::ready(()) => {}
        }

        assert_eq!(
            claim
                .accept(std::future::ready("accepted"))
                .await
                .expect("accept delivery"),
            Some("accepted")
        );
        let left = leaving.await.expect("leave after acceptance");
        assert!(left.members.iter().all(|member| member.bot_id != reviewer));
    }

    #[tokio::test]
    async fn claimed_delivery_serializes_disband_until_queue_acceptance() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        post(&store, "leader", "@reviewer please review".into())
            .await
            .expect("post");
        let reviewer = bot_id(&store, "reviewer");
        let claim = store
            .claim_next_delivery(&reviewer)
            .await
            .expect("claim delivery")
            .expect("pending delivery");
        let disbanding = store.disband(&swarm.id);
        tokio::pin!(disbanding);
        tokio::select! {
            biased;
            result = &mut disbanding => {
                panic!("disband settled before queue acceptance: {result:?}");
            }
            () = std::future::ready(()) => {}
        }

        assert_eq!(
            claim
                .accept(std::future::ready("accepted"))
                .await
                .expect("accept delivery"),
            Some("accepted")
        );
        disbanding.await.expect("disband after acceptance");
        assert!(store.summaries().await.expect("swarms").is_empty());
    }

    #[tokio::test]
    async fn pending_source_sessions_are_protected_until_acknowledgement() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        create_swarm(&store).await;
        let post = post(&store, "leader", "@reviewer please review".into())
            .await
            .expect("post");
        let source_tree = vec!["unrelated-thread".into(), "leader-thread".into()];
        let reviewer = bot_id(&store, "reviewer");

        assert!(
            store
                .has_pending_source_sessions(&source_tree)
                .await
                .expect("pending source")
        );
        store
            .acknowledge(&post.entry.id, &reviewer)
            .await
            .expect("acknowledge");
        assert!(
            !store
                .has_pending_source_sessions(&source_tree)
                .await
                .expect("settled source")
        );
    }

    #[tokio::test]
    async fn leave_and_disband_settle_removed_pending_deliveries() {
        let (_directory, _checkpoints, store, mut deliveries) = store();
        let leader = bot_id(&store, "leader");
        let observer = bot_id(&store, "observer");
        let reviewer = bot_id(&store, "reviewer");
        let swarm = store
            .create(
                "Review team".into(),
                leader.clone(),
                vec![leader, observer.clone(), reviewer.clone()],
            )
            .await
            .expect("create swarm");
        let handle = |bot_id: &str| {
            swarm
                .members
                .iter()
                .find(|member| member.bot_id == bot_id)
                .expect("member")
                .handle
                .clone()
        };
        let post = post(
            &store,
            "leader",
            format!(
                "@{} @{} please review",
                handle(&observer),
                handle(&reviewer)
            ),
        )
        .await
        .expect("post");
        for _ in 0..3 {
            deliveries.recv().await.expect("initial delivery signal");
        }

        store
            .leave(&swarm.id, &reviewer)
            .await
            .expect("leave swarm");
        store.disband(&swarm.id).await.expect("disband swarm");

        assert_eq!(
            [deliveries.recv().await, deliveries.recv().await],
            [
                Some(SwarmDelivery::Acknowledged {
                    target_bot_id: reviewer,
                    message_id: post.entry.id.clone(),
                }),
                Some(SwarmDelivery::Acknowledged {
                    target_bot_id: observer,
                    message_id: post.entry.id,
                }),
            ]
        );
    }

    #[tokio::test]
    async fn acknowledgement_is_idempotent_after_its_board_is_gone() {
        let (_directory, checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        let reviewer = bot_id(&store, "reviewer");
        let reviewer_handle = swarm
            .members
            .iter()
            .find(|member| member.bot_id == reviewer)
            .expect("reviewer")
            .handle
            .clone();
        let post = post(&store, "leader", format!("@{reviewer_handle} review this"))
            .await
            .expect("post");
        store.disband(&swarm.id).await.expect("disband");
        let (reloaded, mut deliveries) = reload(checkpoints, &store);

        let outcome = reloaded
            .acknowledge(&post.entry.id, &reviewer)
            .await
            .expect("acknowledge removed board");

        assert_eq!(outcome, AcknowledgeOutcome::MessageGone);
        assert_eq!(
            deliveries.recv().await,
            Some(SwarmDelivery::Acknowledged {
                target_bot_id: reviewer,
                message_id: post.entry.id,
            })
        );
    }

    #[tokio::test]
    async fn acknowledgement_rejects_a_non_recipient_of_a_retained_message() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        create_swarm(&store).await;
        let post = post(&store, "leader", "Board-only update".into())
            .await
            .expect("post");
        let reviewer = bot_id(&store, "reviewer");

        let error = store
            .acknowledge(&post.entry.id, &reviewer)
            .await
            .expect_err("non-recipient acknowledgement");

        assert!(error.to_string().contains("is not a recipient"));
    }

    #[tokio::test]
    async fn board_only_posts_signal_catalog_changes_without_peer_delivery() {
        let (_directory, _checkpoints, store, mut deliveries) = store();
        create_swarm(&store).await;

        post(&store, "leader", "Shared status update".into())
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
        post(&store, "leader", "\0".repeat(MAX_MESSAGE_BYTES))
            .await
            .expect("escape-heavy post");

        let output = store
            .tool_read(&bot_id(&store, "leader"))
            .await
            .expect("model board read");
        let output: serde_json::Value = serde_json::from_str(&output).expect("valid JSON output");

        assert!(serde_json::to_vec(&output).expect("encoded output").len() <= MAX_TOOL_READ_BYTES);
        assert_eq!(output["entries"][0]["text_truncated"], true);
        assert_eq!(output["has_older"], false);
    }

    #[tokio::test]
    async fn retention_never_evicts_pending_entries() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        let reviewer = bot_id(&store, "reviewer");
        let reviewer_handle = swarm
            .members
            .iter()
            .find(|member| member.bot_id == reviewer)
            .expect("reviewer")
            .handle
            .clone();
        let pending = post(&store, "leader", format!("Please check @{reviewer_handle}"))
            .await
            .expect("pending post");
        for sequence in 0..=MAX_ACKNOWLEDGED_ENTRIES {
            post(&store, "leader", format!("board update {sequence}"))
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
            .pending_deliveries(&reviewer)
            .await
            .expect("pending delivery");
        assert_eq!(pending_delivery.len(), 1);
        assert_eq!(pending_delivery[0].entry.id, pending.entry.id);
    }

    #[tokio::test]
    async fn posting_backpressures_a_recipient_with_a_full_pending_queue() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        let reviewer = bot_id(&store, "reviewer");
        let reviewer_handle = swarm
            .members
            .iter()
            .find(|member| member.bot_id == reviewer)
            .expect("reviewer")
            .handle
            .clone();
        for sequence in 0..MAX_PENDING_DELIVERIES_PER_RECIPIENT {
            post(
                &store,
                "leader",
                format!("@{reviewer_handle} review {sequence}"),
            )
            .await
            .expect("pending post within limit");
        }

        let error = post(&store, "leader", format!("@{reviewer_handle} one too many"))
            .await
            .expect_err("pending delivery limit");

        assert!(error.to_string().contains("pending swarm messages"));
        assert_eq!(
            store
                .pending_deliveries(&reviewer)
                .await
                .expect("pending deliveries")
                .len(),
            MAX_PENDING_DELIVERIES_PER_RECIPIENT
        );
    }

    #[tokio::test]
    async fn human_and_bot_messages_share_the_leaders_swarm_conversation() {
        let (directory, checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        let workspace = directory.path().to_path_buf();

        let posted = store
            .post_user(
                &swarm.id,
                workspace.clone(),
                "Please coordinate this".into(),
            )
            .await
            .expect("human post");

        assert_eq!(posted.entry.author.bot_id, USER_AUTHOR_ID);
        assert_eq!(posted.entry.author.handle, USER_HANDLE);
        assert_eq!(
            posted.resolved_recipient_bot_ids.as_slice(),
            std::slice::from_ref(&swarm.leader_bot_id)
        );
        let (reloaded, _deliveries) = reload(checkpoints, &store);
        let delivery = reloaded
            .claim_next_delivery(&posted.resolved_recipient_bot_ids[0])
            .await
            .expect("claim")
            .expect("leader delivery");
        assert_eq!(delivery.delivery().entry.id, posted.entry.id);
        let session_id = delivery.session_id().to_owned();
        assert_eq!(
            session_id,
            participant_session_id(&swarm.id, &swarm.leader_bot_id)
        );
        drop(delivery);
        reloaded
            .settle_delivery(
                &posted.entry.id,
                &session_id,
                &swarm.leader_bot_id,
                SwarmRunOutcome::Succeeded {
                    summary: "Coordinated".into(),
                },
            )
            .await
            .expect("settle human post");

        let peer_post = post(&reloaded, "reviewer", "@leader please follow up".into())
            .await
            .expect("Bot post");
        let peer_delivery = reloaded
            .claim_next_delivery(&swarm.leader_bot_id)
            .await
            .expect("claim Bot post")
            .expect("leader delivery");

        assert_eq!(peer_delivery.session_id(), session_id);
        let context = reloaded
            .swarm_chat_context(&swarm.leader_bot_id, &session_id)
            .await
            .expect("Swarm Chat context")
            .expect("active participant context");
        assert!(context.contains(&posted.entry.id));
        assert!(context.contains(&peer_post.entry.id));
    }

    #[tokio::test]
    async fn terminal_delivery_projects_once_and_wakes_only_the_leader() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        let leader = bot_id(&store, "leader");
        let reviewer = bot_id(&store, "reviewer");
        let posted = post(&store, "leader", "@reviewer review this".into())
            .await
            .expect("post");
        let claim = store
            .claim_next_delivery(&reviewer)
            .await
            .expect("claim")
            .expect("review delivery");
        let session_id = claim.session_id().to_owned();
        drop(claim);

        assert!(
            store
                .settle_delivery(
                    &posted.entry.id,
                    &session_id,
                    &reviewer,
                    SwarmRunOutcome::Succeeded {
                        summary: "Looks good".into(),
                    },
                )
                .await
                .expect("settle")
        );
        assert!(
            !store
                .settle_delivery(
                    &posted.entry.id,
                    &session_id,
                    &reviewer,
                    SwarmRunOutcome::Failed {
                        message: "duplicate".into(),
                    },
                )
                .await
                .expect("idempotent settle")
        );
        let page = store
            .board_page(&swarm.id, None, MAX_PAGE_ENTRIES)
            .await
            .expect("board");
        let outcome = page.entries.first().expect("terminal projection");
        assert_eq!(outcome.author.bot_id, reviewer);
        assert_eq!(outcome.pending_recipient_bot_ids, vec![leader]);
        assert_eq!(
            outcome.in_reply_to_message_id.as_deref(),
            Some(posted.entry.id.as_str())
        );
        assert_eq!(outcome.text, "Looks good");
        assert!(
            store
                .pending_deliveries(&reviewer)
                .await
                .expect("reviewer pending")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn approval_attention_is_nonterminal_and_dedupes_by_request() {
        let (_directory, checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        let reviewer = bot_id(&store, "reviewer");
        let posted = post(&store, "leader", "@reviewer investigate this".into())
            .await
            .expect("post");
        let claim = store
            .claim_next_delivery(&reviewer)
            .await
            .expect("claim")
            .expect("review delivery");
        let session_id = claim.session_id().to_owned();
        drop(claim);
        let request_id = Uuid::new_v4().to_string();

        assert!(
            !store
                .project_attention(
                    &reviewer,
                    &session_id,
                    Some(&Uuid::new_v4().to_string()),
                    &Uuid::new_v4().to_string(),
                    "stale approval",
                )
                .await
                .expect("ignore stale approval")
        );
        assert!(
            store
                .project_attention(
                    &reviewer,
                    &session_id,
                    Some(&posted.entry.id),
                    &request_id,
                    "Approve command",
                )
                .await
                .expect("project attention")
        );
        let (reloaded, _deliveries) = reload(checkpoints, &store);
        assert!(
            !reloaded
                .project_attention(
                    &reviewer,
                    &session_id,
                    Some(&posted.entry.id),
                    &request_id,
                    "Approve command",
                )
                .await
                .expect("dedupe replayed attention")
        );
        assert!(
            reloaded
                .project_attention(
                    &reviewer,
                    &session_id,
                    Some(&posted.entry.id),
                    &Uuid::new_v4().to_string(),
                    "Approve another command",
                )
                .await
                .expect("project distinct attention")
        );
        assert_eq!(
            reloaded
                .pending_deliveries(&reviewer)
                .await
                .expect("original delivery")
                .iter()
                .map(|delivery| delivery.entry.id.as_str())
                .collect::<Vec<_>>(),
            vec![posted.entry.id.as_str()]
        );
        let attention = reloaded
            .pending_user_attention()
            .await
            .expect("pending attention");
        assert_eq!(attention.len(), 2);
        assert!(
            attention
                .iter()
                .all(|request| request.root_source_session_id == "leader-thread")
        );
        assert!(
            reloaded
                .settle_delivery(
                    &posted.entry.id,
                    &session_id,
                    &reviewer,
                    SwarmRunOutcome::Succeeded {
                        summary: "Approved work completed".into(),
                    },
                )
                .await
                .expect("terminal settlement")
        );
        assert!(
            reloaded
                .pending_deliveries(&reviewer)
                .await
                .expect("settled delivery")
                .is_empty()
        );
        assert_eq!(
            reloaded
                .board_page(&swarm.id, None, MAX_PAGE_ENTRIES)
                .await
                .expect("board")
                .entries
                .iter()
                .filter(|entry| { entry.source_event_id.as_deref() == Some(request_id.as_str()) })
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn terminal_and_attention_projections_preserve_the_causal_root_at_the_hop_cap() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        let leader = bot_id(&store, "leader");
        let reviewer = bot_id(&store, "reviewer");
        let first = store
            .post(
                &reviewer,
                "causal-visible-chat",
                "@leader start".into(),
                None,
            )
            .await
            .expect("first hop");
        store
            .acknowledge(&first.entry.id, &leader)
            .await
            .expect("first delivered");
        let second = store
            .post(
                &leader,
                "leader-hidden",
                "@reviewer second".into(),
                Some(first.entry.id.clone()),
            )
            .await
            .expect("second hop");
        store
            .acknowledge(&second.entry.id, &reviewer)
            .await
            .expect("second delivered");
        let third = store
            .post(
                &reviewer,
                "reviewer-hidden",
                "@leader third".into(),
                Some(second.entry.id.clone()),
            )
            .await
            .expect("third hop");
        store
            .acknowledge(&third.entry.id, &leader)
            .await
            .expect("third delivered");
        let fourth = store
            .post(
                &leader,
                "leader-hidden-2",
                "@reviewer fourth".into(),
                Some(third.entry.id.clone()),
            )
            .await
            .expect("fourth hop");
        let claim = store
            .claim_next_delivery(&reviewer)
            .await
            .expect("claim fourth")
            .expect("fourth delivery");
        let session_id = claim.session_id().to_owned();
        drop(claim);

        store
            .project_attention(
                &reviewer,
                &session_id,
                Some(&fourth.entry.id),
                &Uuid::new_v4().to_string(),
                "Decision needed",
            )
            .await
            .expect("attention at cap");
        store
            .settle_delivery(
                &fourth.entry.id,
                &session_id,
                &reviewer,
                SwarmRunOutcome::Succeeded {
                    summary: "Finished".into(),
                },
            )
            .await
            .expect("terminal at cap");

        let page = store
            .board_page(&swarm.id, None, MAX_PAGE_ENTRIES)
            .await
            .expect("board");
        let capped = page
            .entries
            .iter()
            .filter(|entry| entry.reply_depth == MAX_TERMINAL_REPLY_DEPTH)
            .collect::<Vec<_>>();
        assert_eq!(capped.len(), 2);
        assert!(capped.iter().all(|entry| {
            entry.in_reply_to_message_id.as_deref() == Some(fourth.entry.id.as_str())
                && entry.mentioned_recipient_bot_ids.is_empty()
                && entry.pending_recipient_bot_ids.is_empty()
        }));
        assert!(
            store
                .pending_deliveries(&leader)
                .await
                .expect("leader delivery")
                .is_empty()
        );
        let attention = store.pending_user_attention().await.expect("attention");
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].root_source_session_id, "causal-visible-chat");

        let state = store.lock_loaded().await.expect("catalog");
        let mut invalid = state.as_ref().expect("loaded catalog").clone();
        let terminal = invalid
            .swarms
            .get_mut(&swarm.id)
            .expect("swarm")
            .board
            .iter_mut()
            .find(|entry| entry.reply_depth == MAX_TERMINAL_REPLY_DEPTH)
            .expect("terminal projection");
        terminal.mentioned_recipient_bot_ids.push(leader);
        assert!(
            validate_catalog(&invalid)
                .expect_err("routable depth-four entry")
                .to_string()
                .contains("routes beyond")
        );
    }

    #[tokio::test]
    async fn routine_terminal_projection_is_idempotent_and_wakes_the_leader() {
        let (directory, _checkpoints, store, _deliveries) = store();
        let swarm = create_swarm(&store).await;
        let leader = bot_id(&store, "leader");
        let reviewer = bot_id(&store, "reviewer");
        let routine = store
            .bots
            .create_routine(
                &reviewer,
                directory.path(),
                "Check dependencies.",
                RoutineSchedule {
                    kind: crate::wire::RoutineScheduleKind::Once,
                    at: Some(unix_ms() / 1_000 + 3_600),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
                None,
            )
            .expect("create routine");
        let BeginRun::Started(active) = store.bots.begin_run(&routine.id).expect("begin run")
        else {
            panic!("routine should start");
        };
        let run = store
            .bots
            .finish_run(active, RoutineRunStatus::Succeeded, None)
            .expect("finish run");

        assert!(
            store
                .project_routine_outcome(
                    &run,
                    Some("Dependencies are current".into()),
                    Some(directory.path().into())
                )
                .await
                .expect("project routine")
        );
        assert!(
            !store
                .project_routine_outcome(&run, None, Some(directory.path().into()))
                .await
                .expect("idempotent routine projection")
        );
        let entry = store
            .board_page(&swarm.id, None, 1)
            .await
            .expect("board")
            .entries
            .into_iter()
            .next()
            .expect("routine projection");
        assert_eq!(entry.author.bot_id, reviewer);
        assert_eq!(entry.pending_recipient_bot_ids, vec![leader]);
        assert!(entry.text.contains("Dependencies are current"));

        store.bots.delete_run(&run.id).expect("delete run");
        post(&store, "leader", "Prune projection markers".into())
            .await
            .expect("mutate board");
        assert!(
            !store
                .lock_loaded()
                .await
                .expect("catalog")
                .as_ref()
                .expect("loaded catalog")
                .projected_routine_run_ids
                .contains(&run.id)
        );
    }

    #[tokio::test]
    async fn routine_projection_marker_persists_when_the_bot_has_no_swarm() {
        let (directory, checkpoints, store, _deliveries) = store();
        let observer = bot_id(&store, "observer");
        let routine = store
            .bots
            .create_routine(
                &observer,
                directory.path(),
                "Check later.",
                RoutineSchedule {
                    kind: crate::wire::RoutineScheduleKind::Once,
                    at: Some(unix_ms() / 1_000 + 3_600),
                    every_seconds: None,
                    expression: None,
                    time_zone: None,
                },
                None,
            )
            .expect("create routine");
        let BeginRun::Started(active) = store.bots.begin_run(&routine.id).expect("begin run")
        else {
            panic!("routine should start");
        };
        let run = store
            .bots
            .finish_run(active, RoutineRunStatus::Succeeded, None)
            .expect("finish run");

        assert!(
            !store
                .project_routine_outcome(&run, None, Some(directory.path().into()))
                .await
                .expect("record projection without swarm")
        );
        let (reloaded, _deliveries) = reload(checkpoints, &store);
        let swarm = reloaded
            .create(
                "Later team".into(),
                observer.clone(),
                vec![observer, bot_id(&store, "third")],
            )
            .await
            .expect("create later swarm");
        assert!(
            !reloaded
                .project_routine_outcome(&run, None, Some(directory.path().into()))
                .await
                .expect("historical run remains projected")
        );
        assert!(
            reloaded
                .board_page(&swarm.id, None, 1)
                .await
                .expect("board")
                .entries
                .is_empty()
        );
    }

    #[tokio::test]
    async fn user_attention_protects_its_exact_origin_and_can_share_that_chat() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        create_swarm(&store).await;
        let leader = bot_id(&store, "leader");
        let first = store
            .post(
                &leader,
                "causal-visible-chat",
                "Decision one @user".into(),
                None,
            )
            .await
            .expect("first attention");
        let second = store
            .post(
                &leader,
                "causal-visible-chat",
                "Decision two @user".into(),
                None,
            )
            .await
            .expect("second attention");
        let visible_session = Uuid::new_v4().to_string();

        store
            .assign_user_attention(&first.entry.id, &visible_session)
            .await
            .expect("first assignment");
        store
            .assign_user_attention(&second.entry.id, &visible_session)
            .await
            .expect("shared exact assignment");

        assert!(
            store
                .has_pending_source_sessions(&["causal-visible-chat".into()])
                .await
                .expect("pending source")
        );
        assert!(
            store
                .has_pending_source_sessions(&[visible_session])
                .await
                .expect("assigned attention chat")
        );
    }

    #[tokio::test]
    async fn purging_bot_messages_preserves_the_board_sequence_high_water() {
        let (_directory, _checkpoints, store, _deliveries) = store();
        create_swarm(&store).await;
        let first = post(&store, "leader", "first".into()).await.expect("first");
        let removed = post(&store, "reviewer", "removed".into())
            .await
            .expect("removed");
        assert_eq!(removed.entry.sequence, first.entry.sequence + 1);

        store
            .remove_bot(&bot_id(&store, "reviewer"))
            .await
            .expect("purge");
        let next = post(&store, "leader", "next".into()).await.expect("next");

        assert_eq!(next.entry.sequence, removed.entry.sequence + 1);
    }

    #[test]
    fn catalog_encoded_size_is_bounded_before_persistence_or_broadcast() {
        let now = unix_ms();
        let author = SwarmMember {
            bot_id: "leader".into(),
            handle: "leader".into(),
            joined_at_ms: now,
        };
        let board = (1..=64)
            .map(|sequence| BoardEntry {
                id: Uuid::new_v4().to_string(),
                sequence,
                created_at_ms: now,
                author: author.clone(),
                source_session_id: "leader-thread".into(),
                source_event_id: None,
                text: "\0".repeat(MAX_MESSAGE_BYTES),
                mentioned_recipient_bot_ids: Vec::new(),
                pending_recipient_bot_ids: Vec::new(),
                assigned_recipient_session_ids: BTreeMap::new(),
                in_reply_to_message_id: None,
                reply_depth: 0,
            })
            .collect();
        let catalog = Catalog {
            swarms: BTreeMap::from([(
                Uuid::new_v4().to_string(),
                StoredSwarm {
                    title: "Bounded swarm".into(),
                    leader_bot_id: author.bot_id.clone(),
                    members: BTreeMap::from([(author.bot_id, StoredMember { joined_at_ms: now })]),
                    latest_sequence: 64,
                    board,
                    created_at_ms: now,
                    updated_at_ms: now,
                },
            )]),
            ..Catalog::default()
        };

        assert!(
            validate_catalog(&catalog)
                .expect_err("oversized catalog")
                .to_string()
                .contains("encoded bytes")
        );
    }

    #[test]
    fn retention_protects_more_than_one_page_of_attention_chains() {
        let now = unix_ms();
        let author = SwarmMember {
            bot_id: "leader".into(),
            handle: "leader".into(),
            joined_at_ms: now,
        };
        let mut board = VecDeque::new();
        let mut pending_attention = BTreeSet::new();
        for sequence in 0..=MAX_ACKNOWLEDGED_ENTRIES {
            let root_id = format!("root-{sequence}");
            let attention_id = format!("attention-{sequence}");
            board.push_back(BoardEntry {
                id: root_id.clone(),
                sequence: u64::try_from(sequence * 2 + 1).expect("sequence"),
                created_at_ms: now,
                author: author.clone(),
                source_session_id: "causal-chat".into(),
                source_event_id: None,
                text: "Work".into(),
                mentioned_recipient_bot_ids: Vec::new(),
                pending_recipient_bot_ids: Vec::new(),
                assigned_recipient_session_ids: BTreeMap::new(),
                in_reply_to_message_id: None,
                reply_depth: 0,
            });
            board.push_back(BoardEntry {
                id: attention_id.clone(),
                sequence: u64::try_from(sequence * 2 + 2).expect("sequence"),
                created_at_ms: now,
                author: author.clone(),
                source_session_id: "hidden-chat".into(),
                source_event_id: Some(format!("approval-{sequence}")),
                text: "Needs input @user".into(),
                mentioned_recipient_bot_ids: Vec::new(),
                pending_recipient_bot_ids: Vec::new(),
                assigned_recipient_session_ids: BTreeMap::new(),
                in_reply_to_message_id: Some(root_id),
                reply_depth: 1,
            });
            pending_attention.insert(attention_id);
        }
        for sequence in 0..=MAX_ACKNOWLEDGED_ENTRIES {
            board.push_back(BoardEntry {
                id: format!("status-{sequence}"),
                sequence: u64::try_from((MAX_ACKNOWLEDGED_ENTRIES + 1) * 2 + sequence + 1)
                    .expect("sequence"),
                created_at_ms: now,
                author: author.clone(),
                source_session_id: "status-chat".into(),
                source_event_id: None,
                text: "Status".into(),
                mentioned_recipient_bot_ids: Vec::new(),
                pending_recipient_bot_ids: Vec::new(),
                assigned_recipient_session_ids: BTreeMap::new(),
                in_reply_to_message_id: None,
                reply_depth: 0,
            });
        }

        trim_acknowledged(&mut board, &pending_attention);

        assert_eq!(
            board
                .iter()
                .filter(|entry| entry.id.starts_with("root-"))
                .count(),
            MAX_ACKNOWLEDGED_ENTRIES + 1
        );
        assert_eq!(
            board
                .iter()
                .filter(|entry| entry.id.starts_with("attention-"))
                .count(),
            MAX_ACKNOWLEDGED_ENTRIES + 1
        );
        assert_eq!(
            board
                .iter()
                .filter(|entry| entry.id.starts_with("status-"))
                .count(),
            MAX_ACKNOWLEDGED_ENTRIES
        );
    }

    #[test]
    fn mention_parser_ignores_email_boundaries_and_deduplicates() {
        assert_eq!(
            mentioned_handles("@one-bot mail@two @one-bot; @three"),
            BTreeSet::from(["one-bot".into(), "three".into()])
        );
    }
}
