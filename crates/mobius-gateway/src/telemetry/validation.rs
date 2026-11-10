//! Checks of the operator's telemetry configuration at the configuration boundary.

use super::{SinkMethod, TelemetryConfig};
use crate::{Error, Result};

impl TelemetryConfig {
    /// Validates every collector and the policy they run under.
    pub(crate) fn validate(&self) -> Result<()> {
        self.policy.validate()?;
        if let Some(hook) = &self.activity_hook {
            hook.validate()?;
        }
        if self.sinks.len() > 16 {
            return Err(Error::Config(
                "telemetry.sinks accepts at most 16 destinations".into(),
            ));
        }
        let mut ids = std::collections::BTreeSet::new();
        if self
            .sinks
            .iter()
            .filter(|sink| sink.enabled && sink.upload_admission)
            .count()
            > 1
        {
            return Err(Error::Config(
                "telemetry accepts one enabled upload admission collector".into(),
            ));
        }
        for (index, sink) in self.sinks.iter().enumerate() {
            let invalid = |field: &str, requirement: &str| {
                Error::Config(format!("telemetry.sinks[{index}].{field} {requirement}"))
            };
            if sink.id.is_empty()
                || sink.id.len() > 64
                || !sink
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            {
                return Err(invalid(
                    "id",
                    "must contain 1–64 lowercase letters, digits, underscores or hyphens",
                ));
            }
            if !ids.insert(&sink.id) {
                return Err(invalid("id", "must be unique"));
            }
            let url = url::Url::parse(&sink.url).map_err(|error| {
                invalid("url", &format!("must be an absolute HTTP(S) URL: {error}"))
            })?;
            let loopback = match url.host() {
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                Some(url::Host::Domain("localhost")) => true,
                _ => false,
            };
            if url.host().is_none()
                || !(url.scheme() == "https"
                    || url.scheme() == "http" && (loopback || self.policy.allow_insecure_http))
            {
                return Err(invalid(
                    "url",
                    "requires HTTPS or an explicit operator HTTP policy",
                ));
            }
            if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
                return Err(invalid("url", "must not contain credentials or a fragment"));
            }
            if !(15..=86_400).contains(&sink.every_seconds)
                || !sink.every_seconds.is_multiple_of(15)
            {
                return Err(invalid(
                    "every_seconds",
                    "must be a multiple of 15 between 15 and 86400",
                ));
            }
            if sink.method == SinkMethod::Get && !sink.events.is_empty() {
                return Err(invalid("events", "requires the POST method"));
            }
            if sink.method == SinkMethod::Get && sink.upload_admission {
                return Err(invalid("upload_admission", "requires the POST method"));
            }
            if sink.headers.len() > 16 {
                return Err(invalid("headers", "accepts at most 16 headers"));
            }
            if sink.fields.len() > 16 {
                return Err(invalid("fields", "accepts at most 16 labels"));
            }
            if sink.bearer_env.is_some() && sink.bearer_file.is_some() {
                return Err(invalid("bearer_env", "cannot be combined with bearer_file"));
            }
            for (name, value) in &sink.headers {
                if name.is_empty()
                    || !name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                {
                    return Err(invalid("headers", "contains an invalid HTTP header name"));
                }
                if [
                    "authorization",
                    "proxy-authorization",
                    "cookie",
                    "host",
                    "content-length",
                    "transfer-encoding",
                ]
                .iter()
                .any(|reserved| name.eq_ignore_ascii_case(reserved))
                {
                    return Err(invalid(
                        "headers",
                        "contains a reserved credential or framing header",
                    ));
                }
                if value.len() > 1024 || !value.bytes().all(|b| (32..=126).contains(&b)) {
                    return Err(invalid(
                        "headers",
                        "values must be at most 1024 printable ASCII bytes",
                    ));
                }
            }
            if sink
                .fields
                .iter()
                .any(|(key, value)| key.is_empty() || key.len() > 64 || value.len() > 1024)
            {
                return Err(invalid(
                    "fields",
                    "keys must be 1–64 bytes and values at most 1024 bytes",
                ));
            }
            if serde_json::to_vec(&sink.fields)?.len() > 8192 {
                return Err(invalid("fields", "encoded labels must fit within 8 KiB"));
            }
            if let Some(name) = &sink.bearer_env
                && (!name.starts_with("MOBIUS_")
                    || name.len() == "MOBIUS_".len()
                    || !mobius::identifier::valid_ascii_identifier(
                        name,
                        128,
                        mobius::identifier::AsciiCase::Any,
                        b"_",
                    ))
            {
                return Err(invalid(
                    "bearer_env",
                    "must start with MOBIUS_ and contain at most 128 ASCII letters, digits or underscores",
                ));
            }
            if let Some(name) = &sink.bearer_env
                && crate::sandbox::reserved_credential_environment()
                    .any(|reserved| reserved == name)
            {
                return Err(invalid(
                    "bearer_env",
                    "cannot select a gateway or provider credential",
                ));
            }
            if let Some(path) = &sink.bearer_file
                && (path.is_empty()
                    || !std::path::Path::new(path)
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))))
            {
                return Err(invalid(
                    "bearer_file",
                    "must be a relative path without parent or current directory components",
                ));
            }
        }
        Ok(())
    }
}
