Providers can be configured for text, images and live voice independently, including a live-only OpenAI configuration. Native OpenAI WebSocket and Codex text model catalogs remain locked.

- Optional image and voice catalogs override built-in presets; empty catalogs disable the corresponding capability. A provider with no text selection does not advertise text models.
- Streaming batches reduce journal writes and transmitted frames while preserving ordered durable barriers.
- Framing writes the length and payload together and TCP connections enable NODELAY.
- Usage publication moves only changed counters and expired buckets, retaining exact rollback on failed publication. Whole-config persistence is unchanged.
- Bot event dispatch skips unchanged catalog cursors and avoids repeatedly scanning ignored records.

Protocol 92, config version 28 and checkpoint formats are unchanged from 0.16.28. No new state conversion is required from 0.16.28 or 0.16.29. Binary archives include the pinned cloudflared runtime, manuals and licenses.
