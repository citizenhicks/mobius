Adds an optional `resources` telemetry section for host CPU and memory, gateway-process CPU and resident memory, disk counters, local cgroup-v2 limits and usage, and available GPU counters. Measurements stay inside the existing telemetry owner, are bounded, and report unavailable or warming-up counters explicitly. No telemetry endpoint is enabled by default. With no enabled sinks, no collector task or snapshot collection runs.

Uses Core 0.16.20, which keeps unrelated workspace commands and worker startup working when an optional skill directory disappears. Gateway wire protocol remains 91.
