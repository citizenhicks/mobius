use super::*;
use crate::host::Rejection;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadDecision {
    request_id: String,
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
        let config = host.telemetry.config().map_err(|_| unavailable())?;
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
                    .map_err(|_| unavailable())?;
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
                    .map_err(|_| unavailable())?;
                if status != 200 {
                    return Err(unavailable());
                }
                let decision: UploadDecision =
                    serde_json::from_slice(&body).map_err(|_| unavailable())?;
                if decision.request_id != request_id {
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
                Err(Rejection {
                    code: "storage_full",
                    message,
                    fatal: false,
                })
            },
        )
        .await
        .map_err(|_| unavailable())?
    }
}

fn unavailable() -> Rejection {
    Rejection {
        code: "upload_admission_unavailable",
        message: "Cloud storage could not be checked. Try uploading again.".into(),
        fatal: false,
    }
}
