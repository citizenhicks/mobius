# möbius 0.9.12

- Executes scratchpad edits and promotions during active turns when the shared store is available.
- Removes the retired conversational cron middleware and `schedule_task`; scheduling is owned by the gateway protocol.
- Requires Rust 1.98 or newer.

Gateway protocol 48, configuration 20, and chat specification 9 are supported by the companion Gateway release.
