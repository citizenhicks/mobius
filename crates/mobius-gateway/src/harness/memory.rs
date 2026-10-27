//! Every harness's memories, listed through one hook per harness.
//!
//! The möbius store is read and edited here; a harness with its own native store lists it by
//! filling its entry in [`source`].

use std::path::PathBuf;
use std::sync::Arc;

use ::mobius::BoxFuture;
use ::mobius::middleware::memory::{Memory, MemoryItem};

use crate::Result;
use crate::wire::{HarnessKind, MemoryRecord, WorkspaceInfo};

/// What a harness's memory listing may read.
pub(crate) struct MemoryContext<'a> {
    /// The möbius memory store.
    pub(crate) memory: &'a Memory,
    /// Every project a chat on this gateway uses, as the chat list shows it.
    pub(crate) projects: &'a [WorkspaceInfo],
}

/// One harness's memory store, as the memory page lists it.
pub(crate) trait MemorySource: Sync {
    /// Every memory this store holds.
    fn list<'a>(
        &'a self,
        context: &'a MemoryContext<'a>,
    ) -> BoxFuture<'a, Result<Vec<MemoryRecord>>>;
}

/// The registry: each harness's memory store, or `None` without one on this gateway.
fn source(kind: HarnessKind) -> Option<&'static dyn MemorySource> {
    match kind {
        HarnessKind::Mobius => Some(&MobiusMemories),
        HarnessKind::Codex | HarnessKind::Claude => None,
    }
}

/// Every harness's memories: möbius, then Codex, then Claude.
pub(crate) async fn list(context: &MemoryContext<'_>) -> Result<Vec<MemoryRecord>> {
    let mut memories = Vec::new();
    for source in [HarnessKind::Mobius, HarnessKind::Codex, HarnessKind::Claude]
        .into_iter()
        .filter_map(source)
    {
        memories.extend(source.list(context).await?);
    }
    Ok(memories)
}

/// Replaces the text of the editable memory `id`.
pub(crate) async fn edit(context: &MemoryContext<'_>, id: &str, text: &str) -> Result<()> {
    Ok(context.memory.edit(id, text).await?)
}

/// Forgets the editable memory `id`.
pub(crate) async fn forget(context: &MemoryContext<'_>, id: &str) -> Result<()> {
    Ok(context.memory.forget(id).await?)
}

struct MobiusMemories;

impl MemorySource for MobiusMemories {
    fn list<'a>(
        &'a self,
        context: &'a MemoryContext<'a>,
    ) -> BoxFuture<'a, Result<Vec<MemoryRecord>>> {
        Box::pin(async move {
            Ok(context
                .memory
                .list()
                .await?
                .into_iter()
                .map(|item| mobius_record(item, context.projects))
                .collect())
        })
    }
}

/// `item` on the wire, in the chat list's project when one still uses it. A global memory is
/// in no project: it keeps only the Bot that wrote it.
fn mobius_record(item: MemoryItem, projects: &[WorkspaceInfo]) -> MemoryRecord {
    let owner = item.owner.as_deref();
    MemoryRecord {
        id: item.id,
        harness: HarnessKind::Mobius,
        project: owner
            .filter(|_| !item.global)
            .and_then(|owner| owner.project.as_ref())
            .map(|project| WorkspaceInfo {
                id: (*project.id).into(),
                path: projects
                    .iter()
                    .find(|known| *known.id == *project.id)
                    .map_or_else(
                        || {
                            project
                                .label
                                .as_deref()
                                .map_or_else(PathBuf::new, PathBuf::from)
                        },
                        |known| known.path.clone(), // rules-allow: each record owns its project path
                    ),
            }),
        bot_id: owner.map(|owner| Arc::clone(&owner.bot)),
        global: item.global,
        text: item.text,
        updated_at: i64::try_from(item.updated_at).unwrap_or(i64::MAX),
        editable: true,
    }
}
