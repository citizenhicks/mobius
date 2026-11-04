# mobius 0.16.47

Stops queued tool calls as soon as an identical call and result reach the turn's repetition limit. Calls already running finish, polling tools remain exempt, and parent and child agents keep separate counters. Durable saves validate one immutable snapshot at ingress, share queued-message payloads, and retain the warm SQLite context across overlapping writes. Image preparation is shared across compatible model transports and avoids reading images that will be omitted from the request.

Custom `CheckpointStore` implementations now accept `ValidatedCheckpoint` in `save_with_events`; direct callers construct it with `ValidatedCheckpoint::new`. The wrapper exposes immutable checkpoint access. Protocol 97 and stored record formats are unchanged.
