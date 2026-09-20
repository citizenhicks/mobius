Fixes OpenAI WebSocket streams failing when event processing falls behind. Response buffering retains memory and event limits while keeping connection reads responsive.

After bounded WebSocket retries, the session can fall back to HTTP. Incomplete HTTP streams are reported as interruptions, and failed fallback attempts close their model steps correctly.
