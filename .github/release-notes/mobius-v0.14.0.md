Adds a separate, durable voice conversation that can discuss the current Bot chat and delegate agreed tasks without putting casual speech into the Bot's working context.

- Messages owns the linked voice transcript, existing preview widget, task requests, and result delivery. Only explicit voice-agent requests enter the Bot through normal peer messages.
- Realtime adapters stream user and assistant transcripts, accept background Bot context, and advertise their supported voices. OpenAI uses `gpt-realtime-2.1-mini`; Codex retains its native voice model and voice catalog.
- Native Codex delegation resolves references against the private voice discussion in an isolated request with tools disabled, so only the complete agreed task enters the Bot's chat.
- Preview events preserve message identity, and peer messages can carry a semantic icon. Existing transcript rendering is reused by both frontends.
- Capability presentation callbacks cannot keep a stopped Agent's event stream alive.

This release changes public provider, protocol, and active-command records; update dependent crates together.
