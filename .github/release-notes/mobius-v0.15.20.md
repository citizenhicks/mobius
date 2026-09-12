# mobius 0.15.20

Fast model streams now wait for durable event recording instead of aborting when a burst fills the recorder queue. Provider event callbacks are asynchronous and must be awaited; Responses HTTP, WebSocket, Anthropic, and Kimi transports use the same backpressure contract.

Event order, bounded recording memory, persistence before delivery, and cancellation remain enforced. Regression tests cover 2,048-event model and WebSocket bursts. No checkpoint or wire format changes.
