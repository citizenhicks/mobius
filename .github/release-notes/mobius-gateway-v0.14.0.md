Separates realtime voice discussion from the Bot's working chat while keeping the voice agent informed of the current conversation and ongoing work. Gateway protocol is now 69; update clients and gateways together.

- Voice transcripts persist in a hidden linked session and open through the existing read-only preview flow. Ending a call finalizes text already received, and reopening the chat restores its voice pill.
- Agreed task requests appear as ordinary peer messages from `voice agent`. The selected Bot still owns its tools, approvals, and work; voice explains the results.
- Native voice task extraction runs asynchronously in request order while speech, transcripts, and stop controls remain responsive.
- Bot settings can select a provider-supported voice. OpenAI API and möbius Cloud use `gpt-realtime-2.1-mini`; Codex uses its existing native endpoint.
- Existing gateway credentials, workspaces, sessions, and usage accounting are preserved.
