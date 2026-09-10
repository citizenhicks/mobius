# möbius 0.15.11

Voice now carries the Bot's name, identity, and instructions into the call and speaks as the same Bot across voice and workspace work. Voice handoffs still use normal execution and approval handling, and completion is announced only after the committed result arrives.

The voice prompt preserves the complete persona and call policy, trims only historical context, and rejects instructions that exceed its size limit. Library callers now provide Bot instructions to `voice::instructions` (which returns a `Result`) and a Bot name to `VoiceConversation::new`.

Validation: workspace formatting, Clippy, documentation, unit/integration tests, and documentation examples; voice prompt bounds and handoff behavior are covered by tests.
