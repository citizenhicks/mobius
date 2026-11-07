//! Per-turn policy for repeated tool executions without new observable progress.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, MutexGuard};

use serde_json::Value;

use crate::{Error, Result};

const MAX_HOSTED_EXECUTIONS_PER_TURN: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepeatDecision {
    Continue,
    Duplicate,
    Warn,
    Stop,
}

// shortcut: counts live in memory; restore them with durable turns if restart recovery needs them.
pub(super) struct RepeatGuard {
    warning_at: usize,
    stop_at: usize,
    turn: Mutex<TurnRepeats>,
}

#[derive(Default)]
struct TurnRepeats {
    turn_id: String,
    counts: HashMap<u64, usize>,
    hosted: HashSet<(String, String)>,
    stop: bool,
}

impl Default for RepeatGuard {
    fn default() -> Self {
        let (warning_at, stop_at) = Self::default_limits();
        Self::new(warning_at, stop_at)
    }
}

impl RepeatGuard {
    pub(super) fn default_limits() -> (usize, usize) {
        let default = |id| {
            usize::try_from(super::super::manifest::integer_default(
                &super::text::DEFINITION.settings,
                id,
            ))
            .unwrap_or(usize::MAX)
        };
        (
            default("repeat_warning_calls"),
            default("repeat_stop_calls"),
        )
    }

    pub(super) fn new(warning_at: usize, stop_at: usize) -> Self {
        Self {
            warning_at,
            stop_at,
            turn: Mutex::default(),
        }
    }

    /// Hashes a call before its handler takes the arguments.
    pub(super) fn call_identity(name: &str, arguments: &Value) -> DefaultHasher {
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        arguments.hash(&mut hasher);
        hasher
    }

    pub(super) fn hosted_identity(
        name: &str,
        arguments: Option<&Value>,
        result_fingerprint: Option<u64>,
    ) -> Option<DefaultHasher> {
        let arguments = arguments?;
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        match (name, result_fingerprint) {
            // Reworded searches returning the same evidence are not progress.
            ("web_search", Some(_)) => {}
            ("web_search", None) => hash_search_arguments(arguments, &mut hasher),
            // Other tools (e.g. code execution) make progress with new inputs even when
            // their output repeats, so their inputs always count.
            _ => arguments.hash(&mut hasher),
        }
        Some(hasher)
    }

    pub(super) fn result_fingerprint(result: &impl Hash) -> u64 {
        let mut hasher = DefaultHasher::new();
        result.hash(&mut hasher);
        hasher.finish()
    }

    /// Unknown results count only input repetition; they never claim equal output.
    pub(super) fn observe(
        &self,
        turn_id: &str,
        call: Option<DefaultHasher>,
        result_fingerprint: Option<u64>,
        hosted: Option<(&str, &str)>,
    ) -> Result<RepeatDecision> {
        let identity = call.map(|mut call| {
            result_fingerprint.hash(&mut call);
            call.finish()
        });
        let mut turn = self.lock()?;
        if turn.turn_id != turn_id {
            *turn = TurnRepeats {
                turn_id: turn_id.to_owned(),
                ..TurnRepeats::default()
            };
        }
        let duplicate = hosted.is_some_and(|(model_step_id, call_id)| {
            !turn
                .hosted
                .insert((model_step_id.to_owned(), call_id.to_owned()))
        });
        if duplicate {
            return Ok(RepeatDecision::Duplicate);
        }
        if turn.hosted.len() > MAX_HOSTED_EXECUTIONS_PER_TURN {
            turn.stop = true;
            return Err(Error::Stopped(
                "provider exceeded the per-turn hosted execution limit".into(),
            ));
        }
        let count = identity.map(|identity| {
            let count = turn.counts.entry(identity).or_default();
            *count = count.saturating_add(1);
            *count
        });
        turn.stop |= count.is_some_and(|count| count >= self.stop_at);
        let decision = if turn.stop {
            RepeatDecision::Stop
        } else if count == Some(self.warning_at) {
            RepeatDecision::Warn
        } else {
            RepeatDecision::Continue
        };
        drop(turn);
        Ok(decision)
    }

    pub(super) fn should_stop(&self, turn_id: &str) -> Result<bool> {
        let turn = self.lock()?;
        Ok(turn.stop && turn.turn_id == turn_id)
    }

