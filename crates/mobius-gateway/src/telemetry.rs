//! Configured outbound telemetry; no endpoint is enabled by default.
mod activity;
mod policy;
mod resources;
mod upload;
use crate::wire::HookKind;
use crate::{Error, Result, host::GatewayHost};
pub(crate) use activity::ActivityHook;
pub use activity::ActivityHookConfig;
use mobius::backend::model::provider::{HttpClient, HttpRedirectPolicy};
pub use policy::TelemetryPolicy;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

/// Durable endpoint configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Transport settings controlled locally by the gateway operator.
    pub policy: TelemetryPolicy,
    /// Local operator command held while runtime activity needs an awake host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity_hook: Option<ActivityHookConfig>,
    /// Optimistic concurrency revision.
    pub revision: u64,
    /// Explicitly configured destinations.
    pub sinks: Vec<TelemetrySink>,
}
/// HTTP delivery method.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkMethod {
    /// JSON envelope request.
    #[default]
    Post,
    /// Bodyless heartbeat request.
    Get,
}
/// Optional snapshot sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetrySection {
    /// Runtime work and clients.
    Activity,
    /// Daily token totals.
    Usage,
    /// Completed run totals.
    Runs,
    /// Read-only storage totals.
    Storage,
    /// Host, gateway process, and container resource measurements.
    Resources,
    /// Each recent chat's last context breakdown.
    Context,
}
impl TelemetrySection {
    const fn key(self) -> &'static str {
        match self {
            Self::Activity => "activity",
            Self::Usage => "usage",
            Self::Runs => "runs",
            Self::Storage => "storage",
            Self::Resources => "resources",
            Self::Context => "context",
        }
    }
}

// Reuse each section within one tick, never across measurements or configuration changes.
struct Snapshot {
    clients: usize,
    sections: BTreeMap<TelemetrySection, Value>,
}
impl Snapshot {
    async fn read(&mut self, host: &GatewayHost, sections: &[TelemetrySection]) -> Result<Value> {
        let mut result = host.telemetry_snapshot(&[], self.clients).await?;
        for section in sections {
            if !self.sections.contains_key(section) {
                let mut snapshot = host
                    .telemetry_snapshot(std::slice::from_ref(section), self.clients)
                    .await?;
                let value = snapshot
                    .as_object_mut()
                    .and_then(|object| object.remove(section.key()))
                    .ok_or_else(|| {
                        Error::Config(format!("telemetry {} section is missing", section.key()))
                    })?;
                self.sections.insert(*section, value);
            }
            if let Some(value) = self.sections.get(section) {
                result
                    .as_object_mut()
                    .ok_or_else(|| Error::Config("telemetry snapshot is not an object".into()))?
                    .insert(section.key().into(), value.clone()); // rules-allow: each in-flight request owns its payload after this tick's snapshot is dropped
            }
        }
        Ok(result)
    }
}

/// One collector subscription. Secret values are never persisted here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetrySink {
    /// Unique stable identifier.
    pub id: String,
    /// Collector URL.
    pub url: String,
    /// Delivery method.
    #[serde(default)]
    pub method: SinkMethod,
    /// Snapshot interval in seconds.
    pub every_seconds: u32,
    /// Requested snapshots.
    #[serde(default)]
    pub sections: Vec<TelemetrySection>,
    /// Committed facts to deliver.
    #[serde(default)]
    pub events: Vec<HookKind>,
    /// Non-secret HTTP headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Environment variable containing a bearer token.
    #[serde(default)]
    pub bearer_env: Option<String>,
    /// Owner-only token file relative to the state directory.
    #[serde(default)]
    pub bearer_file: Option<String>,
    /// Static collector labels.
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    /// Whether delivery is enabled.
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// Ask this POST collector to admit user uploads; generated files bypass it.
    #[serde(default)]
    pub upload_admission: bool,
}
impl TelemetrySink {
    pub(crate) fn redact_report(&mut self) -> Result<()> {
        let url = url::Url::parse(&self.url)
            .map_err(|error| Error::Config(format!("telemetry endpoint is invalid: {error}")))?;
        // A collector can carry credentials in its path, query or custom headers.
        self.url = url.origin().ascii_serialization();
        self.headers.clear();
        self.bearer_env = None;
        self.bearer_file = None;
        Ok(())
    }
}

