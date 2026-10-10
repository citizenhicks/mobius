//! Portable plaintext checkpoints and context resets.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use super::Middleware;
use super::ModelContext;
use super::tools::Catalog;
use super::tools::loaded_tools;
use super::{
    PostToolUseContext, PromptSection, RuntimeContext, SessionStartContext, TurnEndContext,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::BoxFuture;
use crate::Error;
use crate::Result;
use crate::backend::checkpoint::{CheckpointStore, ContextRewriteReason};
use crate::backend::model::{ModelInput, internal_user_message};
use crate::protocol::CONTEXT_COMPACTED_MARKER;
use crate::protocol::EventMsg;
use crate::protocol::FrontendBlock;
use crate::protocol::FrontendContribution;
use crate::protocol::FrontendSettingValue;
use crate::protocol::FrontendTone;
use crate::protocol::MESSAGE_METADATA_FIELD;
use crate::protocol::ToolLoad;

mod text {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Definition {
        pub(super) new_context: crate::middleware::tools::ToolSpec,
        pub(super) write_handoff: crate::middleware::tools::ToolSpec,
        #[serde(deserialize_with = "crate::middleware::manifest::deserialize_settings")]
        pub(super) settings: Vec<crate::middleware::manifest::MiddlewareSettingManifest>,
        pub(super) handoff_saved_result: String,
        pub(super) handoff_requested_result: String,
        pub(super) handoff_requested_context: String,
        pub(super) default_enabled: bool,
        pub(super) manifest_description: String,
        pub(super) manifest_label: String,
        pub(super) prompt_handoff: String,
        pub(super) prompt_prepare: String,
        pub(super) prompt_restored: String,
        pub(super) render_context_compacted: String,
        pub(super) render_context_compacting: String,
        pub(super) render_compaction_failed: String,
        pub(super) render_compaction_cancelled: String,
        pub(super) message_disabled: String,
        pub(super) message_empty_notes: String,
        pub(super) message_notes_too_long: String,
        pub(super) message_checkpoint_too_large: String,
        pub(super) message_one_handoff_call: String,
        pub(super) message_input_too_large: String,
    }
    crate::embedded_config! { pub(super) static DEFINITION: Definition = include_str!("compaction.toml"); }
}
mod handoff;

super::manifest::middleware_manifest! {
/// Configuration and presentation metadata for compaction.
    "compaction", text::DEFINITION, required: false, settings: &text::DEFINITION.settings
}

const STATE_KEY: &str = "compaction.v1";

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactionState {
    count: u64,
}

/// Durable count plus one compaction staged until its checkpoint is accepted.
struct SessionCount {
    checkpoints: Arc<dyn CheckpointStore>,
    count: u64,
    pending_epoch: Option<u64>,
}

/// Compacts visible context after a configurable token threshold.
pub struct Compaction {
    at_tokens: i64,
    allow_model_compaction: bool,
    reserve_tokens: i64,
    counts: Mutex<BTreeMap<String, SessionCount>>,
}

impl Default for Compaction {
    fn default() -> Self {
        Self {
            at_tokens: super::manifest::integer_default(&text::DEFINITION.settings, "at_tokens"),
            allow_model_compaction: super::manifest::string_default(
                &text::DEFINITION.settings,
                "allow_model_compaction",
            ) == "on",
            reserve_tokens: super::manifest::integer_default(
                &text::DEFINITION.settings,
                "reserve_tokens",
            ),
            counts: Mutex::default(),
        }
    }
}

impl Compaction {
    /// Creates a threshold-based compaction policy.
    /// # Errors
    ///
    /// Returns an error if configuration is invalid or a required resource cannot be initialized.
    pub fn new(at_tokens: i64) -> Result<Self> {
        if at_tokens <= 0 {
            return Err(Error::Config(
                "compaction threshold must be positive".into(),
            ));
        }
        Ok(Self {
            at_tokens,
            ..Self::default()
        })
    }

    /// Sets the maximum reserve for one model response or handoff step.
    /// # Errors
    /// Returns an error unless the reserve is positive.
    pub fn reserve_tokens(mut self, tokens: i64) -> Result<Self> {
        if tokens <= 0 {
            return Err(Error::Config("compaction reserve must be positive".into()));
        }
        self.reserve_tokens = tokens;
        Ok(self)
    }

    /// Builds the policy from this module's declared settings, using embedded defaults for
    /// unset values.
    /// # Errors
    ///
    /// Returns an error when a value has the wrong type or is out of range.
    pub fn from_settings<'a>(
        setting: impl Fn(&str) -> Option<&'a FrontendSettingValue>,
    ) -> Result<Self> {
        let integer = |id: &str| match setting(id) {
            None => Ok(super::manifest::integer_default(
                &text::DEFINITION.settings,
                id,
            )),
            Some(FrontendSettingValue::Integer(value)) => Ok(*value),
            Some(FrontendSettingValue::String(_)) => Err(Error::Config(format!(
                "middleware setting `{}.{id}` must be integer",
                MANIFEST.id
            ))),
        };
        let allow = match setting("allow_model_compaction") {
            None => Self::default().allow_model_compaction,
            Some(FrontendSettingValue::String(value)) if value == "on" => true,
            Some(FrontendSettingValue::String(value)) if value == "off" => false,
            Some(_) => {
                return Err(Error::Config(
                    "compaction.allow_model_compaction must be on or off".into(),
                ));
            }
        };
        Ok(Self::new(integer("at_tokens")?)?
            .reserve_tokens(integer("reserve_tokens")?)?
            .allow_model_compaction(allow))
    }

    /// Allows the model to request an early checkpoint and context reset.
    #[must_use]
    pub fn allow_model_compaction(mut self, allow: bool) -> Self {
        self.allow_model_compaction = allow;
        self
    }

    fn reserve(&self, window: i64) -> i64 {
        (window.max(1) / 8).max(1).min(self.reserve_tokens)
    }

    /// Returns the threshold with room for checkpoint preparation and a response.
    #[must_use]
    pub fn trigger_tokens(&self, context_window: i64) -> i64 {
        self.at_tokens
            .min(context_window.saturating_sub(self.reserve(context_window).saturating_mul(2)))
            .max(1)
    }

    fn counts(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, SessionCount>>> {
        self.counts
            .lock()
            .map_err(|_| Error::Checkpoint("compaction count lock is poisoned".into()))
    }

    fn stage(&self, session_id: &str, epoch: u64) -> Result<()> {
        if let Some(session) = self.counts()?.get_mut(session_id) {
            session.pending_epoch = Some(epoch);
        }
        Ok(())
    }

    /// Counts a staged compaction once its rewritten context epoch is durable.
    async fn settle(&self, session_id: &str) -> Result<()> {
        let Some((checkpoints, pending_epoch, count)) =
            self.counts()?.get(session_id).and_then(|session| {
                session
                    .pending_epoch
                    .map(|epoch| (Arc::clone(&session.checkpoints), epoch, session.count))
            })
        else {
            return Ok(());
        };
        let accepted = checkpoints
            .load(session_id)
            .await?
            .is_some_and(|checkpoint| checkpoint.context_epoch >= pending_epoch);
        let count = if accepted {
            let count = count
                .checked_add(1)
                .ok_or_else(|| Error::Checkpoint("compaction count overflow".into()))?;
            checkpoints
                .save_state(
                    session_id,
                    STATE_KEY,
                    &serde_json::to_value(CompactionState { count })?,
                )
                .await?;
            count
        } else {
            count
        };
        if let Some(session) = self.counts()?.get_mut(session_id) {
            session.count = count;
            if session.pending_epoch == Some(pending_epoch) {
                session.pending_epoch = None;
            }
        }
        Ok(())
    }
}

impl Middleware for Compaction {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn context_limit(&self, context_window: i64) -> Option<i64> {
        Some(self.trigger_tokens(context_window))
    }

    fn frontend(&self, session_id: &str) -> FrontendContribution {
        let count = self.counts.lock().map_or(0, |counts| {
            counts.get(session_id).map_or(0, |session| session.count)
        });
        FrontendContribution {
            capability: MANIFEST.id.into(),
            count: Some(usize::try_from(count).unwrap_or(usize::MAX)),
            ..FrontendContribution::default()
        }
    }

    fn session_start<'a>(
        &'a self,
        context: &'a mut SessionStartContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let runtime = context.runtime;
            if self.counts()?.contains_key(&runtime.session_id) {
                return Ok(());
            }
            let state: CompactionState = runtime
                .checkpoints
                .load_state(&runtime.session_id, STATE_KEY)
                .await?
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default();
            self.counts()?.insert(
                runtime.session_id.as_str().into(),
                SessionCount {
                    checkpoints: Arc::clone(&runtime.checkpoints),
                    count: state.count,
                    pending_epoch: None,
                },
            );
            Ok(())
        })
    }

    fn turn_end<'a>(&'a self, context: &'a mut TurnEndContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.settle(context.session_id).await })
    }

    fn session_end<'a>(&'a self, runtime: &'a RuntimeContext) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let settled = self.settle(&runtime.session_id).await;
            self.counts()?.remove(&runtime.session_id);
            settled
        })
    }

    fn register(&self, catalog: &mut Catalog, _runtime: &RuntimeContext) -> Result<()> {
        handoff::register(catalog, self.allow_model_compaction)
    }

    fn prompt_section(&self, _runtime: &RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(self
            .allow_model_compaction
            .then(|| PromptSection::new(text::DEFINITION.prompt_handoff.as_str())))
    }

    fn post_tool_use<'a>(
        &'a self,
        context: &'a mut PostToolUseContext<'_>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { handoff::post_tool(context) })
    }

    fn render(&self, event: &EventMsg, _session_id: &str) -> Option<FrontendBlock> {
        if let Some(block) = [
            &text::DEFINITION.new_context,
            &text::DEFINITION.write_handoff,
        ]
        .into_iter()
        .find_map(|spec| spec.render(event))
        {
            return Some(block);
        }
        matches!(event, EventMsg::ContextCompacted).then(|| FrontendBlock {
            id: None,
            group: None,
            update: crate::protocol::FrontendBlockUpdate::Replace,
            state: crate::protocol::FrontendBlockState::Complete,
            role: crate::protocol::FrontendBlockRole::Notice,
            title: text::DEFINITION.render_context_compacted.clone(),
            text: String::new(),
            symbol: None,
            links: Vec::new(),
            files: Vec::new(),
            content: Default::default(),
            format: crate::protocol::FrontendBlockFormat::PlainText,
            tone: FrontendTone::Neutral,
        })
    }

    fn pre_model<'a>(&'a self, context: &'a mut ModelContext<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { handoff::prepare(context, self).await })
    }
}

