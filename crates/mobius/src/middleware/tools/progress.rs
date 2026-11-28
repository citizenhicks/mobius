//! Provider-hosted tool work: repeat policy for each response and a bounded factual history
//! retained across compaction. The agent loop only forwards model events here.

use std::hash::Hasher as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Catalog, RepeatDecision};
use crate::backend::model::{ModelInput, internal_user_message};
use crate::protocol::{
    HostedToolExecution, ModelEvent, content_parts, content_parts_mut, internal_message_kind,
};
use crate::{Error, Result};

const KIND: &str = "tool_progress";
const MAX_BYTES: usize = 8 * 1_024;
const MAX_RECORDS: usize = 32;
const MAX_ARGUMENT_BYTES: usize = 512;
const MAX_SOURCES: usize = 4;
const MAX_SOURCE_BYTES: usize = 512;

/// Applies the tools capability's hosted-execution policy to one model step's responses.
pub(crate) struct HostedObserver {
    catalog: Arc<Catalog>,
    turn_id: Arc<str>,
    progress: Mutex<ToolProgress>,
    warned: AtomicBool,
}

impl HostedObserver {
    pub(crate) fn new(
        catalog: Arc<Catalog>,
        turn_id: Arc<str>,
        input: ModelInput<'_>,
    ) -> Result<Self> {
        Ok(Self {
            catalog,
            turn_id,
            progress: Mutex::new(ToolProgress::from_input(input)?),
            warned: AtomicBool::new(false),
        })
    }

    /// Consumes hosted execution events and returns every other event unchanged.
    ///
    /// # Errors
    ///
    /// Stops the response when the turn's repeat limit is reached.
    pub(crate) fn observe(
        &self,
        model_step_id: &str,
        event: ModelEvent,
    ) -> Result<Option<ModelEvent>> {
        match event {
            ModelEvent::HostedToolStarted { .. } => {
                self.stop_if_limited()?;
                Ok(None)
            }
            ModelEvent::HostedToolCompleted(execution) => {
                let decision =
                    self.catalog
                        .observe_hosted(&self.turn_id, model_step_id, &execution)?;
                if decision != RepeatDecision::Duplicate {
                    self.progress()?.record(execution)?;
                }
                match decision {
                    RepeatDecision::Warn => self.warned.store(true, Ordering::Relaxed),
                    RepeatDecision::Stop => self.stop_if_limited()?,
                    RepeatDecision::Continue | RepeatDecision::Duplicate => {}
                }
                Ok(None)
            }
            event => Ok(Some(event)),
        }
    }

    /// Returns the context items this response's hosted work adds: a progress delta and,
    /// when the repeat limit warned, the repeat notice.
    pub(crate) fn take_input(&self) -> Result<Vec<Value>> {
        let mut input = Vec::new();
        input.extend(self.progress()?.take_message()?);
        if self.warned.swap(false, Ordering::Relaxed) {
            input.push(super::repeat_notice());
        }
        Ok(input)
    }

    fn stop_if_limited(&self) -> Result<()> {
        match self.catalog.repeat_stop(&self.turn_id)? {
            Some(reason) => Err(Error::Stopped(reason.to_owned())),
            None => Ok(()),
        }
    }

    fn progress(&self) -> Result<MutexGuard<'_, ToolProgress>> {
        self.progress
            .lock()
            .map_err(|error| Error::Stopped(format!("tool progress is unavailable: {error}")))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolProgress {
    records: Vec<Record>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    name: Arc<str>,
    arguments: Option<Value>,
    arguments_omitted: bool,
    result_fingerprint: Option<u64>,
    sources: Vec<String>,
    sources_omitted: bool,
    calls: usize,
    /// Changed since the last appended message; only changed records are re-sent.
    #[serde(skip)]
    touched: bool,
}

#[derive(Serialize)]
struct LedgerView<'a> {
    records: &'a [Record],
}

impl ToolProgress {
    /// Folds every appended progress message in order; later records replace earlier ones.
    fn from_input(input: ModelInput<'_>) -> Result<Self> {
        let mut progress = Self::default();
        for item in input.iter().filter(|item| is_progress(item)) {
            for record in Self::parse_message(item)?.records {
                progress.insert(record);
            }
        }
        progress.trim()?;
        Ok(progress)
    }

    fn insert(&mut self, mut record: Record) {
        if !self.records.is_empty()
            && let Some(identity) = record.identity()
            && let Some(index) = self
                .records
                .iter()
                .position(|previous| previous.identity() == Some(identity))
        {
            let previous = self.records.remove(index);
            // Restored deltas already contain cumulative counts and sources.
            if record.touched {
                record.include_previous(previous);
            }
        }
        self.records.push(record);
        if self.records.len() > MAX_RECORDS {
            self.records.remove(0);
        }
    }

