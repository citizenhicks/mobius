//! One deadline per middleware, measured from the last foreground turn.

use std::collections::BTreeSet;

use tokio::time::Instant;

use super::MiddlewareStack;
use crate::SessionId;

pub(crate) struct IdleSchedule {
    since: Instant,
    completed: BTreeSet<&'static str>,
}

impl Default for IdleSchedule {
    fn default() -> Self {
        Self {
            since: Instant::now(),
            completed: BTreeSet::new(),
        }
    }
}

impl IdleSchedule {
    pub(crate) fn reset(&mut self) {
        self.since = Instant::now();
        self.completed.clear();
    }

    fn deadlines<'a>(
        &'a self,
        stack: &'a MiddlewareStack,
        session: &'a SessionId,
    ) -> impl Iterator<Item = (&'static str, Instant)> + 'a {
        stack.entries.iter().filter_map(|entry| {
            if self.completed.contains(entry.name()) {
                return None;
            }
            let deadline = self.since.checked_add(entry.idle_after(session)?)?;
            Some((entry.name(), deadline))
        })
    }

    pub(crate) fn deadline(&self, stack: &MiddlewareStack, session: &SessionId) -> Option<Instant> {
        self.deadlines(stack, session)
            .map(|(_, deadline)| deadline)
            .min()
    }

    pub(crate) fn take_due(
        &mut self,
        stack: &MiddlewareStack,
        session: &SessionId,
    ) -> Vec<&'static str> {
        let now = Instant::now();
        let due = self
            .deadlines(stack, session)
            .filter_map(|(name, deadline)| (deadline <= now).then_some(name))
            .collect::<Vec<_>>();
        // An interrupted or failed hook must not spin within the same idle period.
        self.completed.extend(due.iter().copied());
        due
    }
}
