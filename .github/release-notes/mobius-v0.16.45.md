# mobius 0.16.45

Adds idle compaction for large chats, enabled by default after four idle minutes when the last prepared context has at least 100,000 input tokens. The capability owns its settings and runs once per idle period; accepted steering is answered normally, rejected steering cannot trigger a reply, and failed compaction preserves the existing context. Idle maintenance does not trigger normal turn hooks or replace the previous answer preview.