    fn parse_message(item: &Value) -> Result<Self> {
        if crate::serialized_len(item)? > MAX_BYTES
            || item
                .get("type")
                .is_some_and(|kind| kind.as_str() != Some("message"))
            || item.get("role").and_then(Value::as_str) != Some("user")
        {
            return Err(invalid_history());
        }
        let [summary, data] = content_parts(item).ok_or_else(invalid_history)?.as_slice() else {
            return Err(invalid_history());
        };
        input_text(summary).ok_or_else(invalid_history)?;
        let data = input_text(data).ok_or_else(invalid_history)?;
        let ledger: Self = serde_json::from_str(data).map_err(|_error| invalid_history())?;
        if ledger.records.len() > MAX_RECORDS {
            return Err(invalid_history());
        }
        for record in &ledger.records {
            record.validate()?;
        }
        Ok(ledger)
    }

    fn record(&mut self, execution: HostedToolExecution) -> Result<()> {
        self.insert(Record::from_execution(execution)?);
        self.trim()
    }

    fn trim(&mut self) -> Result<()> {
        while !self.records.is_empty()
            && crate::serialized_len(&progress_message(&self.records)?)? > MAX_BYTES
        {
            match self.records.as_mut_slice() {
                [last] => last.omit_detail()?,
                _ => {
                    self.records.remove(0);
                }
            }
        }
        Ok(())
    }

    /// Returns an append-only delta of records changed since the last message. Earlier
    /// messages stay in place so the provider's cached request prefix is never rewritten.
    fn take_message(&mut self) -> Result<Option<Value>> {
        // Insertion puts new or updated records after all the restored records.
        let Some(first) = self.records.iter().position(|record| record.touched) else {
            return Ok(None);
        };
        let (_, delta) = self.records.split_at_mut(first);
        let message = progress_message(delta)?;
        for record in delta {
            record.touched = false;
        }
        Ok(Some(message))
    }
}

fn progress_message(records: &[Record]) -> Result<Value> {
    let mut item = internal_user_message(KIND, &super::text::DEFINITION.prompt_tool_progress);
    content_parts_mut(&mut item)
        .ok_or_else(invalid_history)?
        .push(serde_json::json!({
            "type": "input_text",
            "text": serde_json::to_string(&LedgerView { records })?
        }));
    Ok(item)
}

impl Record {
    /// The execution was validated by its provider; only the ledger's budget applies here.
    fn from_execution(execution: HostedToolExecution) -> Result<Self> {
        let mut arguments = execution.arguments;
        let arguments_omitted = arguments_over_budget(arguments.as_ref())?;
        if arguments_omitted {
            arguments = None;
        }
        let mut record = Self {
            name: execution.name,
            arguments,
            arguments_omitted,
            result_fingerprint: execution.result_fingerprint,
            sources: Vec::with_capacity(execution.sources.len().min(MAX_SOURCES)),
            sources_omitted: false,
            calls: 1,
            touched: true,
        };
        record.add_sources(execution.sources);
        Ok(record)
    }

    fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty()
            || self.name.len() > crate::protocol::MAX_HOSTED_TOOL_NAME_BYTES
            || self.calls == 0
            || (self.arguments_omitted && self.arguments.is_some())
            || self.sources.len() > MAX_SOURCES
            || self
                .sources
                .iter()
                .any(|source| source.is_empty() || source.len() > MAX_SOURCE_BYTES)
            || arguments_over_budget(self.arguments.as_ref())?
        {
            return Err(invalid_history());
        }
        Ok(())
    }

    fn include_previous(&mut self, previous: Self) {
        self.calls = previous.calls.saturating_add(1);
        self.sources_omitted |= previous.sources_omitted;
        self.add_sources(previous.sources);
    }

    fn omit_detail(&mut self) -> Result<()> {
        if self.sources.pop().is_some() {
            self.sources_omitted = true;
        } else if self.arguments.take().is_some() {
            self.arguments_omitted = true;
        } else {
            return Err(Error::Internal(
                "tool progress guidance exceeds its byte limit".into(),
            ));
        }
        Ok(())
    }

    fn add_sources(&mut self, sources: Vec<String>) {
        for source in sources {
            if source.is_empty() || source.len() > MAX_SOURCE_BYTES {
                self.sources_omitted = true;
            } else if !self.sources.contains(&source) {
                if self.sources.len() < MAX_SOURCES {
                    self.sources.push(source);
                } else {
                    self.sources_omitted = true;
                }
            }
        }
    }

    fn identity(&self) -> Option<u64> {
        super::repeats::RepeatGuard::hosted_identity(&self.name, self.arguments.as_ref(), None)
            .map(|identity| identity.finish())
    }
}

fn arguments_over_budget(arguments: Option<&Value>) -> Result<bool> {
    let Some(arguments) = arguments else {
        return Ok(false);
    };
    Ok(crate::bounded_json::len_within(arguments, MAX_ARGUMENT_BYTES)?.is_none())
}

fn input_text(part: &Value) -> Option<&str> {
    if part.get("type")?.as_str()? != "input_text" {
        return None;
    }
    part.get("text")?.as_str()
}

fn invalid_history() -> Error {
    Error::Checkpoint("invalid tool progress history".into())
}

fn is_progress(item: &Value) -> bool {
    internal_message_kind(item) == Some(KIND)
}

