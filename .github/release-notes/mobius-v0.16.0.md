Core message provenance now uses one Source contract for session, subagent and gateway-originated reports. Durable admission receipts prevent duplicate delivery after retries, and initiating authors survive execution history, checkpoints and compaction. The existing checkpoint protocol gains ordered catch-up for gateway lifecycle projection.

This release requires checkpoint SQLite schema11/payload18 and coordinated gateway/client upgrades.
