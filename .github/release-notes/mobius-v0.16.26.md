Model requests and compaction share a request-local cancellation reason. The agent records explicit interrupts, frontend disconnects and credential expiry before dropping provider futures. Unfinished Responses WebSocket requests close cleanly with a finite, non-sensitive reason; cleanup is bounded by the socket I/O timeout.

Background-command activity now counts only unfinished tasks. Completed commands retain their output for later polling without keeping an otherwise idle session active.

Protocol version remains 91.