const fn enabled() -> bool {
    true
}
/// In-memory delivery status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetrySinkStatus {
    /// Most recent attempt.
    pub last_attempt_at: Option<i64>,
    /// Most recent successful delivery.
    pub last_success_at: Option<i64>,
    /// Last HTTP status.
    pub last_status: Option<u16>,
    /// Bounded diagnostic, excluding credentials.
    pub last_error: Option<String>,
    /// Failures since last success.
    pub consecutive_failures: u32,
    /// Next snapshot time.
    pub next_at: Option<i64>,
    /// Undelivered matching facts.
    pub events_pending: u64,
    /// A request is running.
    pub in_flight: bool,
}
/// Safe endpoint report returned to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetrySinkReport {
    /// Configuration without credential source names.
    pub sink: TelemetrySink,
    /// Credential source kind; never its name or value.
    pub auth: SinkAuth,
    /// Current delivery state.
    pub status: TelemetrySinkStatus,
}

/// Redacted authentication mechanism exposed to paired clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkAuth {
    /// No bearer credential.
    None,
    /// Credential read from the gateway environment.
    BearerEnv,
    /// Credential read from a protected file.
    BearerFile,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopCause {
    Idle,
    Signal,
    LeaseExpired,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "reason", content = "cause", rename_all = "snake_case")]
pub(crate) enum Trigger {
    Start,
    Interval,
    Events,
    Manual,
    Stop(StopCause),
}

pub(crate) struct Telemetry {
    pub(crate) notify: Arc<tokio::sync::Notify>,
    // GatewayConfig owns the persisted configuration; this snapshot is refreshed by
    // configure_telemetry after saving and shared with in-flight deliveries.
    config: RwLock<Arc<TelemetryConfig>>,
    state_dir: std::path::PathBuf,
    client: Result<HttpClient>,
    statuses: Mutex<BTreeMap<String, TelemetrySinkStatus>>,
    manual: Mutex<std::collections::BTreeSet<String>>,
    sequence: AtomicU64,
    instance: String,
    started_at_ms: i64,
    started: Instant,
    resources: resources::Resources,
    contexts: Mutex<VecDeque<(String, ChatContext)>>,
}

/// Chats noted at most; the oldest note gives way.
const MAX_CHAT_CONTEXTS: usize = 64;

#[derive(Serialize)]
struct ChatContext {
    window: Option<i64>,
    input_tokens: i64,
    compaction_buffer: i64,
    parts: Vec<mobius::protocol::ContextPart>,
}

impl Telemetry {
    /// Keeps `session`'s latest breakdown for the `context` section.
    pub(crate) fn note_context(&self, session: &str, usage: &mobius::protocol::TokenUsageInfo) {
        let Some(context) = &usage.context else {
            return;
        };
        let mut contexts = mobius::sync::recover_lock(&self.contexts);
        let session = contexts
            .iter()
            .position(|(id, _)| id == session)
            .and_then(|index| contexts.remove(index))
            .map_or_else(|| session.to_owned(), |(id, _)| id);
        let chat = ChatContext {
            window: usage.model_context_window,
            input_tokens: usage.last_token_usage.input_tokens,
            compaction_buffer: context.compaction_buffer,
            parts: context.parts.clone(), // rules-allow: the note outlives the usage report
        };
        contexts.push_back((session, chat));
        if contexts.len() > MAX_CHAT_CONTEXTS {
            contexts.pop_front();
        }
    }

    pub(crate) fn contexts(&self) -> Value {
        Value::Array(
            mobius::sync::recover_lock(&self.contexts)
                .iter()
                .map(|(session, chat)| json!({"session_id": session, "context": chat}))
                .collect(),
        )
    }

