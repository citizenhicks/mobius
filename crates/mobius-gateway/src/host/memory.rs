//! The memory page: every harness's memories, and edits to the editable ones.

use std::collections::BTreeMap;

use super::{Arc, GatewayHost, Rejection, internal, memory_error};
use crate::harness::memory::{self, MemoryContext, list};
use crate::wire::{MemoryRecord, WorkspaceInfo};
use mobius::middleware::memory::Memory;

impl GatewayHost {
    /// Every harness's memories.
    pub(crate) async fn list_memories(&self) -> std::result::Result<Vec<MemoryRecord>, Rejection> {
        let projects = self
            .sessions()
            .await?
            .into_iter()
            .filter_map(|session| {
                let context = session.session_context;
                Some((
                    context.workspace_id?,
                    context.workspace_label.unwrap_or_default(),
                ))
            })
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .map(|(id, path)| WorkspaceInfo {
                id,
                path: path.into(),
            })
            .collect::<Vec<_>>();
        let memory = self.memory_store().await;
        list(&MemoryContext {
            memory: &memory,
            projects: &projects,
        })
        .await
        .map_err(internal)
    }

    async fn memory_store(&self) -> Arc<Memory> {
        Arc::clone(&self.state.lock().await.memory)
    }

    /// Replaces one editable memory's text, then lists every memory.
    pub(crate) async fn edit_memory(
        &self,
        id: &str,
        text: &str,
    ) -> std::result::Result<Vec<MemoryRecord>, Rejection> {
        {
            let _mutation = self.begin_mutation().await?;
            let memory = self.memory_store().await;
            let context = MemoryContext {
                memory: &memory,
                projects: &[],
            };
            memory::edit(&context, id, text).await.map_err(rejection)?;
        }
        self.list_memories().await
    }

    /// Forgets one editable memory, then lists every memory.
    pub(crate) async fn forget_memory(
        &self,
        id: &str,
    ) -> std::result::Result<Vec<MemoryRecord>, Rejection> {
        {
            let _mutation = self.begin_mutation().await?;
            let memory = self.memory_store().await;
            let context = MemoryContext {
                memory: &memory,
                projects: &[],
            };
            memory::forget(&context, id).await.map_err(rejection)?;
        }
        self.list_memories().await
    }
}

fn rejection(error: crate::Error) -> Rejection {
    match error {
        crate::Error::Mobius(error) => memory_error(error),
        error => internal(error),
    }
}
