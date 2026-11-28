//! Per-batch and per-call sandbox authority.

use crate::SessionId;
use std::collections::BTreeSet;
use std::sync::Arc;

use super::NetworkAccess;
use super::SandboxMode;
use crate::Error;
use crate::Result;

/// Batch authority issued by the sandbox approval policy.
#[derive(Debug)]
pub(crate) struct SandboxPermissions {
    pub(super) session_id: SessionId,
    pub(super) sandbox_mode: SandboxMode,
    pub(super) network_access: NetworkAccess,
    pub(super) mutation_call_ids: BTreeSet<Arc<str>>,
}

impl SandboxPermissions {
    pub(super) fn new(
        session_id: SessionId,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
        mutation_call_ids: impl IntoIterator<Item = Arc<str>>,
    ) -> Self {
        Self {
            session_id,
            sandbox_mode,
            network_access,
            mutation_call_ids: mutation_call_ids.into_iter().collect(),
        }
    }

    pub(crate) fn restore(
        session_id: &SessionId,
        sandbox_mode: SandboxMode,
        network_access: NetworkAccess,
        mutation_call_ids: impl IntoIterator<Item = Arc<str>>,
    ) -> Self {
        Self::new(
            session_id.share(),
            sandbox_mode,
            network_access,
            mutation_call_ids,
        )
    }

    pub(crate) fn sandbox_mode(&self) -> SandboxMode {
        self.sandbox_mode
    }

    pub(crate) fn network_access(&self) -> NetworkAccess {
        self.network_access
    }

    pub(crate) fn mutation_call_ids(&self) -> Vec<Arc<str>> {
        self.mutation_call_ids.iter().map(Arc::clone).collect()
    }

    pub(crate) fn for_call(&self, call_id: &str) -> ToolPermissions {
        ToolPermissions {
            session_id: self.session_id.share(),
            sandbox_mode: self.sandbox_mode,
            network_access: self.network_access,
            mutation: self.mutation_call_ids.contains(call_id),
        }
    }

    pub(super) fn allow_mutations(&mut self, call_ids: impl IntoIterator<Item = Arc<str>>) {
        self.mutation_call_ids.extend(call_ids);
    }
}

/// Opaque authority attached to exactly one tool call.
#[derive(Debug)]
pub struct ToolPermissions {
    pub(super) session_id: SessionId,
    pub(super) sandbox_mode: SandboxMode,
    pub(super) network_access: NetworkAccess,
    pub(super) mutation: bool,
}

impl ToolPermissions {
    pub(crate) fn sandbox_mode(&self) -> SandboxMode {
        self.sandbox_mode
    }

    pub(crate) fn network_access(&self) -> NetworkAccess {
        self.network_access
    }

    pub(crate) fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub(crate) fn allows_mutation(&self) -> bool {
        self.mutation
    }

    /// Returns the permit sandbox mutations require, when this call has mutation authority.
    /// # Errors
    ///
    /// Returns an error when the call was not authorized to mutate the workspace.
    pub fn mutation(&self) -> Result<MutationPermit<'_>> {
        if !self.mutation {
            return Err(Error::Sandbox(
                "tool call is not authorized to mutate the workspace".into(),
            ));
        }
        Ok(MutationPermit(self))
    }
}

/// Proof that one tool call may mutate the workspace; only [`ToolPermissions::mutation`] makes it.
#[derive(Debug, Clone, Copy)]
pub struct MutationPermit<'a>(pub(super) &'a ToolPermissions);

pub(crate) enum SandboxAuthorization {
    Execute(SandboxPermissions),
    Approval {
        request: SandboxApprovalRequest,
        permissions: SandboxPermissions,
    },
}

pub(crate) struct SandboxApprovalRequest {
    pub(crate) id: Arc<str>,
    pub(crate) reason: String,
    pub(crate) call_ids: Vec<Arc<str>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_permit_requires_per_call_authority() {
        let permissions = SandboxPermissions::new(
            "session".parse().expect("session ID"),
            SandboxMode::WorkspaceWrite,
            NetworkAccess::Allowed,
            Vec::new(),
        );
        assert!(permissions.for_call("call").mutation().is_err());
    }
}
