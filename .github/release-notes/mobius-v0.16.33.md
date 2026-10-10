Voice is a standalone middleware with its own model selection, transcript, state and call lifecycle. Shared session resources and child configuration remove redundant ownership copies while preserving isolated child execution and main-agent-only capabilities.

Middleware hooks now own attachment admission, background-work detection and read-only tool policy. Compaction accounting persists before updating its visible counter, and hidden session visibility has one durable owner. Anthropic accepts empty streamed thinking blocks; DeepSeek advertises image input for its Flash model.

Computer control keeps desktop-only sessions free of browser startup, renews browser leases before evaluating retained page handles and cancels installation-lock waits on shutdown. Generated images share one validated publication path.

This release requires gateway protocol 93, config version 29, Bot state version 9 and checkpoint version 20. Back up existing state and convert it offline with all writers stopped before upgrading; SQLite user versions alone do not identify the logical format. Older clients cannot connect to protocol 93. Rust 1.99.0 or newer is required.
