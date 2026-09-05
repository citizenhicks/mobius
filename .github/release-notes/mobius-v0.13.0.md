Realtime voice transports for OpenAI API and ChatGPT Codex, with provider-neutral capability discovery, authenticated WebRTC negotiation, and server-side conversation control.

- Messages owns voice handoffs through normal submissions and committed Agent results. Tools, approvals, delivery policy, and durable history retain their existing owners.
- Provider adapters normalize finalized speech, handoffs, errors, cancellation, and reported usage. Credentials and private provider events stay behind the model boundary.
- Voice setup, control channels, and session lifetime are bounded. Interrupted speech does not cancel Agent work.
