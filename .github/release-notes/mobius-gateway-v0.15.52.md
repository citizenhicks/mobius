# möbius Gateway 0.15.52

Computer control reports browser-viewing availability through a capability-owned frontend widget. Browser links are removed when the worker fails, times out, or is cancelled.

The browser endpoint listens on loopback and provides no remote browser access. It has no authentication and trusts processes running on the gateway host.

Moves shared gateway account persistence and endpoint selection into the gateway crate so desktop clients can use them without terminal frontend dependencies. Existing account paths, formats, permissions, and writes are preserved. Transcript blocks can expose capability-owned file links.

Pins mobius 0.15.48. Protocol 85, configuration 26, checkpoint payload 17, and SQLite schema 10 are unchanged. Packages retain LICENSE and NOTICE.
