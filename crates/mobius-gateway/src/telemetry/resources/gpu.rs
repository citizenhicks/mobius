//! Best-effort, unprivileged local GPU counters; never remote provider metrics.
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::io::AsyncReadExt as _;

pub(super) async fn sample() -> Value {
    match tokio::time::timeout(super::DEADLINE, probe()).await {
        Ok(Ok(value)) => value,
        Ok(Err(_)) => super::unavailable("gpu_probe_unavailable"),
        Err(_) => super::unavailable("timeout"),
    }
}

async fn probe() -> crate::Result<Value> {
    #[cfg(target_os = "macos")]
    {
        let bytes = output("/usr/sbin/ioreg", &["-r", "-c", "IOAccelerator", "-a"]).await?;
        apple(&bytes)
    }
    #[cfg(target_os = "linux")]
    {
        // ponytail: one GPU source per sample; merge inventories if mixed-vendor hosts need it.
        let bytes = output(
            "/usr/bin/nvidia-smi",
            &[
                "--query-gpu=name,utilization.gpu,memory.used,memory.total",
                "--format=csv,noheader,nounits",
            ],
        )
        .await?;
        nvidia(&bytes)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    Ok(json!({"status": "unsupported"}))
}

pub(super) fn fallback() -> Value {
    #[cfg(target_os = "linux")]
    {
        drm(std::path::Path::new("/sys/class/drm"))
            .unwrap_or_else(|_| super::unavailable("gpu_probe_unavailable"))
    }
    #[cfg(not(target_os = "linux"))]
    Value::Null
}

#[cfg(target_os = "linux")]
fn drm(root: &std::path::Path) -> crate::Result<Value> {
    let entries = std::fs::read_dir(root)?;
    let mut devices = Vec::new();
    for entry in entries.take(256) {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name
            .strip_prefix("card")
            .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let path = entry.path().join("device");
        let number = |file: &str| -> Option<u64> {
            std::fs::read_to_string(path.join(file))
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        let usage = number("gpu_busy_percent").filter(|v| *v <= 100);
        devices.push(json!({"name": name, "utilization_percent": usage,
            "dedicated_memory_used_bytes": number("mem_info_vram_used"),
            "dedicated_memory_total_bytes": number("mem_info_vram_total"),
        }));
        if devices.len() == 64 {
            break;
        }
    }
    Ok(
        json!({"status": if devices.is_empty() { "not_detected" } else { "ok" }, "source": "drm_sysfs", "devices": devices}),
    )
}

async fn output(program: &str, args: &[&str]) -> crate::Result<Vec<u8>> {
    const MAX_BYTES: u64 = 256 * 1024;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| crate::Error::Config("GPU probe stdout missing".into()))?;
    let mut bytes = Vec::new();
    stdout.take(MAX_BYTES + 1).read_to_end(&mut bytes).await?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_BYTES {
        return Err(crate::Error::Config(
            "GPU probe output exceeded limit".into(),
        ));
    }
    if !child.wait().await?.success() {
        return Err(crate::Error::Config("GPU probe failed".into()));
    }
    Ok(bytes)
}

#[cfg(target_os = "macos")]
fn apple(bytes: &[u8]) -> crate::Result<Value> {
    let value = plist::Value::from_reader_xml(bytes)
        .map_err(|error| crate::Error::Config(format!("invalid GPU property list: {error}")))?;
    let entries = value
        .as_array()
        .ok_or_else(|| crate::Error::Config("invalid GPU device list".into()))?;
    let devices = entries
        .iter()
        .take(64)
        .filter_map(|entry| {
            let entry = entry.as_dictionary()?;
            let stats = entry.get("PerformanceStatistics")?.as_dictionary()?;
            let number = |name| stats.get(name).and_then(plist::Value::as_unsigned_integer);
            Some(json!({
                "name": entry.get("model").and_then(plist::Value::as_string),
                "utilization_percent": number("Device Utilization %").filter(|v| *v <= 100),
                "system_memory_used_bytes": number("In use system memory"),
                "dedicated_memory_used_bytes": number("vramUsedBytes"),
            }))
        })
        .collect::<Vec<_>>();
    Ok(
        json!({"status": if devices.is_empty() { "unavailable" } else { "ok" }, "source": "ioreg", "devices": devices}),
    )
}

#[cfg(target_os = "linux")]
fn nvidia(bytes: &[u8]) -> crate::Result<Value> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| crate::Error::Config(format!("invalid GPU output: {error}")))?;
    let mut devices = Vec::new();
    for line in text.lines().take(64) {
        let fields: Vec<_> = line.split(',').map(str::trim).collect();
        let [name, usage, used, total] = fields.as_slice() else {
            return Err(crate::Error::Config("invalid GPU counter row".into()));
        };
        let mib = |value: &str| value.parse::<u64>().ok()?.checked_mul(1024 * 1024);
        devices.push(json!({"name": name,
            "utilization_percent": usage.parse::<u64>().ok().filter(|v| *v <= 100),
            "dedicated_memory_used_bytes": mib(used), "dedicated_memory_total_bytes": mib(total)}));
    }
    Ok(
        json!({"status": if devices.is_empty() { "unavailable" } else { "ok" }, "source": "nvidia_smi", "devices": devices}),
    )
}