    pub(crate) fn new(config: &TelemetryConfig, state_dir: &std::path::Path) -> Self {
        Self {
            notify: Arc::new(tokio::sync::Notify::new()),
            config: RwLock::new(Arc::new(config.clone())), // rules-allow: the live copy is shared with deliveries apart from the durable configuration
            state_dir: state_dir.to_path_buf(),
            // One connection pool and root store for the gateway's lifetime.
            client: HttpClient::builder()
                .redirect(HttpRedirectPolicy::none())
                .timeout(Duration::from_secs(config.policy.request_timeout_seconds))
                .build()
                .map_err(|error| {
                    Error::Config(format!(
                        "telemetry HTTP client initialization failed: {error}"
                    ))
                }),
            statuses: Mutex::new(
                config
                    .sinks
                    .iter()
                    .map(|sink| (sink.id.clone(), TelemetrySinkStatus::default())) // rules-allow: the status map owns its keys
                    .collect(),
            ),
            manual: Mutex::default(),
            sequence: AtomicU64::new(0),
            instance: uuid::Uuid::new_v4().to_string(),
            started_at_ms: chrono::Utc::now().timestamp_millis(),
            started: Instant::now(),
            resources: resources::Resources::default(),
            contexts: Mutex::default(),
        }
    }
    pub(crate) async fn resources(&self) -> Value {
        self.resources.sample().await
    }

    pub(crate) fn config(&self) -> Result<Arc<TelemetryConfig>> {
        self.config
            .read()
            .map(|config| Arc::clone(&config))
            .map_err(crate::poisoned("telemetry configuration"))
    }

