# Contributing to möbius for macOS

The repository's [AGENTS.md](../../AGENTS.md) applies here in full: the design rules,
the Rust style, the review contract and the commit evidence checklist. This file adds
only what is specific to the app.

## Ownership

| Path | Owns |
| --- | --- |
| `src/ui/` | GPUI presentation: pages, chat, transcript, panels, navigation, primitives |
| `src/host/` | Native integration: the embedded browser, the agent's browser broker, platform bridges |
| `crates/mobius-client/` | Gateway connection, wire state, uploads and downloads, preferences |
| `browser-helper/` | Chromium's helper processes |
| `platform/` | The Swift package: native bridge, voice, dictation, updates, Cloud sign-in, desktop control |
| `scripts/` | Bundling, release, native checks and the app's rules |

The gateway owns protocol and capability behavior; this client renders gateway records
and never re-implements gateway rules. Platform views render presentation types rather
than branching on middleware names.

## App-specific rules

- The app is its own Cargo workspace (CEF and the Swift build are macOS-only) and uses
  the framework and gateway by path from this repository.
- `unsafe` is allowed only for AppKit, CEF and Objective-C FFI, and every block carries
  a `SAFETY:` comment (`undocumented_unsafe_blocks` is denied).
- The möbius Cloud sign-in lives here, in `platform/NativeCloud*.swift` and
  `src/ui/pages/cloud.rs`. It is the one place hosted-service code is allowed; the
  framework, gateway and CLI stay cloud-agnostic.
- `platform/OnDeviceWriters.swift` is the source of a byte-identical copy in the iOS
  app; `scripts/check-native.py` fails when the two differ.
- `platform/desktop/GatewayWire.swift` carries the gateway protocol version; run
  `python3 .github/sync-gateway-contract.py --swift apps/macos/platform/desktop/GatewayWire.swift`
  from the repository root after a protocol change.
- UI uses the centrally owned tokens and components in `src/ui/primitives` and the
  theme for every color, size, radius, spacing, shadow and motion. macOS buttons
  preferably show an icon with text. Treat decorative tags, italic UI words, extra
  dividers and bordered containers, several shades of one color, and section
  numbering as defects.
- Form and application errors use the shared toast. Inline errors belong only to
  transcript records retained in chat history.
- Visual changes attach before and after captures from the snapshot harness
  (`MOBIUS_SNAPSHOT=<dir> cargo run --features snapshot`).

## Checks

Run the commands in [README.md](README.md#checks) from `apps/macos`. Releases are tagged
`mobius-desktop-v<version>` and built by `.github/workflows/desktop-release.yml`;
promotion to the update feed is the separate `desktop-promote.yml` workflow. Neither
runs without explicit authorization.
