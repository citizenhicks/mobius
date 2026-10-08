Validation runs at the owning boundary instead of repeatedly walking unchanged input through the agent and provider chain.

- Immutable validated submissions carry ingress checks into agent admission; rewritten source messages are checked again.
- Queued messages validate on construction and deserialization, avoiding repeated body serialization on checkpoint saves while retaining queue and execution consistency checks.
- Agent streaming owns tool-call duplicate, count and size limits for all model implementations. Providers retain their wire decoding and completed-output consistency checks.
- Tool hooks only trigger a second prepared-call check when they rewrite the call. Live authorization remains checked after asynchronous work.
- OpenAI HTTP and WebSocket streams reuse the validated output index. Internal realtime construction reuses transport settings validated at ingress.

Library callers of `VoiceConversation::handoff` now receive `ValidatedSubmission`; use its borrowed accessor or consume it with `into_submission` when raw access is required.

Protocol 92, configuration version 28 and checkpoint storage formats are unchanged from 0.16.28. Rust 1.99.0 or newer is required. These changes remove repeated work; no measured performance percentage is claimed.
