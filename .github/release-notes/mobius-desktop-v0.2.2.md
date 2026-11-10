# möbius for macOS 0.2.2

- Fixes "Not connected" on Macs whose gateway uses a Cloudflare tunnel: the app now
  bundles the pinned cloudflared (2026.7.3) the gateway starts, instead of looking for
  one on PATH.
- A proper installer: download the disk image, then drag möbius into Applications.
  The disk image is signed and notarized.

Requires Apple Silicon and macOS 26 or later. Works with the same gateway (0.15.53,
protocol 86) as 0.2.0 and 0.2.1.
