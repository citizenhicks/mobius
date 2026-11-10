# möbius for macOS 0.3.10

- Cloud connections follow the server's startup retry interval, so reconnects can resume as soon as the gateway is ready.
- A missing or invalid startup retry interval now shows an error instead of retrying with a fallback delay.
- Includes Core and gateway 0.16.4.

Requires Apple Silicon and macOS 26 or later. Gateway protocol remains 88.
