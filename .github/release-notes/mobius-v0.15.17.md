# möbius 0.15.17

API voice now uses GPT-Live 1 with native client delegation, streamed captions, and final transcript delivery on hangup. Codex voice retains GPT-Live 1 Codex. Requested work reaches the existing Bot directly with recent voice context, removing the extra task-extraction model call while preserving the separate voice transcript.

Checkpoint recording avoids redundant context copies. Regression tests count blob verification, tool-definition construction, middleware setup, and model calls to prevent repeated work from returning.
