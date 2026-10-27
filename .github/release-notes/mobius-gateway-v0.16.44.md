# mobius-gateway 0.16.44

Centralizes protocol 97 transcripts and client capabilities, extracts the möbius runtime behind one harness interface, and adds project-scoped and global memory management. Only the möbius harness is available; Codex and Claude adapters are not included.

Protocol 95/96 clients are incompatible and must be updated together with the gateway. Bot catalog 10, checkpoint 21 and SQLite schema 14 are strict. Existing Bot 9 configurations must be explicitly converted to tagged möbius configurations, scratchpad settings/records to memory, and checkpoint 20/schema 13 records to the new format before a gateway data cutover. No fallback or automatic conversion/reset is performed. Back up all state and retain the previous binary before manual conversion.
