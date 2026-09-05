Adds authenticated Realtime voice for eligible OpenAI API, ChatGPT Codex, and möbius Cloud configurations. Gateway protocol is now 68; update clients and gateways together.

- Voice signaling belongs to its authenticated connection and selected chat, with one call per chat. Disconnecting, deleting a chat, or changing providers closes the call.
- Spoken requests use the existing Bot, message delivery policy, tools, sandbox approvals, and committed history. Signaling and provider credentials never enter the transcript.
- Realtime usage shares the existing provider usage accounting.
- Eligibility is advertised in the shared provider and model catalogs. Custom compatible endpoints do not implicitly advertise Realtime.
