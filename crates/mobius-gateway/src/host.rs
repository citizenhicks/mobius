//! Per-chat agent ownership, event sequencing, replay, and authenticated operations.

mod bot_events;
mod catalog;
mod deletion;
mod desktop;
mod extensions;
mod files;
pub(crate) mod git;
mod harnesses;
mod live_chats;
mod memory;
mod profile;
mod providers;
mod replay;
mod routines;
mod session;
pub(crate) mod ssh;
mod telemetry;
mod transcript;

pub(crate) use telemetry::SessionActivityReport;

#[cfg(feature = "fixtures")]
pub use transcript::TranscriptReducer;

mod activity;
mod composition;
mod gateway_host;
mod publication;
mod rejection;
mod state;

use std::sync::Arc;
use tokio::sync::Mutex;

const COMMAND_CAPACITY: usize = 128;
const BROADCAST_CAPACITY: usize = 512;
const REPLAY_CAPACITY: usize = 1024;
const REPLAY_LOAD_PAGE_SIZE: usize = 8;
const MAX_REPLAY_BYTES: usize = crate::wire::MAX_FRAME_BYTES;
const SESSION_PAGE_SIZE: usize = 100;
const MAX_SESSION_DELETE_ROOTS: usize = 1_024;
const RECENT_RUN_LIMIT: usize = 30;
const IDLE_QUERY_BATCH: usize = 4;

type SessionActivities = Arc<Mutex<catalog::SessionCatalog>>;

use self::profile::gateway_session_summaries;

pub(crate) use self::replay::{RenderEvents, event_sequence};
pub(crate) use self::session::{HostHandle, SessionRuntimeState};

pub(crate) use activity::{RuntimeActivity, WorkActivity};
use composition::validate_composition;
pub(crate) use composition::validated;

pub(crate) use gateway_host::{GatewayHost, HostAccess};
use memory::with_memory_contribution;
pub(crate) use publication::{GatewayUpdate, SessionCatalogUpdate, finish_publication};
use publication::{send_catalog, send_event, send_warning};
pub(crate) use rejection::{Rejection, internal, invalid_routine, session_file_rejection};
use rejection::{harness_unavailable, stopped_by};
pub(crate) use replay::{OpenReplay, SessionHistoryPage};
use state::GatewayState;

#[cfg(test)]
mod tests;