    fn header(&self, sink: &TelemetrySink, now: i64) -> Value {
        json!({"version": 1, "sent_at": now, "sequence": self.sequence.fetch_add(1, Ordering::Relaxed),
            "instance": self.instance, "gateway_version": env!("CARGO_PKG_VERSION"),
            "protocol_version": crate::wire::PROTOCOL_VERSION, "started_at_ms": self.started_at_ms,
            "uptime_seconds": self.started.elapsed().as_secs(), "fields": sink.fields})
    }
    // Reserve runtime mutation before changing durable configuration, so assignment cannot fail afterward.
    pub(crate) fn configure_after(
        &self,
        operation: impl FnOnce() -> Result<(TelemetryConfig, crate::publication::Outcome)>,
    ) -> Result<()> {
        let mut live = self
            .config
            .write()
            .map_err(crate::poisoned("telemetry configuration"))?;
        let mut statuses = self
            .statuses
            .lock()
            .map_err(crate::poisoned("telemetry status"))?;
        let (config, publication) = operation()?;
        statuses.retain(|id, _| config.sinks.iter().any(|sink| &sink.id == id));
        for sink in &config.sinks {
            match statuses.get_mut(&sink.id) {
                Some(status) => status.next_at = None,
                None => {
                    statuses.insert(sink.id.clone(), TelemetrySinkStatus::default()); // rules-allow: the status map owns its keys
                }
            }
        }
        *live = Arc::new(config);
        drop(statuses);
        drop(live);
        publication.confirm()
    }
    pub(crate) fn request_manual(&self, id: String) -> Result<()> {
        if !self
            .config()?
            .sinks
            .iter()
            .any(|sink| sink.id == id && sink.enabled)
        {
            return Err(Error::Config("unknown or disabled telemetry sink".into()));
        }
        self.manual
            .lock()
            .map_err(crate::poisoned("telemetry manual request"))?
            .insert(id);
        Ok(())
    }
    pub(crate) fn status(&self, id: &str) -> Result<TelemetrySinkStatus> {
        // The report owns its diagnostic strings after the short lock is released.
        Ok(self
            .statuses
            .lock()
            .map_err(crate::poisoned("telemetry status"))?
            .get(id)
            .cloned() // rules-allow: the report outlives the status lock
            .unwrap_or_default())
    }
    pub(crate) fn can_drain(&self, id: &str) -> Result<bool> {
        Ok(self
            .statuses
            .lock()
            .map_err(crate::poisoned("telemetry status"))?
            .get(id)
            .is_none_or(|status| status.consecutive_failures < 3))
    }
    /// Serve-loop boundary: collector failures must never stop the gateway.
    #[expect(
        clippy::unused_async,
        reason = "the serve loop awaits every telemetry step through one async boundary"
    )]
    pub(crate) async fn tick(
        host: &GatewayHost,
        clients: usize,
        trigger: Trigger,
        tasks: &mut tokio::task::JoinSet<()>,
    ) {
        if !tasks.is_empty() {
            return;
        }
        let config = match host.telemetry.config() {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(%error, "telemetry scheduling failed");
                return;
            }
        };
        if !config.sinks.iter().any(|sink| sink.enabled) {
            return;
        }
        let host = host.clone(); // rules-allow: the telemetry worker owns its host handle
        tasks.spawn(async move {
            let mut deliveries = tokio::task::JoinSet::new();
            if let Err(error) = Self::tick_at(
                &host,
                clients,
                trigger,
                &mut deliveries,
                chrono::Utc::now().timestamp(),
            )
            .await
            {
                tracing::warn!(%error, "telemetry scheduling failed");
            }
            while deliveries.join_next().await.is_some() {}
        });
    }
    pub(crate) async fn stop(
        host: &GatewayHost,
        cause: StopCause,
        tasks: &mut tokio::task::JoinSet<()>,
    ) {
        // Cancelling the worker drops its JoinSet, cancelling its deliveries too.
        // Pending event cursors remain unacknowledged and are retried after restart.
        tasks.shutdown().await;
        if let Ok(mut statuses) = host.telemetry.statuses.lock() {
            for status in statuses.values_mut() {
                status.in_flight = false;
            }
        } else {
            tracing::warn!("telemetry stop status lock poisoned");
        }
        Self::tick(host, 0, Trigger::Stop(cause), tasks).await;
        if tokio::time::timeout(Duration::from_secs(5), async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            tracing::warn!("telemetry stop delivery timed out");
        }
        tasks.shutdown().await;
    }
    pub(crate) async fn tick_at(
        host: &GatewayHost,
        clients: usize,
        trigger: Trigger,
        tasks: &mut tokio::task::JoinSet<()>,
        now: i64,
    ) -> Result<()> {
        let config = host.telemetry.config()?;
        let mut snapshot = Snapshot {
            clients,
            sections: BTreeMap::new(),
        };
        for (index, sink) in config
            .sinks
            .iter()
            .enumerate()
            .filter(|(_, sink)| sink.enabled)
        {
            // A broken source or sink cannot starve the remaining destinations.
            if let Err(error) = Self::schedule(
                host,
                trigger,
                Arc::clone(&config),
                index,
                tasks,
                now,
                &mut snapshot,
            )
            .await
            {
                tracing::warn!(sink_id = %sink.id, %error, "telemetry scheduling failed");
                if let Ok(mut statuses) = host.telemetry.statuses.lock() {
                    let Some(status) = statuses.get_mut(&sink.id) else {
                        continue;
                    };
                    status.in_flight = false;
                    status.last_attempt_at = Some(now);
                    status.last_error = Some(error.to_string());
                    status.consecutive_failures = status.consecutive_failures.saturating_add(1);
                    status.next_at = Some(now.saturating_add(i64::from(sink.every_seconds)));
                } else {
                    tracing::warn!("telemetry status lock poisoned");
                }
            }
        }
        Ok(())
    }
    async fn schedule(
        host: &GatewayHost,
        trigger: Trigger,
        config: Arc<TelemetryConfig>,
        index: usize,
        tasks: &mut tokio::task::JoinSet<()>,
        now: i64,
        snapshot: &mut Snapshot,
    ) -> Result<()> {
        let sink = config
            .sinks
            .get(index)
            .ok_or_else(|| Error::Config("telemetry sink disappeared".into()))?;
        let manual = host
            .telemetry
            .manual
            .lock()
            .map_err(crate::poisoned("telemetry manual request"))?
            .contains(&sink.id);
        let (snapshot_due, retry_due) = {
            let statuses = host
                .telemetry
                .statuses
                .lock()
                .map_err(crate::poisoned("telemetry status"))?;
            let status = statuses.get(&sink.id);
            let in_flight = status.is_some_and(|status| status.in_flight);
            let next_due = status
                .and_then(|status| status.next_at)
                .is_none_or(|at| at <= now);
            let retry_due = status.is_none_or(|status| {
                status.consecutive_failures == 0
                    || status
                        .last_attempt_at
                        .is_none_or(|at| now.saturating_sub(at) >= 15)
            });
            drop(statuses);
            if in_flight {
                return Ok(());
            }
            let snapshot_due = match trigger {
                Trigger::Events => false,
                Trigger::Interval => manual || next_due,
                Trigger::Start | Trigger::Manual | Trigger::Stop(_) => true,
            };
            (snapshot_due, retry_due)
        };
        let mut cursor = None;
        let mut pending = 0;
        let mut envelope = if snapshot_due {
            let sections = if matches!(trigger, Trigger::Stop(_)) {
                &[][..]
            } else {
                &sink.sections
            };
            snapshot.read(host, sections).await?
        } else {
            if sink.events.is_empty() || !retry_due {
                return Ok(());
            }
            let (events, after, count) = host.telemetry_events(sink).await?;
            if events.is_empty() {
                return Ok(());
            }
            cursor = after;
            pending = count;
            let mut header = snapshot.read(host, &[]).await?;
            header
                .as_object_mut()
                .ok_or_else(|| Error::Config("telemetry snapshot is not an object".into()))?
                .insert("events".into(), json!(events));
            header
        };
        let reason = if !snapshot_due {
            Trigger::Events
        } else if manual && matches!(trigger, Trigger::Interval) {
            Trigger::Manual
        } else {
            trigger
        };
        let header = host.telemetry.header(sink, now);
        let object = envelope
            .as_object_mut()
            .ok_or_else(|| Error::Config("telemetry snapshot is not an object".into()))?;
        if let Value::Object(header) = header {
            object.extend(header);
        }
        if let Value::Object(reason) = serde_json::to_value(reason)? {
            object.extend(reason);
        }
        {
            let mut statuses = host
                .telemetry
                .statuses
                .lock()
                .map_err(crate::poisoned("telemetry status"))?;
            let Some(status) = statuses.get_mut(&sink.id) else {
                return Ok(());
            };
            status.in_flight = true;
            status.last_attempt_at = Some(now);
            status.events_pending = pending;
            if snapshot_due {
                status.next_at = Some(now.saturating_add(i64::from(sink.every_seconds)));
            }
            drop(statuses);
        }
        if manual {
            host.telemetry
                .manual
                .lock()
                .map_err(crate::poisoned("telemetry manual request"))?
                .remove(&sink.id);
        }
        let host = host.clone(); // rules-allow: the delivery task owns its host handle
        tasks.spawn(async move {
            let Some(sink) = config.sinks.get(index) else {
                return;
            };
            let result = deliver(&host.telemetry, sink, &envelope, 0).await;
            let http = match &result {
                Ok((status, _)) => Some(*status),
                Err(error) => error.status,
            };
            let acknowledge = match &result {
                Ok(_) => true,
                Err(error) => error.permanent,
            };
            let error = if acknowledge
                && let Some(cursor) = cursor
                && let Err(failure) = host
                    .advance_telemetry(&sink.id, cursor, config.revision)
                    .await
            {
                Some(failure.to_string())
            } else {
                result.err().map(|error| error.message)
            };
            if let Ok(mut statuses) = host.telemetry.statuses.lock() {
                if let Some(status) = statuses.get_mut(&sink.id) {
                    status.in_flight = false;
                    status.last_status = http;
                    if error.is_none() {
                        status.last_success_at = Some(chrono::Utc::now().timestamp());
                        status.consecutive_failures = 0;
                    } else {
                        status.consecutive_failures = status.consecutive_failures.saturating_add(1);
                    }
                    status.last_error = error;
                }
            } else {
                tracing::warn!("telemetry delivery status lock poisoned");
            }
            host.telemetry.notify.notify_one();
        });
        Ok(())
    }
    pub(crate) async fn pending(host: &GatewayHost) -> bool {
        match host.telemetry_pending().await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%error, "telemetry pending check failed");
                false
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct DeliveryError {
    status: Option<u16>,
    message: String,
    permanent: bool,
}
impl DeliveryError {
    fn caused(context: &str, cause: &dyn std::error::Error) -> Self {
        let mut message = format!("{context}: {cause}");
        let mut source = cause.source();
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        Self {
            status: None,
            message,
            permanent: false,
        }
    }
    fn transient(message: &str) -> Self {
        Self {
            status: None,
            message: message.into(),
            permanent: false,
        }
    }
}
async fn deliver(
    telemetry: &Telemetry,
    sink: &TelemetrySink,
    envelope: &Value,
    response_limit: usize,
) -> std::result::Result<(u16, Vec<u8>), DeliveryError> {
    let error = DeliveryError::transient;
    let client = telemetry.client.as_ref().map_err(|cause| {
        error(&format!(
            "telemetry HTTP client initialization failed: {cause}"
        ))
    })?;
    let mut request = match sink.method {
        SinkMethod::Post => {
            let bytes = serde_json::to_vec(envelope)
                .map_err(|cause| error(&format!("telemetry encoding failed: {cause}")))?;
            if bytes.len() > 64 * 1024 {
                return Err(DeliveryError {
                    status: None,
                    message: "telemetry envelope exceeds 64 KiB".into(),
                    permanent: true,
                });
            }
            client
                .post(&sink.url)
                .header("content-type", "application/json")
                .body(bytes)
        }
        SinkMethod::Get => client.get(&sink.url),
    };
    for (name, value) in &sink.headers {
        request = request.header(name, value);
    }
    let token =
        if let Some(name) = &sink.bearer_env {
            Some(std::env::var(name).map_err(|cause| {
                error(&format!("bearer environment variable unavailable: {cause}"))
            })?)
        } else if let Some(path) = &sink.bearer_file {
            let path = telemetry.state_dir.join(path);
            let state_dir = &telemetry.state_dir;
            let canonical = tokio::fs::canonicalize(&path)
                .await
                .map_err(|cause| error(&format!("bearer file unavailable: {cause}")))?;
            if !canonical.starts_with(state_dir) {
                return Err(error("bearer file escapes state directory"));
            }
            Some(
                tokio::task::spawn_blocking(move || crate::config::load_secret_file(&path))
                    .await
                    .map_err(|cause| error(&format!("bearer file task failed: {cause}")))?
                    .map_err(|cause| error(&format!("invalid bearer file: {cause}")))?,
            )
        } else {
            None
        };
    if let Some(token) = token {
        if token.is_empty()
            || token.len() > 16 * 1024
            || !token.bytes().all(|b| (33..=126).contains(&b))
        {
            return Err(error("invalid bearer token"));
        }
        request = request.bearer_auth(token);
    }
    // Transport diagnostics intentionally omit the URL, which may contain a collector secret.
    let mut response = request
        .send()
        .await
        .map_err(|cause| DeliveryError::caused("telemetry request failed", &cause.without_url()))?;
    let status = response.status();
    if status.is_success() {
        let mut body = Vec::new();
        if response_limit > 0 {
            while let Some(chunk) = response.chunk().await.map_err(|cause| {
                DeliveryError::caused("telemetry response failed", &cause.without_url())
            })? {
                if chunk.len() > response_limit.saturating_sub(body.len()) {
                    return Err(error("telemetry response exceeds its size limit"));
                }
                body.extend_from_slice(&chunk);
            }
        }
        return Ok((status.as_u16(), body));
    }
    Err(DeliveryError {
        status: Some(status.as_u16()),
        message: format!("telemetry collector returned HTTP {}", status.as_u16()),
        permanent: status.is_client_error() && status.as_u16() != 408 && status.as_u16() != 429,
    })
}

