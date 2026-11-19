# mobius-cli 0.17.14

- Bundle gateway **0.17.14**: GPT Live 1.1 voice with a 15-second idle standby, and early rejection of unsupported image uploads.
- Strip symbols from release binaries: the CLI is about 16% smaller on macOS ARM64.
- Verify HTTPS certificates with the operating system's trust store. Linux hosts need a CA certificate package such as `ca-certificates`.

Uses protocol **104**. The CLI also works with 0.17.11 and 0.17.12 gateways.
