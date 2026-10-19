use super::{
    Arc, Deserialize, Duration, GatewayHost, Telemetry, TelemetrySection, Value, deliver, json,
};
use crate::host::Rejection;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadDecision {
    request_id: Arc<str>,
    allowed: bool,
    code: Option<String>,
    message: Option<String>,
}

impl Telemetry {
    pub(crate) async fn admit_upload(
        host: &GatewayHost,
        request_id: &str,
        bytes: u64,
    ) -> std::result::Result<(), Rejection> {
        let config = host.telemetry.config().map_err(unavailable_after(
            "upload admission configuration is unavailable",
        ))?;
        tokio::time::timeout(
            Duration::from_secs(config.policy.upload_admission_timeout_seconds),
            async {
                let Some(sink) = config
                    .sinks
                    .iter()
                    .find(|sink| sink.enabled && sink.upload_admission)
                else {
                    return Ok(());
                };
                let mut envelope = host
                    .telemetry_snapshot(&[TelemetrySection::Storage], 0)
                    .await
                    .map_err(unavailable_after("upload admission snapshot failed"))?;
                let object = envelope.as_object_mut().ok_or_else(unavailable)?;
                if let Value::Object(header) =
                    host.telemetry.header(sink, chrono::Utc::now().timestamp())
                {
                    object.extend(header);
                }
                object.insert("reason".into(), json!("upload"));
                object.insert(
                    "upload".into(),
                    json!({"request_id": request_id, "bytes": bytes}),
                );
                // ponytail: Cloud admits from a fresh snapshot; concurrent writes can exceed
                // the soft allowance. Add Cloud reservations only if an exact cap is required.
                let (status, body) = deliver(&host.telemetry, sink, &envelope, 4096)
                    .await
                    .map_err(unavailable_after("upload admission delivery failed"))?;
                if status != 200 {
                    return Err(unavailable());
                }
                let decision: UploadDecision = serde_json::from_slice(&body)
                    .map_err(unavailable_after("upload admission decision is invalid"))?;
                if *decision.request_id != *request_id {
                    return Err(unavailable());
                }
                if decision.allowed {
                    return if decision.code.is_none() && decision.message.is_none() {
                        Ok(())
                    } else {
                        Err(unavailable())
                    };
                }
                let message = decision.message.ok_or_else(unavailable)?;
                if decision.code.as_deref() != Some("storage_full")
                    || message.is_empty()
                    || message.len() > 256
                    || message.chars().any(char::is_control)
                {
                    return Err(unavailable());
                }
                Err(Rejection::new("storage_full", message))
            },
        )
        .await
        .map_err(unavailable_after("upload admission timed out"))?
    }
}

/// Keeps the client-facing rejection generic while recording why admission failed.
fn unavailable_after<E: std::fmt::Display>(context: &'static str) -> impl FnOnce(E) -> Rejection {
    move |error| {
        tracing::warn!(%error, "{context}");
        unavailable()
    }
}

fn unavailable() -> Rejection {
    Rejection::new(
        "upload_admission_unavailable",
        "Cloud storage could not be checked. Try uploading again.",
    )
}
