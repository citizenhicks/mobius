//! Opt-in OS measurements, independent of session locks and collector delivery.
mod cgroup;
mod gpu;

use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use sysinfo::{DiskRefreshKind, Disks, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::sync::Mutex;

const DEADLINE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(super) struct Resources {
    sampler: Arc<Mutex<Sampler>>,
}

#[derive(Default)]
struct Sampler {
    system: System,
    disks: Disks,
    previous: Option<Instant>,
}

impl Resources {
    pub(super) async fn sample(&self) -> Value {
        // Moving the guard into the blocking worker also bounds outstanding work
        // if an OS query outlives its caller's deadline or cancellation.
        let Ok(mut sampler) = Arc::clone(&self.sampler).try_lock_owned() else {
            return unavailable("busy");
        };
        let worker = tokio::task::spawn_blocking(move || {
            let value = sampler.sample();
            (sampler, value)
        });
        let (measured, gpu) = tokio::join!(tokio::time::timeout(DEADLINE, worker), gpu::sample());
        let mut measured = match measured {
            Ok(Ok((_sampler, value))) => value,
            Ok(Err(_)) => unavailable("measurement_failed"),
            Err(_) => unavailable("timeout"),
        };
        // The blocking worker owns any uncancellable DRM reads. A failed command
        // uses its fallback; successful NVIDIA counters take precedence.
        if gpu["status"] != "unavailable" || measured["gpu"].is_null() {
            measured["gpu"] = gpu;
        }
        measured
    }
}

impl Sampler {
    fn sample(&mut self) -> Value {
        let now = Instant::now();
        let interval = self.previous.map(|previous| now.duration_since(previous));
        // Manual sends can be closer together than the OS CPU sampling window.
        let ready = interval.is_some_and(|elapsed| elapsed >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        if interval.is_none() || ready {
            self.system.refresh_cpu_usage();
            self.previous = Some(now);
        }
        self.system.refresh_memory();
        let pid = sysinfo::get_current_pid().ok();
        let updated = if let Some(pid) = pid {
            if interval.is_none() || ready {
                self.system.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[pid]),
                    true,
                    ProcessRefreshKind::nothing()
                        .without_tasks()
                        .with_memory()
                        .with_disk_usage()
                        .with_cpu(),
                ) > 0
            } else {
                // Even a non-CPU sysinfo refresh advances Linux process CPU
                // baselines. Reuse process counters during the short CPU window.
                self.system.process(pid).is_some()
            }
        } else {
            false
        };
        self.disks
            .refresh_specifics(true, DiskRefreshKind::nothing().with_io_usage());
        let gateway = pid
            .filter(|_| updated)
            .and_then(|pid| self.system.process(pid))
            .map(|process| {
                let io = process.disk_usage();
                json!({
                    "cpu_percent": ready.then(|| process.cpu_usage()),
                    "cpu_time_ms": process.accumulated_cpu_time(),
                    "resident_memory_bytes": process.memory(),
                    "read_bytes_total": io.total_read_bytes,
                    "written_bytes_total": io.total_written_bytes,
                })
            });
        let mut seen = std::collections::BTreeSet::new();
        let disks: Vec<_> = self.disks.iter().filter(|disk| seen.insert(disk.name())).take(64).map(|disk| {
            let io = disk.usage();
            json!({"device": disk.name().to_string_lossy(),
                "read_bytes_total": io.total_read_bytes, "written_bytes_total": io.total_written_bytes})
        }).collect();
        json!({
            "version": 1,
            "status": "ok",
            "measured_at_ms": chrono::Utc::now().timestamp_millis(),
            "sample_interval_ms": interval.map(|elapsed| elapsed.as_millis()),
            "host": {
                "cpu_percent": ready.then(|| self.system.global_cpu_usage()),
                "logical_cpus": self.system.cpus().len(),
                "memory_total_bytes": self.system.total_memory(),
                "memory_available_bytes": self.system.available_memory(),
                "swap_total_bytes": self.system.total_swap(),
                "swap_used_bytes": self.system.used_swap(),
                "uptime_seconds": System::uptime(),
                "disks": disks,
            },
            "gateway": gateway,
            "container": cgroup::sample(),
            "gpu": gpu::fallback(),
        })
    }
}

fn unavailable(reason: &str) -> Value {
    json!({"status": "unavailable", "reason": reason})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resources_warm_up_and_bound_concurrent_measurements() {
        let resources = Resources::default();
        let held = resources.sampler.lock().await;
        assert_eq!(resources.sample().await["reason"], "busy");
        drop(held);
        let first = resources.sample().await;
        assert_eq!(first["status"], "ok");
        assert!(first["host"]["cpu_percent"].is_null());
        assert!(first["gateway"]["resident_memory_bytes"].as_u64().unwrap() > 0);
        tokio::time::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL).await;
        let second = resources.sample().await;
        assert!(second["host"]["cpu_percent"].as_f64().is_some());
        assert!(second["host"]["memory_total_bytes"].as_u64().unwrap() > 0);
        assert!(second["gpu"]["status"].is_string());
    }

    #[test]
    fn too_close_samples_preserve_process_counters_and_cpu_baseline() {
        let mut sampler = Sampler::default();
        let first = sampler.sample();
        let allocation = std::hint::black_box(vec![42_u8; 16 * 1024 * 1024]);
        let baseline = Instant::now();
        sampler.previous = Some(baseline);
        let quick = sampler.sample();
        assert_eq!(quick["gateway"], first["gateway"]);
        assert_eq!(sampler.previous, Some(baseline));
        assert!(quick["host"]["cpu_percent"].is_null());
        drop(allocation);
    }

    #[tokio::test]
    async fn timed_out_os_work_keeps_the_sampler_busy_until_it_finishes() {
        let resources = Resources::default();
        let guard = Arc::clone(&resources.sampler).try_lock_owned().unwrap();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let worker = tokio::task::spawn_blocking(move || {
            wait.recv().unwrap();
            guard
        });
        assert!(tokio::time::timeout(Duration::ZERO, worker).await.is_err());
        assert_eq!(resources.sample().await["reason"], "busy");
        release.send(()).unwrap();
        let guard = tokio::time::timeout(DEADLINE, resources.sampler.lock()).await;
        assert!(guard.is_ok());
    }
}
