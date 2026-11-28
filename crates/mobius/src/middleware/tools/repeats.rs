//! Per-turn policy for repeated tool executions without new observable progress.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, MutexGuard};

use serde_json::Value;

use crate::{Error, Result};

const MAX_HOSTED_EXECUTIONS_PER_TURN: usize = 1_024;
const MAX_TRACKED_TURNS: usize = 256;

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
    turns: Mutex<HashMap<String, TurnRepeats>>,
}

#[derive(Default)]
struct TurnRepeats {
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
            turns: Mutex::default(),
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
        let mut turns = self.lock()?;
        if !turns.contains_key(turn_id) && turns.len() >= MAX_TRACKED_TURNS {
            turns.clear();
        }
        let turn = turns.entry(turn_id.to_owned()).or_default();
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
        drop(turns);
        Ok(decision)
    }

    pub(super) fn should_stop(&self, turn_id: &str) -> Result<bool> {
        let turns = self.lock()?;
        Ok(turns.get(turn_id).is_some_and(|turn| turn.stop))
    }

    pub(super) fn finish_turn(&self, turn_id: &str) -> Result<()> {
        let mut turns = self.lock()?;
        turns.remove(turn_id);
        drop(turns);
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<String, TurnRepeats>>> {
        self.turns
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
mod tests;
