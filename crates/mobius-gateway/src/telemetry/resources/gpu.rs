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

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "macos", test))]
fn apple(bytes: &[u8]) -> crate::Result<Value> {
    let value = plist::Value::from_reader_xml(bytes)
        .map_err(|_| crate::Error::Config("invalid GPU property list".into()))?;
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

#[cfg(any(target_os = "linux", test))]
fn nvidia(bytes: &[u8]) -> crate::Result<Value> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| crate::Error::Config("invalid GPU output".into()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_counters_distinguish_unavailable_devices_from_zero_usage() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(drm(root.path()).unwrap()["status"], "not_detected");
        let device = root.path().join("card0/device");
        std::fs::create_dir_all(&device).unwrap();
        std::fs::write(device.join("gpu_busy_percent"), "0").unwrap();
        std::fs::write(device.join("mem_info_vram_used"), "4096").unwrap();
        std::fs::create_dir_all(root.path().join("card0-DP-1")).unwrap();
        let value = drm(root.path()).unwrap();
        assert_eq!(value["devices"].as_array().unwrap().len(), 1);
        assert_eq!(value["devices"][0]["utilization_percent"], 0);
        assert_eq!(value["devices"][0]["dedicated_memory_used_bytes"], 4096);
        assert!(value["devices"][0]["dedicated_memory_total_bytes"].is_null());
    }

    #[test]
    fn gpu_counters_preserve_units_and_unavailable_values() {
        let value = nvidia(b"Test GPU, 40, 128, 8192\nTest GPU, [N/A], [N/A], 8192").unwrap();
        assert_eq!(
            value["devices"][0]["dedicated_memory_used_bytes"],
            128 * 1024 * 1024
        );
        assert!(value["devices"][1]["utilization_percent"].is_null());
        let xml = br#"<?xml version="1.0"?><plist version="1.0"><array><dict><key>model</key><string>Apple GPU</string><key>PerformanceStatistics</key><dict><key>Device Utilization %</key><integer>12</integer><key>In use system memory</key><integer>4096</integer></dict></dict></array></plist>"#;
        let value = apple(xml).unwrap();
        assert_eq!(value["devices"][0]["system_memory_used_bytes"], 4096);
        assert_eq!(value["devices"][0]["utilization_percent"], 12);
        assert!(value["devices"][0]["dedicated_memory_used_bytes"].is_null());
    }
}
