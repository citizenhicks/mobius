# Gateway menu bar app for macOS

The gateway's macOS frontend is a 400-point menu bar popover, with the same particle
waves and native WebRTC media implementation as the iOS composer. Requires macOS 26
and Xcode 26.6. It contains no agent runtime.
Icons compile from the iOS asset catalog; controls use its default Nord palette.

Hover over the glass waveform to reveal the chat selectors and call controls, in
both the menu bar popover and pinned mode. Keyboard navigation and VoiceOver also
reveal controls; approvals and errors remain visible until handled. Controls fade in place without moving or resizing the waveform. The pinned glass edge
pulses while connected and unmuted, independently of speech. Reduce Motion keeps a
steady edge instead.

Choose **Pin to corner** from the options menu, then one of the four corners.
The floating panel stays above ordinary windows across Spaces. Its glass
background is more transparent; the unpinned popover uses the system surface.
**Unpin voice window** returns controls to the menu bar without ending the call.
Pinning is explicit; reopening the app does not start a call or restore a pinned panel.

While pinned, choose **Mini mode** to show only a circular waveform. Right-click
the circle and choose **Exit Mini mode** to return. The circle uses the same
active glow, with more widely spaced waveform dots to keep silence subtle.
Approvals and errors temporarily expand the panel.
**Keyboard shortcuts…** lets you record or clear shortcuts for start/stop and
mute/unmute. They work from other apps and are saved on this Mac. Include Command
or Control; unavailable combinations leave the previous assignment intact.
Shortcuts are unassigned until you record them.

Click the möbius logo, choose a chat, and press start on the right. It becomes stop
during a call; the microphone on the left controls mute. Voice continues when the popover closes. One header row
contains three pills: Bot icon and name, the workspace's final folder name, and the
chat title or New chat. Each opens a native menu with an inline picker, matching the
iOS new-chat controls. The folder menu also has Add new folder. Choosing a Bot or
folder prepares a new chat; pressing start creates it and starts voice.
The chat menu groups the selected Bot’s chats by project folder, newest first within
each group. Selecting a chat also selects its folder. Switching chats ends
the old call before opening and connecting the new one.
Approval requests show the reason and complete tool
arguments, with approve-once and decline actions.

Quitting the menu ends its voice call and native desktop control and leaves gateway tasks running. Microphone
access is requested only when starting voice. The UI must run in a graphical login
session. SSH/headless gateways work without the companion; set
`MOBIUS_GATEWAY_NO_MENU_BAR=1` to suppress automatic launch explicitly.

## Native Mac control

Enable **Computer control** on the Bot; the gateway downloads its pinned runtime
on first enable. In the menu bar app, grant Accessibility and Screen Recording
access and turn on **Allow Mac control**. Native control also requires the Bot's
**Full access** sandbox policy. The switch resets when the app disconnects or the
desktop session becomes inactive. **Stop** immediately cancels pending local work.

Bots use the same persistent `computer_control` JavaScript tool for Playwright
browser actions and the `desktop` API for Mac apps. The latter provides app and
display discovery, accessibility inspection, press/set-value actions, screenshots,
mouse movement/clicks/dragging/scrolling, and keyboard input. Element IDs and
screenshot coordinates come from current observations. No separate desktop
server, token, plugin, or agent runs in the app. Native requests and replies use
its existing authenticated local gateway connection; remote clients cannot host
the desktop runtime. Linux and headless cloud hosts retain browser control only.

## Install and launch

```sh
brew tap citizenhicks/mobius
brew trust citizenhicks/mobius
brew install --cask mobius-app
open -a "Mobius Gateway"
```


Install the macOS **möbius-app.app** in Applications and open it. The app starts
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
open 'target/macos/möbius-app.app'
```

`build.sh` builds the Rust gateway and Swift release app, embeds WebRTC, and preserves
LICENSE, NOTICE, and the WebRTC license. Pass `MOBIUS_GATEWAY_BINARY` to package an
already built gateway. Optionally pass `MOBIUS_CLOUDFLARED_BINARY` with its sibling
`cloudflared-LICENSE` to bundle the tunnel dependency; otherwise install cloudflared
beside the gateway or on PATH. Release bundles include the pinned tunnel binary.

The default signature is ad hoc for local development. Set `MOBIUS_SIGNING_IDENTITY`
to a Developer ID Application identity for distribution signing. The release
workflow requires Developer ID signing and notarization before publishing a Mac app.
Configure `MACOS_CERTIFICATE_P12_BASE64`, `MACOS_CERTIFICATE_PASSWORD`,
`MACOS_SIGNING_IDENTITY`, `APP_STORE_CONNECT_KEY_ID`, `APP_STORE_CONNECT_ISSUER_ID`,
and `APP_STORE_CONNECT_PRIVATE_KEY` as repository secrets. The app shares the
Apple client's layered icon.

The Swift target links the existing Apple transport, JSON decoder, wave renderer,
voice session, palette, and design tokens through source symlinks. Both Apple
targets compile the same palette and token definitions.
The macOS WebRTC binary uses the AudioEngine device module so Bluetooth sample-rate
changes rebuild the audio graph. Its release URL and checksum are pinned directly
because the upstream Swift package manifest declares an unsupported tools version.
Desktop wire records project the session, voice, approval, and native-control fields this UI uses;
the build checks its protocol version against the Rust gateway. Swift formatting
uses the iOS `.swift-format`, and the complexity limit is 21, as in the iOS target.
