use super::{
    Arc, AtomicU64, CheckpointStore, Error, EventMsg, EventPageRequest, ExecutionOutcome,
    GatewayHost, Mutex, Ordering, Rejection, Result, gateway_run_stats, gateway_session_summaries,
    internal, invalid_config, local_machine_name, session_catalog,
};
use crate::bots::TelemetryEvent;
use crate::telemetry::{TelemetrySection, TelemetrySink, TelemetrySinkReport};
use crate::wire::{HookData, HookEvent, HookSource};
use mobius::agent::{FRONTEND_DISCONNECTED_REASON, TURN_INTERRUPTED_REASON, TURN_RESTARTED_REASON};
use serde_json::{Value, json};

#[derive(Default)]
pub(super) struct StorageMeasurements {
    completed: Mutex<Option<CompletedStorageMeasurement>>,
    revision: AtomicU64,
}

struct CompletedStorageMeasurement {
    completed_at: std::time::Instant,
    revision: u64,
    usage: crate::storage_usage::StorageUsage,
}

pub(super) fn cancellation_reason(reason: &str, failed: bool) -> &str {
    // Provider and middleware reasons may contain request content or credentials.
    match reason {
        TURN_INTERRUPTED_REASON | TURN_RESTARTED_REASON | FRONTEND_DISCONNECTED_REASON => reason,
        _ if failed => "failed",
        _ => "aborted",
    }
}

async fn session_cancellation(
    checkpoints: &dyn CheckpointStore,
    event: &HookEvent,
    sequence: Option<u64>,
) -> Result<Option<Value>> {
    let HookData::SessionTurnFinished {
        session_id,
        turn_id,
        outcome: outcome @ (ExecutionOutcome::Aborted | ExecutionOutcome::Failed),
    } = &event.data
    else {
        return Ok(None);
    };
    if !matches!(&event.source, HookSource::Session { session_id: source_id } if source_id == session_id)
    {
        return Ok(None);
    }
    let Some((sequence, before_sequence)) =
        sequence.and_then(|sequence| sequence.checked_add(1).map(|before| (sequence, before)))
    else {
        return Ok(None);
    };
    let page = checkpoints
        .event_page(
            session_id,
            EventPageRequest {
                before_sequence: Some(before_sequence),
                limit: 1,
            },
        )
        .await?;
    Ok(page.events.into_iter().next().and_then(|record| {
        let EventMsg::TurnAborted(turn) = record.event.msg else {
            return None;
        };
        (record.sequence == sequence && *turn.turn_id == **turn_id).then(|| {
            json!({
                "reason": cancellation_reason(&turn.reason, *outcome == ExecutionOutcome::Failed),
                "timestamp_ms": record.recorded_at_ms,
                "submission_id": record.event.submission_id,
            })
        })
    }))
}

impl StorageMeasurements {
    pub(super) fn invalidate(&self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
    }
}

impl GatewayHost {
    pub(crate) fn invalidate_storage_usage(&self) {
        self.storage_reads.invalidate();
    }

