# möbius Gateway 0.15.17

Provider, credential, extension, and Bot metadata changes avoid unnecessary resident-chat rebuilds. Shared Bot preparation is invalidated only when its runtime inputs change and binds on next use. Session activity updates reuse the catalog snapshot, hidden Bot listings query only that Bot, and replay avoids repeated checkpoint loads.

Voice delegates requested work directly to the existing Bot, preserving the voice transcript and removing task-extraction inference. API voice uses GPT-Live 1; Codex voice retains GPT-Live 1 Codex. Operation-count regressions cover these execution paths.

Includes the signed and notarized Mac companion. Protocol 75 and checkpoint format 14 are unchanged; existing conversations need no conversion. Packages preserve LICENSE and NOTICE.