    pub(super) fn finish_turn(&self, turn_id: &str) -> Result<()> {
        let mut turn = self.lock()?;
        if turn.turn_id == turn_id {
            *turn = TurnRepeats::default();
        }
        drop(turn);
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, TurnRepeats>> {
        self.turn
            .lock()
            .map_err(|error| Error::Tool(format!("repeat guard lock is poisoned: {error}")))
    }
}

fn hash_search_arguments(arguments: &Value, hasher: &mut impl Hasher) {
    std::mem::discriminant(arguments).hash(hasher);
    let Some(fields) = arguments.as_object() else {
        arguments.hash(hasher);
        return;
    };
    fields.len().hash(hasher);
    for (key, value) in fields {
        key.hash(hasher);
        std::mem::discriminant(value).hash(hasher);
        match (key.as_str(), value) {
            ("query", Value::String(query)) => normalize_query(query).hash(hasher),
            ("queries", Value::Array(queries)) => {
                queries.len().hash(hasher);
                for query in queries {
                    std::mem::discriminant(query).hash(hasher);
                    match query {
                        Value::String(query) => normalize_query(query).hash(hasher),
                        query => query.hash(hasher),
                    }
                }
            }
            _ => value.hash(hasher),
        }
    }
}

fn normalize_query(query: &str) -> String {
    let mut normalized = String::with_capacity(query.len());
    let mut quote = None;
    let mut escaped = false;
    let mut space = false;
    for character in query.chars() {
        if escaped {
            normalized.push(character);
            escaped = false;
            continue;
        }
        if let Some(delimiter) = quote {
            normalized.push(character);
            if character == '\\' {
                escaped = true;
            } else if character == delimiter {
                quote = None;
            }
            continue;
        }
        if character.is_whitespace() {
            space = !normalized.is_empty();
            continue;
        }
        if space {
            normalized.push(' ');
        }
        if character == '"'
            || (character == '\''
                && normalized
                    .chars()
                    .next_back()
                    .is_none_or(|previous| !previous.is_alphanumeric()))
        {
            quote = Some(character);
        }
        normalized.push(character);
        escaped = character == '\\';
        space = false;
    }
    normalized
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn identity(name: &str, arguments: &Value) -> DefaultHasher {
        RepeatGuard::call_identity(name, arguments)
    }

    #[test]
    fn repeats_warn_once_then_stop_and_reset_with_the_turn() {
        let guard = RepeatGuard::new(2, 3);
        let same = json!({"command": "echo ok"});
        let call = |turn| {
            guard
                .observe(turn, Some(identity("bash", &same)), Some(1), None)
                .unwrap()
        };

        assert_eq!(call("a"), RepeatDecision::Continue);
        // Real work between repeats does not reset the count.
        assert_eq!(
            guard
                .observe(
                    "a",
                    Some(identity("bash", &json!({"command": "ls"}))),
                    Some(1),
                    None,
                )
                .unwrap(),
            RepeatDecision::Continue
        );
        assert_eq!(call("a"), RepeatDecision::Warn);
        assert!(!guard.should_stop("a").unwrap());
        assert_eq!(call("a"), RepeatDecision::Stop);
        assert!(guard.should_stop("a").unwrap());

        assert!(!guard.should_stop("b").unwrap());
        assert_eq!(call("b"), RepeatDecision::Continue);
        guard.finish_turn("a").unwrap();
        assert_eq!(call("b"), RepeatDecision::Warn);
        guard.finish_turn("b").unwrap();
        assert_eq!(call("b"), RepeatDecision::Continue);
    }

    #[test]
    fn interleaved_parent_and_child_turns_keep_their_counts() {
        let parent = RepeatGuard::new(2, 3);
        let child = RepeatGuard::new(2, 3);
        let arguments = json!({"command": "echo ok"});
        let call = |guard: &RepeatGuard| {
            guard
                .observe("turn", Some(identity("bash", &arguments)), Some(1), None)
                .unwrap()
        };

        assert_eq!(call(&parent), RepeatDecision::Continue);
        assert_eq!(call(&child), RepeatDecision::Continue);
        assert_eq!(call(&parent), RepeatDecision::Warn);
        assert_eq!(call(&child), RepeatDecision::Warn);
        assert_eq!(call(&parent), RepeatDecision::Stop);
        assert!(parent.should_stop("turn").unwrap());
        assert!(!child.should_stop("turn").unwrap());
        assert_eq!(call(&child), RepeatDecision::Stop);
        child.finish_turn("turn").unwrap();
        assert!(parent.should_stop("turn").unwrap());
        assert!(!child.should_stop("turn").unwrap());
    }

    #[test]
    fn hosted_completions_count_once_per_attempt_and_keep_unknown_results_distinct() {
        let guard = RepeatGuard::new(2, 3);
        let arguments = json!({"query":"rust docs"});
        let call = |step, id, output| {
            guard
                .observe(
                    "turn",
                    Some(identity("web_search", &arguments)),
                    output,
                    Some((step, id)),
                )
                .unwrap()
        };
        assert_eq!(call("step-1", "call", None), RepeatDecision::Continue);
        assert_eq!(call("step-1", "call", None), RepeatDecision::Duplicate);
        assert_eq!(
            call("step-1", "different", Some(0)),
            RepeatDecision::Continue
        );
        assert_eq!(call("step-2", "call", None), RepeatDecision::Warn);
        assert_eq!(call("step-2", "call", None), RepeatDecision::Duplicate);
        assert_eq!(call("step-3", "call", None), RepeatDecision::Stop);
    }

    #[test]
    fn hosted_rewording_with_the_same_evidence_does_not_count_as_progress() {
        let guard = RepeatGuard::new(2, 3);
        let call = |id, arguments: &Value, result| {
            guard
                .observe(
                    "turn",
                    RepeatGuard::hosted_identity("web_search", Some(arguments), result),
                    result,
                    Some(("step", id)),
                )
                .unwrap()
        };
        assert_eq!(
            call("1", &json!({"query":"rust docs"}), Some(1)),
            RepeatDecision::Continue
        );
        assert_eq!(
            call("2", &json!({"query":"rust documentation"}), Some(1)),
            RepeatDecision::Warn
        );
        assert_eq!(
            call("3", &json!({"query":"rust docs"}), Some(2)),
            RepeatDecision::Continue
        );
        assert_eq!(
            call("4", &json!({"query":"rust reference"}), Some(1)),
            RepeatDecision::Stop
        );
        let code = |source| {
            RepeatGuard::hosted_identity("code_execution", Some(&json!({"code": source})), Some(1))
                .unwrap()
                .finish()
        };
        assert_ne!(code("write_a()"), code("write_b()"));
    }

    #[test]
    fn search_normalization_preserves_quoted_text_urls_and_filter_changes() {
        for (query, expected) in [
            ("  rust\t docs  ", "rust docs"),
            ("rust  \"a  b\"  docs", "rust \"a  b\" docs"),
            ("site:'a  b'  docs", "site:'a  b' docs"),
            ("what's  new", "what's new"),
            ("a\\  b", "a\\  b"),
            (
                " https://example.test/A?q=B&c=D  ",
                "https://example.test/A?q=B&c=D",
            ),
        ] {
            assert_eq!(normalize_query(query), expected);
        }
        let hash = |name, arguments: &Value| {
            RepeatGuard::hosted_identity(name, Some(arguments), None)
                .unwrap()
                .finish()
        };
        let first = json!({"queries":["rust  docs"],"filter":"one"});
        let equivalent = json!({"queries":[" rust docs "],"filter":"one"});
        let changed = json!({"queries":["rust docs"],"filter":"two"});
        assert_eq!(hash("web_search", &first), hash("web_search", &equivalent));
        assert_ne!(hash("web_search", &first), hash("web_search", &changed));
        assert_ne!(
            hash("find_in_page", &first),
            hash("find_in_page", &equivalent)
        );
    }

    #[test]
    fn opaque_hosted_arguments_do_not_claim_repetition_and_ingress_is_bounded() {
        let guard = RepeatGuard::new(1, 1);
        assert!(RepeatGuard::hosted_identity("web_search", None, Some(42)).is_none());
        for index in 0..MAX_HOSTED_EXECUTIONS_PER_TURN {
            assert_eq!(
                guard
                    .observe("turn", None, None, Some(("step", &index.to_string())))
                    .unwrap(),
                RepeatDecision::Continue
            );
        }
        assert!(
            guard
                .observe("turn", None, None, Some(("step", "overflow")))
                .is_err()
        );
        assert!(guard.should_stop("turn").unwrap());
    }

    #[test]
    fn a_changed_result_is_not_a_repeat() {
        let guard = RepeatGuard::new(1, 2);
        let arguments = json!({"command": "date"});
        assert_eq!(
            guard
                .observe("a", Some(identity("bash", &arguments)), Some(1), None,)
                .unwrap(),
            RepeatDecision::Warn
        );
        assert_eq!(
            guard
                .observe("a", Some(identity("bash", &arguments)), Some(2), None)
                .unwrap(),
            RepeatDecision::Warn
        );
        assert!(!guard.should_stop("a").unwrap());
    }
}