    pub(crate) async fn configure_telemetry(
        &self,
        expected: u64,
        mut sinks: Vec<TelemetrySink>,
        preserve_auth: &[String],
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        let mut live = state.config()?;
        if live.telemetry.revision != expected {
            return Err(invalid_config(
                "telemetry revision changed; refresh and retry",
            ));
        }
        if preserve_auth.len() > 16 {
            return Err(invalid_config("too many preserved credentials"));
        }
        for id in preserve_auth {
            let previous = live
                .telemetry
                .sinks
                .iter()
                .find(|sink| &sink.id == id)
                .ok_or_else(|| invalid_config("cannot preserve credentials for an unknown sink"))?;
            let replacement = sinks
                .iter_mut()
                .find(|sink| &sink.id == id)
                .ok_or_else(|| invalid_config("preserved sink is missing"))?;
            if replacement.url != previous.url {
                return Err(invalid_config(
                    "cannot preserve credentials for a changed destination",
                ));
            }
            if replacement.bearer_env.is_some() || replacement.bearer_file.is_some() {
                return Err(invalid_config(
                    "cannot both preserve and replace credentials",
                ));
            }
            replacement.bearer_env.clone_from(&previous.bearer_env);
            replacement.bearer_file.clone_from(&previous.bearer_file);
        }
        let revision = expected
            .checked_add(1)
            .ok_or_else(|| invalid_config("telemetry revision overflow"))?;
        let configured = self.telemetry.configure_after(|| {
            // Keep only replaced fields; restore memory only after establishing which file is visible.
            let mut previous = std::mem::replace(&mut live.telemetry.sinks, sinks);
            live.telemetry.revision = revision;
            let publication = match crate::publication::Outcome::applied(
                live.validate().and_then(|()| state.store.publish_live(&live))
            ) {
                    Ok(publication) => publication,
                    Err(error) => {
                        live.telemetry.sinks = previous;
                        live.telemetry.revision = expected;
                        return Err(error);
                    }
            };
            if let Err(error) = state.bots.sync_telemetry_cursors(&live.telemetry.sinks) {
                std::mem::swap(&mut live.telemetry.sinks, &mut previous);
                live.telemetry.revision = expected;
                match state.store.publish_live(&live) {
                    Ok(()) => return Err(error),
                    Err(rollback @ Error::PublicationApplied { .. }) => {
                        return Err(crate::publication::applied_error(Error::Config(format!(
                            "{error}; telemetry rollback applied but completion failed: {rollback}"
                        ))));
                    }
                    Err(rollback) => {
                        // Rollback never replaced the file: keep the published configuration live.
                        live.telemetry.sinks = previous;
                        live.telemetry.revision = revision;
                        let failure = crate::publication::applied_error(Error::Config(format!(
                            "telemetry configuration remains changed; cursor synchronization failed: {error}; rollback failed: {rollback}"
                        )));
                        // Runtime follows the visible config; missing cursor state continues to fail closed.
                        return Ok((live.telemetry.clone(), crate::publication::Outcome::applied(Err(failure))?)); // rules-allow: the runtime receives a snapshot while the configuration stays live
                    }
                }
            }
            Ok((live.telemetry.clone(), publication)) // rules-allow: the runtime receives a snapshot while the configuration stays live
        });
        drop(live);
        configured.map_err(internal)
    }

