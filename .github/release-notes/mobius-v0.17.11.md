# mobius 0.17.11

- Apply multi-file patches through the sandbox using Codex matching, preserving line endings and reporting partial writes accurately.
- Share bounded JSON encoding and complete model-request admission, including images, across tool and transport boundaries.
- Split invalid-input and internal errors and expose typed context accounting.

Coordinated with gateway, CLI and macOS 0.17.11 on protocol **104**. Checkpoint SQLite is now **15**; raw checkpoint payloads remain **21**. Existing SQLite 14 journals require a qualified offline conversion before opening. The runtime rejects older databases and does not migrate them at startup.
