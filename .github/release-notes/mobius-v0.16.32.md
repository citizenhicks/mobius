Anthropic tool schemas omit unsupported root combinators from its outgoing request while retaining nested schemas and tool argument validation. All Anthropic presets advertise low, medium, high, xhigh and max reasoning; Haiku 5.5 replaces Haiku 4.5 in the built-in catalog.

The native Mistral provider includes Large 4 and Small 4 presets with adjustable reasoning, streamed text and tool calls, image observations, and signed thinking replay. Its catalog and endpoint remain configurable; Large 4 is a public preview.

Responses transports, SSE handling, authorization and Chat Completions wire helpers have shared owners. Provider registrations retain their native capabilities and wire mappings; request serialization borrows history and image data.

Protocol 92, config version 28 and checkpoint formats are unchanged. Rust 1.99.0 or newer is required.
