# mobius-gateway 0.17.14

- Reject image uploads in unsupported formats before accepting their bytes. The rejection names the supplied format and the formats the gateway accepts, and the connection stays usable.
- Serve GPT Live 1.1 for OpenAI Live voice calls, with speech wake and a 15-second idle standby.
- Strip symbols from release binaries: the gateway is about 17% smaller on macOS ARM64.
- Verify HTTPS certificates with the operating system's trust store. Linux hosts need a CA certificate package such as `ca-certificates`.

Uses protocol **104**. Gateways and clients running 0.17.11 through 0.17.14 can connect in either direction. An upgrade requires no database conversion.
