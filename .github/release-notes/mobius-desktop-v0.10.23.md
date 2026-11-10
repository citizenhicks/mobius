# möbius for macOS 0.10.23

- Bundles gateway 0.16.42; protocol 95 and persisted-state formats are unchanged.
- Reduces repeated transcript work by indexing turn entries and affected background commands, and reserving projection capacity from the existing item counts.
- In isolated local M4 Max release benchmarks, processing 4,000 simple turns fell from **47.423 to 2.165 ms (21.9×)**, tool-heavy history from **440.596 to 35.807 ms (12.3×)**, and prepend processing from **48.129 to 3.399 ms (14.2×)**. These measure transcript reduction/projection, not window rendering or scroll latency.
- Streamed projection allocation fell **337,294,728 to 222,464,990 bytes (34.0%)**; peak live allocation increased **3.4%**. A 2,975-snapshot comparison retained identical transcript output.
- The bundled gateway's initial Git/session fixture fell **1,154.70 to 52.68 ms (95.4%)** and its streaming allocation churn fell **14.46%**. See the gateway release notes for workload details and the separate warm-Git baseline.

Measurements are deterministic local fixtures, not production latency or memory guarantees.