#[cfg(test)]
mod context_tests {
    use super::*;
    use mobius::protocol::{
        ContextBreakdown, ContextPart, ContextPartKind, TokenUsage, TokenUsageInfo,
    };

    fn usage(input_tokens: i64) -> TokenUsageInfo {
        let last = TokenUsage {
            input_tokens,
            ..TokenUsage::default()
        };
        TokenUsageInfo {
            total_token_usage: TokenUsage::default(),
            last_token_usage: last,
            model_context_window: Some(1_000),
            context: Some(ContextBreakdown {
                parts: vec![ContextPart {
                    kind: ContextPartKind::Messages,
                    tokens: input_tokens,
                    items: 1,
                }],
                compaction_buffer: 100,
            }),
        }
    }

    #[test]
    fn keeps_each_chats_latest_breakdown_for_the_newest_chats() {
        let state = tempfile::tempdir().expect("state");
        let telemetry = Telemetry::new(&TelemetryConfig::default(), state.path());
        for chat in 0..=MAX_CHAT_CONTEXTS {
            telemetry.note_context(&format!("chat-{chat}"), &usage(10));
        }
        telemetry.note_context("chat-1", &usage(20));
        let contexts = telemetry.contexts();
        let chats = contexts.as_array().expect("chats");
        assert_eq!(chats.len(), MAX_CHAT_CONTEXTS);
        assert!(!chats.iter().any(|chat| chat["session_id"] == "chat-0"));
        assert_eq!(chats[0]["session_id"], "chat-2");
        let latest = chats.last().expect("latest chat");
        assert_eq!(latest["session_id"], "chat-1");
        assert_eq!(latest["context"]["input_tokens"], 20);
        assert_eq!(latest["context"]["parts"][0]["kind"], "messages");
    }
}