/// Compaction may rewrite history, so it folds every delta into one consolidated message.
pub(crate) fn preserve(original: ModelInput<'_>, compacted: &mut Vec<Arc<Value>>) -> Result<()> {
    let progress = ToolProgress::from_input(original)?;
    let message = (!progress.records.is_empty())
        .then(|| progress_message(&progress.records))
        .transpose()?;
    compacted.retain(|item| !is_progress(item));
    compacted.extend(message.map(Arc::new));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::MiddlewareStack;

    fn execution(query: &str) -> HostedToolExecution {
        HostedToolExecution::new(
            crate::protocol::HostedCallId::new("search").unwrap(),
            "web_search",
            Some(serde_json::json!({"query": query})),
            None,
            ["https://example.com/source"],
        )
        .unwrap()
    }

    #[test]
    fn progress_bounds_records_and_payload_without_claiming_opaque_results() {
        let mut progress = ToolProgress::default();
        for index in 0..64 {
            progress
                .record(execution(&format!("question {index}")))
                .unwrap();
        }
        let mut oversized = execution("latest question");
        oversized.arguments = Some(serde_json::json!({"query": "é".repeat(MAX_ARGUMENT_BYTES)}));
        oversized.sources = vec!["x".repeat(MAX_SOURCE_BYTES + 1)];
        progress.record(oversized).unwrap();
        let message = progress.take_message().unwrap().unwrap();
        assert!(crate::serialized_len(&message).unwrap() <= MAX_BYTES);
        assert!(progress.take_message().unwrap().is_none());
        let restored = ToolProgress::from_input((&[message]).into()).unwrap();
        assert!(restored.records.len() <= MAX_RECORDS);
        let latest = restored.records.last().unwrap();
        assert!(latest.arguments.is_none());
        assert!(latest.arguments_omitted);
        assert!(latest.result_fingerprint.is_none());
        assert!(latest.sources.is_empty());
        assert!(latest.sources_omitted);
        assert!(restored.records.iter().all(|record| {
            record.arguments != Some(serde_json::json!({"query": "question 0"}))
        }));
    }

    #[test]
    fn progress_appends_deltas_that_fold_back_and_consolidate_on_compaction() {
        let mut progress = ToolProgress::default();
        progress.record(execution("same query")).unwrap();
        progress.record(execution("other query")).unwrap();
        let first = Arc::new(progress.take_message().unwrap().unwrap());
        let mut repeated = execution("same   query");
        repeated.sources = vec!["https://example.com/new-source".into()];
        progress.record(repeated).unwrap();
        let delta = Arc::new(progress.take_message().unwrap().unwrap());
        // The second message carries only the record that changed.
        assert!(!delta.to_string().contains("other query"));

        let ordinary = Arc::new(crate::backend::model::user_message("question"));
        let original = [
            Arc::clone(&first),
            Arc::clone(&ordinary),
            Arc::clone(&delta),
        ];
        let folded = ToolProgress::from_input((&original[..]).into()).unwrap();
        assert_eq!(folded.records.len(), 2);
        let same = folded.records.last().unwrap();
        assert_eq!((same.calls, same.sources.len()), (2, 2));

        let mut compacted = vec![Arc::clone(&ordinary), first];
        MiddlewareStack::new(Vec::new())
            .unwrap()
            .prepare_compacted_input((&original[..]).into(), &mut compacted)
            .unwrap();
        assert_eq!(compacted.len(), 2);
        assert!(Arc::ptr_eq(&compacted[0], &ordinary));
        let restored = ToolProgress::from_input(compacted.as_slice().into()).unwrap();
        assert_eq!(restored.records.len(), 2);
        assert_eq!(restored.records[1].calls, 2);
    }

    #[test]
    fn malformed_or_oversized_progress_history_is_rejected() {
        let missing_data = internal_user_message(KIND, "not structured history");
        let malformed = [missing_data];
        assert!(ToolProgress::from_input((&malformed).into()).is_err());
        let ordinary = Arc::new(crate::backend::model::user_message("keep"));
        let mut compacted = vec![Arc::clone(&ordinary)];
        assert!(
            MiddlewareStack::new(Vec::new())
                .unwrap()
                .prepare_compacted_input((&malformed).into(), &mut compacted)
                .is_err()
        );
        assert_eq!(compacted.len(), 1);
        assert!(Arc::ptr_eq(&compacted[0], &ordinary));
        let mut progress = ToolProgress::default();
        progress.record(execution("query")).unwrap();
        let mut invalid_kind = progress_message(&progress.records).unwrap();
        invalid_kind["type"] = "function_call".into();
        assert!(ToolProgress::from_input((&[invalid_kind]).into()).is_err());
        progress.records[0].calls = 0;
        assert!(
            ToolProgress::from_input((&[progress_message(&progress.records).unwrap()]).into())
                .is_err()
        );
        progress.records[0].calls = 1;
        progress.records[0].arguments = Some(Value::String("x".repeat(MAX_BYTES)));
        assert!(
            ToolProgress::from_input((&[progress_message(&progress.records).unwrap()]).into())
                .is_err()
        );
    }
}
