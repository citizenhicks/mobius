//! Per-turn guard against identical tool calls that keep returning identical results.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, MutexGuard};

use serde_json::Value;

use crate::protocol::ToolContent;
use crate::{Error, Result, SessionId};

// ponytail: counts live in memory, so a gateway restart mid-turn only restarts the count.
pub(super) struct RepeatGuard {
    warning_at: usize,
    stop_at: usize,
    sessions: Mutex<HashMap<SessionId, TurnRepeats>>,
}

#[derive(Default)]
struct TurnRepeats {
    turn_id: String,
    counts: HashMap<u64, usize>,
    stop: bool,
}

impl RepeatGuard {
    pub(super) fn new(warning_at: usize, stop_at: usize) -> Self {
        Self {
            warning_at,
            stop_at,
            sessions: Mutex::default(),
        }
    }

    /// Counts one executed call across the whole turn, because real work may sit between
    /// repeats. Returns whether this call reached the warning threshold.
    pub(super) fn observe(
        &self,
        session_id: &SessionId,
        turn_id: &str,
        name: &str,
        arguments: &Value,
        output: &ToolContent,
    ) -> Result<bool> {
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        arguments.to_string().hash(&mut hasher);
        serde_json::to_vec(output)?.hash(&mut hasher);
        let mut sessions = self.lock()?;
        let turn = sessions.entry(session_id.share()).or_default();
        if turn.turn_id != turn_id {
            *turn = TurnRepeats {
                turn_id: turn_id.to_owned(),
                ..TurnRepeats::default()
            };
        }
        let count = turn.counts.entry(hasher.finish()).or_default();
        *count += 1;
        let count = *count;
        turn.stop |= count >= self.stop_at;
        drop(sessions);
        Ok(count == self.warning_at)
    }

    pub(super) fn should_stop(&self, session_id: &SessionId, turn_id: &str) -> Result<bool> {
        Ok(self
            .lock()?
            .get(session_id)
            .is_some_and(|turn| turn.stop && turn.turn_id == turn_id))
    }

    pub(super) fn finish_turn(&self, session_id: &SessionId, turn_id: &str) -> Result<()> {
        let mut sessions = self.lock()?;
        if sessions
            .get(session_id)
            .is_some_and(|turn| turn.turn_id == turn_id)
        {
            sessions.remove(session_id);
        }
        drop(sessions);
        Ok(())
    }

    pub(super) fn finish_session(&self, session_id: &SessionId) -> Result<()> {
        self.lock()?.remove(session_id);
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<SessionId, TurnRepeats>>> {
        self.sessions
            .lock()
            .map_err(|error| Error::Tool(format!("repeat guard lock is poisoned: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn repeats_warn_once_then_stop_and_reset_with_the_turn() {
        let guard = RepeatGuard::new(2, 3);
        let session = SessionId::new("session").unwrap();
        let same = json!({"command": "echo ok"});
        let output = ToolContent::default();
        let call = |turn| {
            guard
                .observe(&session, turn, "bash", &same, &output)
                .unwrap()
        };

        assert!(!call("a"));
        // Real work between repeats does not reset the count.
        assert!(
            !guard
                .observe(&session, "a", "bash", &json!({"command": "ls"}), &output)
                .unwrap()
        );
        assert!(call("a"));
        assert!(!guard.should_stop(&session, "a").unwrap());
        assert!(!call("a"));
        assert!(guard.should_stop(&session, "a").unwrap());

        assert!(!guard.should_stop(&session, "b").unwrap());
        assert!(!call("b"));
        assert!(!guard.should_stop(&session, "b").unwrap());
        guard.finish_turn(&session, "a").unwrap();
        assert!(!guard.lock().unwrap().is_empty());
        guard.finish_session(&session).unwrap();
        assert!(guard.lock().unwrap().is_empty());
    }

    #[test]
    fn interleaved_parent_and_child_turns_keep_their_counts() {
        let guard = RepeatGuard::new(2, 3);
        let parent = SessionId::new("parent").unwrap();
        let child = SessionId::new("child").unwrap();
        let arguments = json!({"command": "echo ok"});
        let output = ToolContent::default();
        let call = |session: &SessionId| {
            guard
                .observe(session, session.as_str(), "bash", &arguments, &output)
                .unwrap()
        };

        assert!(!call(&parent));
        assert!(!call(&child));
        assert!(call(&parent), "the child must not reset its parent's count");
        assert!(call(&child));
        assert!(!call(&parent));
        assert!(guard.should_stop(&parent, "parent").unwrap());
        assert!(!guard.should_stop(&child, "child").unwrap());
        assert!(!call(&child));
        assert!(guard.should_stop(&parent, "parent").unwrap());
        assert!(guard.should_stop(&child, "child").unwrap());

        guard.finish_turn(&child, "child").unwrap();
        assert!(!guard.should_stop(&child, "child").unwrap());
        assert!(guard.should_stop(&parent, "parent").unwrap());
        guard.finish_session(&parent).unwrap();
        assert!(guard.lock().unwrap().is_empty());
    }

    #[test]
    fn a_changed_result_is_not_a_repeat() {
        let guard = RepeatGuard::new(1, 1);
        let session = SessionId::new("session").unwrap();
        let arguments = json!({"command": "date"});
        assert!(
            guard
                .observe(&session, "a", "bash", &arguments, &ToolContent::default())
                .unwrap()
        );
        let changed: ToolContent = "later".to_owned().into();
        assert!(
            guard
                .observe(&session, "a", "bash", &arguments, &changed)
                .unwrap()
        );
    }
}
