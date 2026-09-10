# Gateway voice menu for macOS

The gateway's macOS frontend is a 400-point menu bar popover, with the same particle
waves and native WebRTC media implementation as the iOS composer. Requires macOS 26
and Xcode 26.6. It contains no agent runtime.
Icons compile from the iOS asset catalog; surfaces and muted ink use its default Nord palette.

Click the möbius logo, choose a chat, and press start on the right. It becomes stop
during a call; the microphone on the left controls mute. Voice continues when the popover closes. One header row
contains three pills: Bot icon and name, the workspace's final folder name, and the
chat title or New chat. Each opens a native menu with an inline picker, matching the
iOS new-chat controls. The folder menu also has Add new folder. Choosing a Bot or
folder prepares a new chat; pressing start creates it and starts voice.
The chat menu lists existing chats for that Bot and folder. Switching chats ends
the old call before opening and connecting the new one.
Approval requests show the reason and complete tool
arguments, with approve-once and decline actions.

Quitting the menu ends its voice call and leaves gateway tasks running. Microphone
access is requested only when starting voice. The UI must run in a graphical login
session. SSH/headless gateways work without the companion; set
`MOBIUS_GATEWAY_NO_MENU_BAR=1` to suppress automatic launch explicitly.

## Install and launch

Install the macOS **Mobius Gateway.app** in Applications and open it. The app starts
the configured local gateway if necessary and uses the existing local pairing.
Initialize an installation with `mobius-gateway init` and configure a provider if
you have not already done so. This first version uses the local TCP listener
(including gateways published through Cloudflare); direct TLS listeners are not
supported by the desktop handoff.

Cargo users still install the executable with:

```sh
cargo install mobius-cli --locked
```

The `mobius-gateway` crate is a library; Cargo cannot install the Swift app bundle.
Install the companion app separately. Starting a gateway then opens the installed
companion, or open it explicitly with `mobius-gateway menu-bar`. The command passes
the gateway executable and state directory, never a bearer token. The app obtains
the existing local credential over a private subprocess output pipe. It keeps no
separate credential store and does not rotate or replace existing credentials.

## Build and verify

From the repository root:

```sh
swift test --package-path crates/mobius-gateway/macos -Xswiftc -warnings-as-errors
crates/mobius-gateway/macos/build.sh
open 'target/macos/Mobius Gateway.app'
```

`build.sh` builds the Rust gateway and Swift release app, embeds WebRTC, and preserves
LICENSE, NOTICE, and the WebRTC license. Pass `MOBIUS_GATEWAY_BINARY` to package an
already built gateway. Optionally pass `MOBIUS_CLOUDFLARED_BINARY` with its sibling
`cloudflared-LICENSE` to bundle the tunnel dependency; otherwise install cloudflared
beside the gateway or on PATH. Release bundles include the pinned tunnel binary.

The default signature is ad hoc for local development. Set `MOBIUS_SIGNING_IDENTITY`
to a Developer ID Application identity for distribution signing. The release
workflow notarizes and staples distributable packages when signing credentials
are configured; it labels other builds as unnotarized.

The Swift target links the existing Apple transport, JSON decoder, wave renderer,
voice session, palette, and design tokens through source symlinks. Both Apple
targets compile the same palette and token definitions.
The macOS WebRTC binary uses the AudioEngine device module so Bluetooth sample-rate
changes rebuild the audio graph. Its release URL and checksum are pinned directly
because the upstream Swift package manifest declares an unsupported tools version.
Desktop wire records project only the session/voice/approval fields this UI uses;
the build checks its protocol version against the Rust gateway. Swift formatting
uses the iOS `.swift-format`, and the complexity limit is 21, as in the iOS target.