    #[expect(
        clippy::unused_async,
        reason = "server dispatch awaits this like the other telemetry requests"
    )]
    pub(crate) async fn schedule_telemetry(&self, id: String) -> Result<()> {
        self.telemetry.request_manual(id)?;
        self.telemetry.notify.notify_one();
        Ok(())
    }

    pub(crate) async fn telemetry_pending(&self) -> Result<bool> {
        let config = self.telemetry.config()?;
        let state = self.state.lock().await;
        for sink in config
            .sinks
            .iter()
            .filter(|sink| sink.enabled && !sink.events.is_empty())
        {
            if self.telemetry.can_drain(&sink.id)? && state.bots.telemetry_count(sink)? > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) async fn storage_usage_request(
        &self,
    ) -> std::result::Result<crate::storage_usage::StorageUsage, Rejection> {
        // The async guard serializes only the bounded walk, so simultaneous clients
        // share its completed result instead of starting competing disk scans.
        let mut completed = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.storage_reads.completed.lock(),
        )
        .await
        .map_err(|error| {
            internal(format!(
                "storage measurement timed out waiting for an active measurement: {error}"
            ))
        })?;
        let revision = self.storage_reads.revision.load(Ordering::Acquire);
        if let Some(previous) = completed.as_ref()
            && previous.revision == revision
            && previous.completed_at.elapsed() < std::time::Duration::from_secs(5)
        {
            // Both the retained snapshot and each wire response require owned data.
            return Ok(previous.usage.clone()); // rules-allow: the retained snapshot and the response both own the usage
        }
        let usage = self.storage_usage().await.map_err(internal)?;
        *completed = Some(CompletedStorageMeasurement {
            completed_at: std::time::Instant::now(),
            revision,
            usage: usage.clone(), // rules-allow: the retained snapshot and the response both own the usage
        });
        drop(completed);
        Ok(usage)
    }

    pub(crate) async fn telemetry_report(&self) -> Result<(u64, Vec<TelemetrySinkReport>)> {
        let config = self.telemetry.config()?;
        let state = self.state.lock().await;
        let mut reports = Vec::new();
        for configured in &config.sinks {
            // Reporting redacts credentials without changing the live sink configuration.
            let mut sink = configured.clone(); // rules-allow: redaction must not change the live sink
            let mut status = self.telemetry.status(&sink.id)?;
            if !sink.events.is_empty() {
                status.events_pending = state.bots.telemetry_count(&sink)?;
            }
            let auth = if sink.bearer_env.is_some() {
                crate::telemetry::SinkAuth::BearerEnv
            } else if sink.bearer_file.is_some() {
                crate::telemetry::SinkAuth::BearerFile
            } else {
                crate::telemetry::SinkAuth::None
            };
            sink.redact_report()?;
            reports.push(TelemetrySinkReport { sink, auth, status });
        }
        drop(state);
        Ok((config.revision, reports))
    }

    pub(crate) async fn telemetry_snapshot(
        &self,
        sections: &[TelemetrySection],
        connected_clients: usize,
    ) -> Result<Value> {
        let mut snapshot = serde_json::Map::new();
        snapshot.insert("machine_name".into(), json!(local_machine_name()?));
        if sections.is_empty() {
            return Ok(Value::Object(snapshot));
        }
        if sections.contains(&TelemetrySection::Activity) {
            let activity = self
                .runtime_activity()
                .await
                .map_err(|e| Error::Config(e.message.to_string()))?;
            snapshot.insert("activity".into(), json!({"idle": activity.idle && connected_clients == 0, "connected_clients": connected_clients, "activity_revision": activity.activity_revision, "next_routine_at": activity.next_routine_at, "active_sessions": activity.active_sessions, "running_routines": activity.running_routines}));
        }
        if sections.contains(&TelemetrySection::Resources) {
            snapshot.insert("resources".into(), self.telemetry.resources().await);
        }
        if sections.contains(&TelemetrySection::Context) {
            snapshot.insert("context".into(), self.telemetry.contexts());
        }
        if sections.contains(&TelemetrySection::Storage) {
            let mut usage = self.storage_usage().await?;
            usage.bound_telemetry_details();
            snapshot.insert("storage".into(), serde_json::to_value(usage)?);
        }
        if sections.contains(&TelemetrySection::Usage) {
            let state = self.state.lock().await;
            let config = crate::config::lock(&state.config)?;
            let cutoff = crate::config::unix_day(std::time::SystemTime::now()).unwrap_or_default();
            let usage = config
                .profile()
                .daily_usage
                .into_iter()
                .filter(|day| day.unix_day >= cutoff.saturating_sub(30))
                .collect::<Vec<_>>();
            drop(config);
            drop(state);
            snapshot.insert("usage".into(), serde_json::to_value(usage)?);
        }
        if sections.contains(&TelemetrySection::Runs) {
            let checkpoints = Arc::clone(&self.state.lock().await.checkpoints);
            snapshot.insert(
                "runs".into(),
                serde_json::to_value(
                    gateway_run_stats(&gateway_session_summaries(&checkpoints).await?)?.completed,
                )?,
            );
        }
        Ok(Value::Object(snapshot))
    }

    pub(crate) async fn telemetry_events(
        &self,
        sink: &TelemetrySink,
    ) -> Result<(Vec<Value>, Option<i64>, u64)> {
        let (batch, pending, checkpoints, activities, bots) = {
            let state = self.state.lock().await;
            let (batch, pending) = state.bots.telemetry_batch(sink)?;
            (
                batch,
                pending,
                Arc::clone(&state.checkpoints),
                Arc::clone(&state.activities),
                Arc::clone(&state.bots),
            )
        };
        let mut cursor = None;
        let mut bytes = 0_usize;
        let sessions = if batch.is_empty() {
            Vec::new()
        } else {
            session_catalog(&checkpoints, &activities).await?
        };
        let mut events = Vec::with_capacity(batch.len());
        for TelemetryEvent {
            rowid,
            event,
            journal_sequence,
        } in batch
        {
            let mut value = serde_json::to_value(&event)?;
            if let crate::wire::HookSource::Session { session_id } = &event.source
                && let Some(session) = sessions.iter().find(|s| *s.session_id == **session_id)
            {
                let bot = bots.find_bot(&session.session_context.owner_id)?;
                let mut enrichment = serde_json::Map::new();
                enrichment.insert("title".into(), json!(session.title));
                enrichment.insert("run_count".into(), json!(session.execution_stats.run_count));
                if let Some(bot) = bot {
                    enrichment.insert("bot_name".into(), json!(bot.name));
                }
                if let Some(cancellation) =
                    session_cancellation(checkpoints.as_ref(), &event, journal_sequence).await?
                {
                    enrichment.insert("cancellation".into(), cancellation);
                }
                if matches!(
                    event.data,
                    crate::wire::HookData::SessionTurnFinished {
                        outcome: ExecutionOutcome::Completed,
                        ..
                    }
                ) {
                    enrichment.insert(
                        "final_message".into(),
                        json!(
                            session
                                .activity
                                .message
                                .as_deref()
                                .unwrap_or_default()
                                .chars()
                                .take(1000)
                                .collect::<String>()
                        ),
                    );
                }
                value
                    .as_object_mut()
                    .ok_or_else(|| Error::Config("telemetry event is not a JSON object".into()))?
                    .insert("session".into(), Value::Object(enrichment));
            }
            let size = serde_json::to_vec(&value)?.len();
            if !events.is_empty() && bytes.saturating_add(size) > 48 * 1024 {
                break;
            }
            bytes = bytes.saturating_add(size);
            cursor = Some(rowid);
            events.push(value);
        }
        Ok((events, cursor, pending))
    }

    pub(crate) async fn advance_telemetry(
        &self,
        id: &str,
        cursor: i64,
        revision: u64,
    ) -> Result<()> {
        let state = self.state.lock().await;
        if crate::config::lock(&state.config)?.telemetry.revision != revision {
            return Ok(());
        }
        state.bots.advance_telemetry(id, cursor)
    }
}

impl GatewayHost {
    pub(crate) async fn storage_usage(&self) -> Result<crate::storage_usage::StorageUsage> {
        let budget = crate::storage_usage::MeasurementBudget::new();
        let (root, checkpoints, files, bots, limit_bytes, tls) = {
            let state = tokio::time::timeout_at(budget.deadline(), self.state.lock())
                .await
                .map_err(|error| {
                    Error::Config(format!(
                        "storage measurement timed out waiting for gateway state: {error}"
                    ))
                })?;
            let config = crate::config::lock(&state.config)?;
            (
                state.store.state_dir().to_path_buf(),
                Arc::clone(&state.checkpoints),
                Arc::clone(&state.session_files),
                Arc::clone(&state.bots),
                config.runtime.storage_limit_bytes,
                config.tls.clone(), // rules-allow: measurement runs after the configuration lock is released
            )
        };
        crate::storage_usage::measure_usage(
            root,
            checkpoints,
            files,
            bots,
            limit_bytes,
            tls,
            budget,
        )
        .await
    }
}
