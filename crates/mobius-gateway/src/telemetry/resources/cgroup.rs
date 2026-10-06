//! Linux cgroup v2 counters. Paths are resolved from the process's own namespace.
use serde_json::{Value, json};

pub(super) fn sample() -> Value {
    #[cfg(target_os = "linux")]
    {
        match read_current() {
            Some(value) => value,
            None => super::unavailable("cgroup_v2_unavailable"),
        }
    }
    #[cfg(not(target_os = "linux"))]
    json!({"status": "unsupported"})
}

#[cfg(target_os = "linux")]
fn read_current() -> Option<Value> {
    let membership = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let path = resolve(&membership, &mounts)?;
    Some(read_at(&path))
}

#[cfg(any(target_os = "linux", test))]
fn resolve(membership: &str, mounts: &str) -> Option<std::path::PathBuf> {
    use std::path::{Component, Path};
    let group = Path::new(
        membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))?,
    );
    if !group.is_absolute()
        || group
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return None;
    }
    for line in mounts.lines() {
        let (fields, fs) = line.split_once(" - ")?;
        if !fs.starts_with("cgroup2 ") {
            continue;
        }
        let mut fields = fields.split_whitespace().skip(3);
        let root = fields.next()?;
        let mount = fields.next()?;
        // Escaped mount names are not guessed; report unavailable instead of reading a wrong scope.
        if root.contains('\\') || mount.contains('\\') {
            continue;
        }
        if let Ok(relative) = group.strip_prefix(root) {
            return Some(Path::new(mount).join(relative));
        }
    }
    None
}

#[cfg(any(target_os = "linux", test))]
fn read_at(path: &std::path::Path) -> Value {
    let read = |name| std::fs::read_to_string(path.join(name)).ok();
    let cpu = read("cpu.stat");
    let memory = read("memory.current").and_then(|v| v.trim().parse::<u64>().ok());
    let memory_limit = read("memory.max");
    let cpu_limit = read("cpu.max");
    let io = read("io.stat");
    if cpu.is_none() && memory.is_none() && io.is_none() {
        return super::unavailable("cgroup_counters_unreadable");
    }
    let quota = cpu_limit.as_deref().and_then(|v| {
        let mut fields = v.split_whitespace();
        let quota = fields.next()?.parse::<f64>().ok()?;
        let period = fields.next()?.parse::<f64>().ok()?;
        (quota.is_finite() && period.is_finite() && quota > 0.0 && period > 0.0)
            .then_some(quota / period)
    });
    let disks = io.as_deref().map(|io| {
        io.lines()
            .take(64)
            .map(|line| {
                let mut fields = line.split_whitespace();
                let device = fields.next().unwrap_or_default();
                let mut disk = json!({"device": device});
                for (key, value) in fields.filter_map(|field| field.split_once('=')) {
                    let name = match key {
                        "rbytes" => "read_bytes_total",
                        "wbytes" => "written_bytes_total",
                        "rios" => "read_operations_total",
                        "wios" => "write_operations_total",
                        _ => continue,
                    };
                    disk[name] = json!(value.parse::<u64>().ok());
                }
                disk
            })
            .collect::<Vec<_>>()
    });
    json!({
        "status": "ok", "scope": "current_cgroup_v2",
        "cpu_time_us": counter(cpu.as_deref(), "usage_usec"),
        "cpu_throttled_time_us": counter(cpu.as_deref(), "throttled_usec"),
        "cpu_throttled_periods_total": counter(cpu.as_deref(), "nr_throttled"),
        "local_cpu_quota_cores": quota,
        "local_cpu_quota_unlimited": cpu_limit.as_deref().map(|v| v.split_whitespace().next() == Some("max")),
        "memory_current_bytes": memory,
        "local_memory_limit_unlimited": memory_limit.as_deref().map(|v| v.trim() == "max"),
        "local_memory_limit_bytes": memory_limit.as_deref().and_then(|v| v.trim().parse::<u64>().ok()),
        "disks": disks,
    })
}

#[cfg(any(target_os = "linux", test))]
fn counter(text: Option<&str>, name: &str) -> Option<u64> {
    text?.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == name)
            .then(|| fields.next()?.parse().ok())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_scope_limits_and_io_are_explicit() {
        let mounts = "31 25 0:28 /tenant /sys/fs/cgroup rw - cgroup2 cgroup rw";
        assert_eq!(
            resolve("0::/tenant/job\n", mounts).unwrap(),
            std::path::Path::new("/sys/fs/cgroup/job")
        );
        assert!(resolve("0::/tenant/../other", mounts).is_none());
        assert!(resolve("0::/other", mounts).is_none());
        let root = tempfile::tempdir().unwrap();
        for (name, text) in [
            (
                "cpu.stat",
                "usage_usec 123\nthrottled_usec 4\nnr_throttled 2",
            ),
            ("memory.current", "500"),
            ("memory.max", "max"),
            ("cpu.max", "50000 100000"),
            ("io.stat", "8:0 rbytes=100 wbytes=200 rios=3 wios=4"),
        ] {
            std::fs::write(root.path().join(name), text).unwrap();
        }
        let value = read_at(root.path());
        assert_eq!(value["cpu_time_us"], 123);
        assert_eq!(value["local_cpu_quota_cores"], 0.5);
        assert!(value["local_memory_limit_bytes"].is_null());
        assert_eq!(value["disks"][0]["read_operations_total"], 3);
        assert!(counter(Some("usage_usec bad"), "usage_usec").is_none());
    }
}
