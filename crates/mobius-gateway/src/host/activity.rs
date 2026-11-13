//! Runtime activity revisions and neutral idle observations.

use super::gateway_host::GatewayHost;
use super::rejection::{Rejection, internal};

use super::IDLE_QUERY_BATCH;

use chrono::Utc;
use futures_util::future::try_join_all;
use std::sync::atomic::Ordering;
use tokio::sync::watch;
use uuid::Uuid;
pub(crate) struct WorkActivity {
    instance: Uuid,
    revision: watch::Sender<u64>,
}

impl WorkActivity {
    pub(crate) fn new() -> Self {
        Self {
            instance: Uuid::new_v4(),
            revision: watch::Sender::new(0),
        }
    }

    pub(crate) fn mark(&self) {
        self.revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub(crate) fn wake(&self) {
        // Storage commits need a fresh snapshot, not another execution grace period.
        self.revision.send_modify(|_| {});
    }
}

pub(crate) struct RuntimeActivity {
    pub(crate) idle: bool,
    pub(crate) activity_revision: String,
    pub(crate) next_routine_at: Option<String>,
    pub(crate) active_sessions: usize,
    pub(crate) running_routines: u64,
}

impl GatewayHost {
    pub(crate) async fn runtime_activity(&self) -> std::result::Result<RuntimeActivity, Rejection> {
        let before = *self.work_activity.revision.borrow();
        // Keep the registry stable while asking each resident actor. Actor idle
        // checks never acquire GatewayState; hidden routine sessions are included.
        let state = self.state.lock().await;
        let starting = !state
            .starting_sessions
            .lock()
            .map_err(|error| internal(format!("session startup lock is poisoned: {error}")))?
            .is_empty();
        let mut idle = !starting
            && !state
                .bots
                .storage
                .has_running_routines()
                .map_err(internal)?
            && !state
                .bots
                .storage
                .has_pending_deliveries()
                .map_err(internal)?;
        let resident: Vec<_> = state
            .sessions
            .values()
            .filter(|session| session.inner.alive.load(Ordering::Acquire))
            .collect();
        let mut active_sessions = 0;
        // Each overlapping query past the first opens its own SQLite connection, so the overlap stays small.
        for batch in resident.chunks(IDLE_QUERY_BATCH) {
            let settled =
                try_join_all(batch.iter().map(|session| session.runtime_is_idle())).await?;
            active_sessions += settled.iter().filter(|idle| !**idle).count();
        }
        idle &= active_sessions == 0;
        // Routine reservations happen outside the registry lock; observe them
        // again and reject an idle result if any work changed during this query.
        let running_routines = state
            .bots
            .storage
            .running_routine_count()
            .map_err(internal)?;
        let now = Utc::now().timestamp();
        let next_routine_at = state.bots.next_routine_at(now).map_err(internal)?;
        idle &= running_routines == 0
            && next_routine_at.is_none_or(|at| at.timestamp() > now)
            && !state
                .bots
                .storage
                .has_pending_deliveries()
                .map_err(internal)?;
        drop(state);
        let after = *self.work_activity.revision.borrow();
        Ok(RuntimeActivity {
            active_sessions,
            running_routines,
            idle: idle && before == after,
            activity_revision: format!("{}:{after}", self.work_activity.instance),
            next_routine_at: next_routine_at.map(|at| at.to_rfc3339()),
        })
    }

    pub(crate) fn mark_runtime_activity(&self) {
        self.work_activity.mark();
    }

    pub(crate) fn activity_changes(&self) -> watch::Receiver<u64> {
        self.work_activity.revision.subscribe()
    }
}
