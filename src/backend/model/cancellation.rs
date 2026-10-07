use std::sync::OnceLock;

/// Local reason for ending an unfinished model request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelCancellationReason {
    /// The request future was dropped without a more specific cause.
    #[default]
    RequestDropped,
    /// The active turn received an explicit interrupt.
    Interrupted,
    /// The frontend's submission channel closed.
    FrontendDisconnected,
    /// The selected provider's credential expired or was revoked.
    CredentialExpired,
    /// Request processing failed before the provider finished.
    RequestFailed,
}

impl ModelCancellationReason {
    /// Returns the finite, non-sensitive reason used in transport diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequestDropped => "request_dropped",
            Self::Interrupted => "interrupted",
            Self::FrontendDisconnected => "frontend_disconnected",
            Self::CredentialExpired => "credential_expired",
            Self::RequestFailed => "request_failed",
        }
    }
}

/// A borrowed request-local cancellation cause, set before its future is dropped.
#[derive(Debug, Default)]
pub struct ModelCancellation(OnceLock<ModelCancellationReason>);

impl ModelCancellation {
    /// Records the first specific cause without replacing an earlier cause.
    /// This does not stop a request; its caller must also drop the response future.
    pub fn record(&self, reason: ModelCancellationReason) {
        if reason != ModelCancellationReason::RequestDropped {
            let _ = self.0.set(reason);
        }
    }

    /// Copy an enclosing cause before this request's pinned provider future is dropped.
    /// Declare this guard after pinning that future so reverse drop order preserves diagnostics.
    pub(super) fn inherit_on_drop<'a>(&'a self, parent: Option<&'a Self>) -> impl Drop + 'a {
        CancellationInheritance {
            local: self,
            parent,
        }
    }

    /// Returns the recorded cause, or [`ModelCancellationReason::RequestDropped`].
    #[must_use]
    pub fn reason(&self) -> ModelCancellationReason {
        self.0.get().copied().unwrap_or_default()
    }
}

struct CancellationInheritance<'a> {
    local: &'a ModelCancellation,
    parent: Option<&'a ModelCancellation>,
}

impl Drop for CancellationInheritance<'_> {
    fn drop(&mut self) {
        if let Some(parent) = self.parent {
            self.local.record(parent.reason());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specific_cause_survives_later_generic_failure() {
        let cancellation = ModelCancellation::default();
        assert_eq!(
            cancellation.reason(),
            ModelCancellationReason::RequestDropped
        );
        cancellation.record(ModelCancellationReason::RequestDropped);
        cancellation.record(ModelCancellationReason::CredentialExpired);
        cancellation.record(ModelCancellationReason::RequestDropped);
        cancellation.record(ModelCancellationReason::RequestFailed);
        assert_eq!(
            cancellation.reason(),
            ModelCancellationReason::CredentialExpired
        );
    }
}
