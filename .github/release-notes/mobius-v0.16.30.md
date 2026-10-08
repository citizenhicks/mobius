Streaming now groups adjacent text deltas into bounded 40 ms batches, with the first delta delivered immediately and durable barriers before tools, completion, retries and cancellation.

- Image and live model routes assemble independently of text model catalogs and retain their credential lifetimes.
- Provider adapters share resolved credentials and immutable media transports without copying conversation history.
- Media identifiers are checked at the owning configuration boundary; incoming provider responses retain their wire and consistency checks.

Protocol 92, config version 28 and checkpoint storage formats are unchanged. Rust 1.99.0 or newer is required. No measured performance percentage is claimed.
