use super::*;
use crate::telemetry::{TelemetryConfig, TelemetrySection, TelemetrySink, TelemetrySinkReport};
use serde_json::{Value, json};

impl GatewayHost {
    pub(crate) async fn configure_telemetry(
        &self,
        expected: u64,
        mut sinks: Vec<TelemetrySink>,
        preserve_auth: &[String],
    ) -> std::result::Result<(), Rejection> {
        let _mutation = self.begin_mutation().await?;
        let state = self.state.lock().await;
        let mut live = state
            .config
            .lock()
            .map_err(|_| internal("configuration lock poisoned"))?;
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
        let mut config = live.clone();
        config.telemetry = TelemetryConfig {
            policy: live.telemetry.policy,
            revision: expected
                .checked_add(1)
                .ok_or_else(|| invalid_config("telemetry revision overflow"))?,
            sinks,
        };
        config.validate().map_err(invalid_config)?;
        state.store.save(&config).map_err(internal)?;
        if let Err(error) = state.bots.sync_telemetry_cursors(&config.telemetry.sinks) {
            state.store.save(&live).map_err(internal)?;
            return Err(internal(error));
        }
        self.telemetry
            .configure(config.telemetry.clone())
            .map_err(internal)?;
        *live = config;
        Ok(())
    }

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
        let busy = || Rejection {
            code: "rate_limited",
            message: "storage measurement is busy; retry after 5 seconds".into(),
            fatal: false,
        };
        // The async guard deliberately owns the single walk; callers never queue behind it.
        let mut previous = self.storage_reads.try_lock().map_err(|_| busy())?;
        if previous.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(5)) {
            return Err(busy());
        }
        *previous = Some(std::time::Instant::now());
        self.storage_usage().await.map_err(internal)
    }

    pub(crate) async fn telemetry_report(&self) -> Result<(u64, Vec<TelemetrySinkReport>)> {
        let config = self.telemetry.config()?;
        let state = self.state.lock().await;
        let mut reports = Vec::new();
        for sink in &config.sinks {
            let mut sink = sink.clone();
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
        Ok((config.revision, reports))
    }

    pub(crate) async fn telemetry_snapshot(
        &self,
        sections: &[TelemetrySection],
        connected_clients: usize,
    ) -> Result<Value> {
        let mut snapshot = json!({"machine_name": local_machine_name()?});
        if sections.is_empty() {
            return Ok(snapshot);
        }
        if sections.contains(&TelemetrySection::Activity) {
            let activity = self
                .runtime_activity()
                .await
                .map_err(|e| Error::Config(e.message))?;
            snapshot["activity"] = json!({"idle": activity.idle && connected_clients == 0, "connected_clients": connected_clients, "activity_revision": activity.activity_revision, "next_routine_at": activity.next_routine_at, "active_sessions": activity.active_sessions, "running_routines": activity.running_routines});
        }
        if sections.contains(&TelemetrySection::Storage) {
            let mut usage = self.storage_usage().await?;
            usage.bound_telemetry_details();
            snapshot["storage"] = serde_json::to_value(usage)?;
        }
        if sections.contains(&TelemetrySection::Usage) {
            let state = self.state.lock().await;
            let config = state
                .config
                .lock()
                .map_err(|_| Error::Config("configuration lock poisoned".into()))?;
            let cutoff = u64::try_from(Utc::now().timestamp().max(0)).unwrap_or_default() / 86_400;
            snapshot["usage"] = serde_json::to_value(
                config
                    .profile()
                    .daily_usage
                    .into_iter()
                    .filter(|day| day.unix_day >= cutoff.saturating_sub(30))
                    .collect::<Vec<_>>(),
            )?;
        }
        if sections.contains(&TelemetrySection::Runs) {
            let checkpoints = Arc::clone(&self.state.lock().await.checkpoints);
            snapshot["runs"] = serde_json::to_value(
                gateway_run_stats(&gateway_session_summaries(&checkpoints).await?)?.completed,
            )?;
        }
        Ok(snapshot)
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
        for (row, event) in batch {
            let mut value = serde_json::to_value(&event)?;
            if let crate::wire::HookSource::Session { session_id } = &event.source
                && let Some(session) = sessions.iter().find(|s| &s.session_id == session_id)
            {
                let bot = bots.find_bot(&session.session_context.owner_id)?;
                let mut enrichment =
                    json!({"title": session.title, "run_count": session.execution_stats.run_count});
                if let Some(bot) = bot {
                    enrichment["bot_name"] = json!(bot.name);
                }
                if matches!(
                    event.data,
                    crate::wire::HookData::SessionTurnFinished {
                        outcome: ExecutionOutcome::Completed,
                        ..
                    }
                ) {
                    enrichment["final_message"] = json!(
                        session
                            .activity
                            .message
                            .as_deref()
                            .unwrap_or_default()
                            .chars()
                            .take(1000)
                            .collect::<String>()
                    );
                }
                value["session"] = enrichment;
            }
            let size = serde_json::to_vec(&value)?.len();
            if !events.is_empty() && bytes.saturating_add(size) > 48 * 1024 {
                break;
            }
            bytes = bytes.saturating_add(size);
            cursor = Some(row);
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
        if state
            .config
            .lock()
            .map_err(|_| Error::Config("configuration lock poisoned".into()))?
            .telemetry
            .revision
            != revision
        {
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
                .map_err(|_| {
                    Error::Config("storage measurement timed out waiting for gateway state".into())
                })?;
            let config = state
                .config
                .lock()
                .map_err(|_| Error::Config("configuration lock poisoned".into()))?;
            (
                state.store.state_dir().to_path_buf(),
                Arc::clone(&state.checkpoints),
                state.session_files.clone(),
                Arc::clone(&state.bots),
                config.runtime.storage_limit_bytes,
                config.tls.clone(),
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
