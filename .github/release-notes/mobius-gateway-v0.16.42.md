Updates the framework to 0.16.42, reducing streaming allocation churn and macOS Git discovery overhead.

Reduces event-accounting and SSE allocations, and makes macOS managed Git queries reuse developer-tool selection within each read request. Each command retains sandbox, trust-path, timeout and cancellation checks; the next request resolves tools again. No dependency, wire-protocol or persisted-state changes.

Measured locally on an M4 Max (128 GiB), macOS 27.0.1 and Rust 1.99.0, using isolated release builds and deterministic loopback fixtures:

| Workload | Before | After | Change |
| --- | ---: | ---: | ---: |
| Fresh session including initial Git status, median | 1,154.70 ms | 52.68 ms | 95.4% lower |
| Streaming cumulative Rust allocations, ten 256 KiB turns | 177,662,973 bytes | 151,980,852 bytes | 14.46% fewer |
| Streaming allocation blocks, same fixture | 82,051 | 63,789 | 22.26% fewer |

The Git request-scoping follow-up was measured separately against the earlier cold-start optimization in this release: warm unstaged totals fell **121.93 → 78.02 ms (36.0%)** with no untracked files and **441.95 → 229.63 ms (48.0%)** with eight. These percentages are not comparisons against 0.16.41. The initial cold fix had made warm queries slower (59.56 → 105.51 ms in its own window); the follow-up reduces that overhead without a cross-request executable cache. Final cold-session medians remained about 53–55 ms.

Peak live Rust allocation was effectively unchanged (15,290,610 → 15,289,101 bytes), and streaming latency/throughput did not consistently improve. Allocation churn is not RSS. Results are local workload measurements, not model latency or production-capacity guarantees. No SQLite writer-admission prototype is included.

Correctness checks covered 54 initial workload runs (3,150 fixture provider requests and 2,160 tool calls), plus the Git follow-up's 60 cold observations, 120 timed warm queries and 24 workspace-mutation checks. Production data and real provider keys were not used in these benchmarks.

Protocol **95**, configuration **29**, Bot state **9**, checkpoint **20** and SQLite schema **13** are unchanged. State already running 0.16.41 needs no conversion.