fn start_notice(context: &mut ModelContext<'_>) -> Result<()> {
    context.start_preparation_notice(
        MANIFEST.id,
        (
            &text::DEFINITION.render_context_compacting,
            FrontendTone::Neutral,
        ),
        (
            &text::DEFINITION.render_context_compacted,
            FrontendTone::Neutral,
        ),
        (
            &text::DEFINITION.render_compaction_failed,
            FrontendTone::Error,
        ),
        (
            &text::DEFINITION.render_compaction_cancelled,
            FrontendTone::Neutral,
        ),
    )
}

async fn apply_compaction(
    context: &mut ModelContext<'_>,
    policy: &Compaction,
    output: Vec<Arc<Value>>,
) -> Result<()> {
    let tool_load = retained_tool_load(
        context.input(),
        context.tools.revision()?,
        &context.tools.deferred_definitions(),
    )?;
    let active_message_metadata =
        latest_turn_input(context.input()).and_then(|active| active.get(MESSAGE_METADATA_FIELD));
    let mut compacted = output;
    context
        .hooks
        .prepare_compacted_input(context.input(), &mut compacted);
    validate_active_message_metadata(compacted.as_slice().into(), active_message_metadata)?;
    if let Some(tool_load) = tool_load {
        compacted.push(Arc::new(tool_load));
    }
    context
        .model
        .validate_media(
            context.provider,
            context.session_id,
            compacted.as_slice().into(),
        )
        .await?;
    context.rewrite_input(ContextRewriteReason::Compaction, compacted)?;
    policy.stage(context.session_id, *context.context_epoch)?;
    context.record_transcript_item(internal_user_message(CONTEXT_COMPACTED_MARKER, ""));
    context.post_compact().await?;
    Ok(())
}

