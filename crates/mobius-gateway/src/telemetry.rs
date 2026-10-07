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
use std::collections::BTreeMap;

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
}
impl TelemetrySection {
    const fn key(self) -> &'static str {
        match self {
            Self::Activity => "activity",
            Self::Usage => "usage",
            Self::Runs => "runs",
            Self::Storage => "storage",
            Self::Resources => "resources",
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
                // Each in-flight request owns its payload after this tick's snapshot is dropped.
                result[section.key()] = value.clone();
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
            .map_err(|_| Error::Config("telemetry endpoint is invalid".into()))?;
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
}
impl Telemetry {
    pub(crate) fn new(config: &TelemetryConfig, state_dir: &std::path::Path) -> Self {
        Self {
            notify: Arc::new(tokio::sync::Notify::new()),
            config: RwLock::new(Arc::new(config.clone())),
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
                    .map(|sink| (sink.id.clone(), TelemetrySinkStatus::default()))
                    .collect(),
            ),
            manual: Mutex::default(),
            sequence: AtomicU64::new(0),
            instance: uuid::Uuid::new_v4().to_string(),
            started_at_ms: chrono::Utc::now().timestamp_millis(),
            started: Instant::now(),
            resources: resources::Resources::default(),
        }
    }
    pub(crate) async fn resources(&self) -> Value {
        self.resources.sample().await
    }

    pub(crate) fn config(&self) -> Result<Arc<TelemetryConfig>> {
        self.config
            .read()
            .map(|config| Arc::clone(&config))
            .map_err(|_| Error::Config("telemetry configuration lock poisoned".into()))
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
            .map_err(|_| Error::Config("telemetry configuration lock poisoned".into()))?;
        let mut statuses = self
            .statuses
            .lock()
            .map_err(|_| Error::Config("telemetry status lock poisoned".into()))?;
        let (config, publication) = operation()?;
        statuses.retain(|id, _| config.sinks.iter().any(|sink| &sink.id == id));
        for sink in &config.sinks {
            match statuses.get_mut(&sink.id) {
                Some(status) => status.next_at = None,
                None => {
                    statuses.insert(sink.id.clone(), TelemetrySinkStatus::default());
                }
            }
        }
        *live = Arc::new(config);
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
            .map_err(|_| Error::Config("telemetry manual request lock poisoned".into()))?
            .insert(id);
        Ok(())
    }
    pub(crate) fn status(&self, id: &str) -> Result<TelemetrySinkStatus> {
        // The report owns its diagnostic strings after the short lock is released.
        Ok(self
            .statuses
            .lock()
            .map_err(|_| Error::Config("telemetry status lock poisoned".into()))?
            .get(id)
            .cloned()
            .unwrap_or_default())
    }
    pub(crate) fn can_drain(&self, id: &str) -> Result<bool> {
        Ok(self
            .statuses
            .lock()
            .map_err(|_| Error::Config("telemetry status lock poisoned".into()))?
            .get(id)
            .is_none_or(|status| status.consecutive_failures < 3))
    }
    /// Serve-loop boundary: collector failures must never stop the gateway.
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
        let host = host.clone();
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
        match host.telemetry.statuses.lock() {
            Ok(mut statuses) => {
                for status in statuses.values_mut() {
                    status.in_flight = false;
                }
            }
            Err(_) => tracing::warn!("telemetry stop status lock poisoned"),
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
                match host.telemetry.statuses.lock() {
                    Ok(mut statuses) => {
                        let Some(status) = statuses.get_mut(&sink.id) else {
                            continue;
                        };
                        status.in_flight = false;
                        status.last_attempt_at = Some(now);
                        status.last_error = Some(error.to_string());
                        status.consecutive_failures = status.consecutive_failures.saturating_add(1);
                        status.next_at = Some(now.saturating_add(i64::from(sink.every_seconds)));
                    }
                    Err(_) => tracing::warn!("telemetry status lock poisoned"),
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
        let sink = &config.sinks[index];
        let manual = host
            .telemetry
            .manual
            .lock()
            .map_err(|_| Error::Config("telemetry manual request lock poisoned".into()))?
            .contains(&sink.id);
        let (snapshot_due, retry_due) = {
            let statuses = host
                .telemetry
                .statuses
                .lock()
                .map_err(|_| Error::Config("telemetry status lock poisoned".into()))?;
            let status = statuses.get(&sink.id);
            if status.is_some_and(|status| status.in_flight) {
                return Ok(());
            }
            let snapshot_due = match trigger {
                Trigger::Events => false,
                Trigger::Interval => {
                    manual
                        || status
                            .and_then(|status| status.next_at)
                            .is_none_or(|at| at <= now)
                }
                Trigger::Start | Trigger::Manual | Trigger::Stop(_) => true,
            };
            let retry_due = status.is_none_or(|status| {
                status.consecutive_failures == 0
                    || status
                        .last_attempt_at
                        .is_none_or(|at| now.saturating_sub(at) >= 15)
            });
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
            header["events"] = json!(events);
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
                .map_err(|_| Error::Config("telemetry status lock poisoned".into()))?;
            let Some(status) = statuses.get_mut(&sink.id) else {
                return Ok(());
            };
            status.in_flight = true;
            status.last_attempt_at = Some(now);
            status.events_pending = pending;
            if snapshot_due {
                status.next_at = Some(now.saturating_add(i64::from(sink.every_seconds)));
            }
        }
        if manual {
            host.telemetry
                .manual
                .lock()
                .map_err(|_| Error::Config("telemetry manual request lock poisoned".into()))?
                .remove(&sink.id);
        }
        let host = host.clone();
        tasks.spawn(async move {
            let sink = &config.sinks[index];
            let result = deliver(&host.telemetry, sink, &envelope, 0).await;
            let http = match &result {
                Ok((status, _)) => Some(*status),
                Err(error) => error.status,
            };
            let acknowledge = match &result {
                Ok(_) => true,
                Err(error) => error.permanent,
            };
            let mut error = result.err().map(|error| error.message);
            if acknowledge
                && let Some(cursor) = cursor
                && let Err(failure) = host
                    .advance_telemetry(&sink.id, cursor, config.revision)
                    .await
            {
                error = Some(failure.to_string());
            }
            match host.telemetry.statuses.lock() {
                Ok(mut statuses) => {
                    if let Some(status) = statuses.get_mut(&sink.id) {
                        status.in_flight = false;
                        status.last_status = http;
                        if error.is_none() {
                            status.last_success_at = Some(chrono::Utc::now().timestamp());
                            status.consecutive_failures = 0;
                        } else {
                            status.consecutive_failures =
                                status.consecutive_failures.saturating_add(1);
                        }
                        status.last_error = error;
                    }
                }
                Err(_) => tracing::warn!("telemetry delivery status lock poisoned"),
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
        use std::fmt::Write as _;
        let mut message = format!("{context}: {cause}");
        let mut source = cause.source();
        while let Some(cause) = source {
            // Writing to a String is infallible.
            let _ = write!(message, ": {cause}");
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
    let token = if let Some(name) = &sink.bearer_env {
        Some(std::env::var(name).map_err(|_| error("bearer environment variable unavailable"))?)
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
