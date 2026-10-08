Realtime voice shutdown now sends an explicit close and drains bounded final events when calls are cancelled, command senders disappear, or event listeners stop consuming. Closing still has a fixed deadline.

Tool descriptions clarify existing-file delivery, automatic image delivery, history targets, task-list replacement, command completion, child permissions and interruption verification. Stopped chats may have no final reply; their running status is the completion signal.

Protocol 92, config version 28 and checkpoint formats are unchanged. Rust 1.99.0 or newer is required.