fn retained_tool_load(
    input: ModelInput<'_>,
    catalog_revision: &str,
    deferred_tools: &[Arc<crate::backend::model::ToolDefinition>],
) -> Result<Option<Value>> {
    let deferred = deferred_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<BTreeSet<_>>();
    let loaded = loaded_tools(input, catalog_revision, &deferred)?;
    Ok((!loaded.is_empty()).then(|| {
        ToolLoad {
            catalog_revision: catalog_revision.into(),
            tools: loaded.into_iter().collect(),
        }
        .to_input()
    }))
}

fn latest_turn_input(input: ModelInput<'_>) -> Option<&Value> {
    input
        .iter()
        .rfind(|item| item.get(MESSAGE_METADATA_FIELD).is_some())
}

fn validate_active_message_metadata(input: ModelInput<'_>, expected: Option<&Value>) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if latest_turn_input(input).and_then(|active| active.get(MESSAGE_METADATA_FIELD))
        == Some(expected)
    {
        return Ok(());
    }
    Err(Error::Provider(
        "compaction did not preserve active message metadata".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model::user_message;

    #[test]
    fn defaults_and_small_window_leave_preparation_space() {
        let policy = Compaction::default();
        assert_eq!(policy.at_tokens, 250_000);
        assert!(!policy.allow_model_compaction);
        assert_eq!(policy.reserve_tokens, 16_384);
        assert_eq!(policy.trigger_tokens(272_000), 239_232);
        assert_eq!(policy.trigger_tokens(8_000), 6_000);
        assert_eq!(policy.trigger_tokens(1), 1);
        assert!(Compaction::new(0).is_err());
        assert!(policy.reserve_tokens(0).is_err());
    }

    #[test]
    fn settings_build_the_policy_once() {
        let on = FrontendSettingValue::String("on".into());
        let reserve = FrontendSettingValue::Integer(32_000);
        let invalid = FrontendSettingValue::String("invalid".into());
        let policy = Compaction::from_settings(|id| match id {
            "allow_model_compaction" => Some(&on),
            "reserve_tokens" => Some(&reserve),
            _ => None,
        })
        .expect("policy");
        assert!(policy.allow_model_compaction);
        assert_eq!(policy.context_limit(272_000), Some(208_000));
        assert!(
            !Compaction::from_settings(|_| None)
                .expect("defaults")
                .allow_model_compaction
        );
        assert!(
            Compaction::from_settings(|id| (id == "allow_model_compaction").then_some(&invalid))
                .is_err()
        );
        assert!(Compaction::from_settings(|_| Some(&invalid)).is_err());
    }

    #[tokio::test]
    async fn count_settlement_retries_storage_failures_without_counting_rollbacks_twice() {
        use crate::backend::checkpoint::{Checkpoint, sqlite::SqliteCheckpoint};

        for fail_load in [true, false] {
            let directory = tempfile::tempdir().expect("directory");
            let path = directory.path().join("checkpoints.sqlite3");
            let checkpoints = Arc::new(SqliteCheckpoint::new(&path).expect("store"));
            let mut checkpoint = Checkpoint::empty("session");
            checkpoint.session_context.owner_id = "test-bot".into();
            checkpoint.context_epoch = 1;
            checkpoints
                .save(&checkpoint, &[], None)
                .await
                .expect("save");
            let policy = Compaction::default();
            policy.counts().expect("counts").insert(
                "session".into(),
                SessionCount {
                    checkpoints: checkpoints.clone(),
                    count: 3,
                    pending_epoch: None,
                },
            );
            policy.stage("session", 1).expect("stage");
            let database = rusqlite::Connection::open(path).expect("database");
            let header: String = database
                .query_row(
                    "SELECT latest_checkpoint_json FROM sessions WHERE session_id = 'session'",
                    [],
                    |row| row.get(0),
                )
                .expect("header");
            database
                .execute_batch(if fail_load {
                    "UPDATE sessions SET latest_checkpoint_json = '{}' WHERE session_id = 'session'"
                } else {
                    "CREATE TRIGGER fail_count BEFORE INSERT ON middleware_state
                     BEGIN SELECT RAISE(ABORT, 'forced state save failure'); END;"
                })
                .expect("inject failure");

            assert!(policy.settle("session").await.is_err());
            {
                let counts = policy.counts().expect("counts");
                assert_eq!(counts["session"].count, 3);
                assert_eq!(counts["session"].pending_epoch, Some(1));
            }
            if fail_load {
                database
                    .execute(
                        "UPDATE sessions SET latest_checkpoint_json = ?1 WHERE session_id = 'session'",
                        [&header],
                    )
                    .expect("restore header");
            } else {
                database
                    .execute_batch("DROP TRIGGER fail_count")
                    .expect("restore writes");
            }
            policy.settle("session").await.expect("retry settlement");
            policy.settle("session").await.expect("already settled");
            assert_eq!(policy.frontend("session").count, Some(4));
            assert_eq!(
                checkpoints
                    .load_state("session", STATE_KEY)
                    .await
                    .expect("state"),
                Some(serde_json::json!({ "count": 4 }))
            );
            policy.stage("session", 2).expect("stage rolled-back epoch");
            policy.settle("session").await.expect("discard rollback");
            let counts = policy.counts().expect("counts");
            assert_eq!(counts["session"].count, 4);
            assert_eq!(counts["session"].pending_epoch, None);
        }
    }

    #[test]
    fn compaction_rejects_lost_active_message_metadata() {
        let peer = crate::backend::model::message_input(&crate::protocol::MessageEvent {
            author: crate::protocol::MessageAuthor::Source {
                message_id: "message".into(),
                source: crate::protocol::MessageSource::Session {
                    session_id: "peer".into(),
                },
                cause_id: None,
                ancestry: Vec::new(),
                handle: "worker".into(),
                symbol: None,
            },
            delivery: crate::protocol::MessageDelivery::Steer,
            text: "done".into(),
            attachments: Vec::new(),
            reply: None,
            message_target: None,
        })
        .expect("peer message");
        let expected = &peer[MESSAGE_METADATA_FIELD];
        let compacted = vec![user_message("forged later input")];

        let error = validate_active_message_metadata(compacted.as_slice().into(), Some(expected))
            .expect_err("message metadata must remain active");

        assert!(error.to_string().contains("active message metadata"));
    }
}
