//! Memory pages from every harness's store, as the memory page lists them.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{HarnessKind, WorkspaceInfo};
pub use mobius::middleware::memory::MemoryLink;

/// One memory page. Clients group by `project` (`None` is "No project") and show `global` ones
/// apart; `links` carry resolved target ids, forming the page graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRecord {
    /// The identifier, unique within `harness`.
    pub id: Arc<str>,
    /// The page name other pages link to with `[[name]]`.
    pub name: Arc<str>,
    /// One line on what the page covers.
    pub description: Arc<str>,
    /// Links with gateway-resolved target ids, in order of first mention.
    pub links: Vec<MemoryLink>,
    /// The harness whose store holds it.
    pub harness: HarnessKind,
    /// The project it was written in.
    pub project: Option<WorkspaceInfo>,
    /// The Bot that owns it, when its store records one.
    pub bot_id: Option<Arc<str>>,
    /// Visible to every Bot in every project rather than only its owner in its project.
    pub global: bool,
    /// The Markdown body.
    pub text: Arc<str>,
    /// Unix seconds of the last write.
    pub updated_at: i64,
    /// Whether `edit_memory` and `forget_memory` accept it; Codex memories are read-only.
    pub editable: bool,
}

/// What a Bot's chats remember, resolved from its harness configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotMemory {
    /// Whether its chats read and write memories; a chat may override it.
    pub enabled: bool,
    /// Whether its chats also see global memories.
    pub global: bool,
}
