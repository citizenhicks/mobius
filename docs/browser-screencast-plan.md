# Gateway-owned Chromium and noVNC pilot

Status: final dirty validation and release audit. Commit to main and release through
GitHub Actions after the local, Cloud test-account and physical iPhone lifecycle
tests, simplification review and required quality gates pass.

## Ownership and selection

Core computer control selects a gateway-coordinated headed browser only when the call
has Full access, network access, and the gateway supplies a browser. Otherwise it
uses the worker-owned headless browser. This is the single capability selection
point. Core advertises observations and browser availability; it does not own
processes, transport, desktop leases, or UI.

The gateway owns allocation and coordinates the selected browser. On macOS it
requests the existing in-app CEF tab over the scoped Unix DevTools broker; the Mac
app renders it. Human-open and agent attachment use the same chat tab. On Linux
the gateway launches Xvnc, Openbox and Playwright's pinned Chromium. Static native
desktop setup adds a logo-only Möbius wallpaper, a top status bar and a bottom
Chrome/Terminal/Files rail through tint2, xterm, pcmanfm and feh. Chrome starts as
a large resizable window with the desktop and rail visible; terminal and file
manager launch on demand. This is a native desktop, not a page inside Chrome.
Packages are installed once through the existing Sprite provisioning/repair
path; dirty rollout is limited to the test allocation before becoming standard
setup for new Sprites. Panels share the existing owned-child cleanup and consumer
lifetime. Clients show
the selected presentation through one Bot's computer action: the in-app tab, the
existing headless browser view when locally available, or the entire Xvnc desktop
through stock noVNC in
CEF/WKWebView. No separate native Chrome window or duplicate browser launchers.

## Browser and desktop lifetime

One persistent Chromium profile on a remote gateway; local CEF uses its existing
persistent app profile. The remote processes are lazy: opening a viewer or first
agent browser use starts them. A viewer owns a consumer until its stream closes.
An agent's sandbox retains its consumer across evaluations until the root becomes
idle; queued turns and pending source admission retain it. Each isolated child
sandbox owns an independent consumer through its final cleanup. Worker exchange
disconnects do not end that lifetime. The last consumer closes the processes after
active sandbox executions drain; takeover also protects the runtime. Reopening
starts it with the same profile. Ordinary chat connections do not launch a desktop.
Chrome is an optional desktop app: closing it leaves the shell and human control
available. Agent browser use reattaches to a rail-opened Chrome or lazily launches
the same pinned browser/profile. A changed CDP endpoint clears tab assignments and
requires a fresh worker observation.

One assigned tab per chat/execution and one foreground evaluation lease.
Every shared-browser evaluation acquires the gateway lease through
the existing framed worker host-service channel. The first evaluation after worker
startup or takeover returns a screenshot and accessibility snapshot without
executing submitted code. Independent interpreters do not mean independent logins.

For remote desktops the gateway owns child processes, process groups and pidfiles. It reaps them on
shutdown and validates stale ownership before cleaning up after a hard kill.
Failed startup reaps partial children and removes the RFB socket without deleting
the profile. Child stderr and intentional start/stop reasons use the existing
managed service log; failures reach the viewer. No
new supervisor or Cloud service. Xvnc combines X and VNC; no x11vnc. Xauthority
cookie, no X TCP listener, RFB Unix socket under private gateway state. CDP remains
loopback TCP for worker attachment.

A desktop gateway rejects workspace-restricted Bots with network, both when enabling
the desktop and during shared Bot admission. They could otherwise reach CDP and the
saved-login profile. Never silently rewrite Bot policies. A future remote-debugging
pipe and authenticated gateway CDP bridge may lift this restriction.

## Watch and control

Protocol 89 adds desktop availability, stream enable/ack, ordered base64 RFB chunks
(maximum 16 KiB raw), and request-correlated control state. Streaming uses a second
authenticated TLS or Noise WebSocket connection; plaintext TCP cannot serve noVNC.
Clients relay bytes through a token-protected loopback WebSocket to stock noVNC.
Queues are bounded and preserve order with backpressure.

Takeover closes all gateway execution admission, cancels and drains all sandbox
work, then acquires the same desktop lease before enabling Xvnc keyboard, pointer
and clipboard input. Release disables input before releasing the hold. The next
worker evaluation observes fresh state without replay. Competing controllers are
rejected. Disconnect/backgrounding releases control. Watchers use noVNC viewOnly.

Xvnc input starts disabled. AllowOverride explicitly includes AcceptPointerEvents,
AcceptKeyEvents and AcceptCutText. No per-connection RFB parser in the pilot; the
watchers are the same user's other devices. Add one only if that trust model changes.

## Login and backup

Chromium's profile on disk is the saved-login store. Use its normal settings to
forget credentials. No storageState export, seed, login store, or custom Forget UI.
Portable backups exclude the desktop subtree through the shared backup allowlist;
a regression test covers profile and Xauthority. Native Sprite checkpoints retain
the profile as ordinary filesystem data.

Passwords and verification codes work. Passkeys and Touch ID forwarding are not
supported in this pilot; show this before login. Explicit Mac Cmd+V/native Paste
and iOS Paste send text to noVNC and trigger remote paste. No automatic clipboard
sync or clipboard history.

## Client views

Use Hugeicons LaptopMinimalIcon beside Bot's computer in the three-dot chat menu
and on its Mac companion tab. Local Mac: the existing CEF browser controls and tab,
with allocation handled by the gateway. Both remote viewers use a black aspect-fit
canvas with the existing custom spinner centered, without loading text on the
canvas. The standard accent control button remains visible and disabled with
Starting up or Connecting, then becomes Take control when the viewer is ready.
Startup errors remain visible with a retry action. Native controls sit outside
the CEF/WKWebView.

Both titles show the existing gateway status dot or spinner, LaptopMinimalIcon,
and {bot}'s computer. A native glass pill at the upper right on Mac and in a
subheader below the native iPhone navigation title shows {bot} has control,
or accent-filled You have control for the controlling client. Return
control uses the standard accent button. Other-device ownership is explicit.

iOS uses native navigation and toolbar controls, including the leading X, with
native glass/accent buttons. It provides explicit Paste, Fit/Actual size, and
keyboard controls. Use noVNC's
textarea/IME path for the software keyboard. Ownership controls move above the
keyboard. Scaling and panning stay local; no remote desktop resize.
Paste from clipboard stages local text in a sheet with X on the left and Paste on
the right. Paste sends once; X cancels. Control and paste remain disabled until
noVNC reports connected. Loading and failures are visible; idle chats can open the
computer for login without first running an agent.

## Files and slices

1. Core sandbox host-service seam and computer-control routing; gateway runtime
   installer, owned browser/desktop module, shared policy admission, actor cleanup,
   persisted desktop_enabled, stopped-only set-desktop CLI; Sprite packages/repair.
2. Gateway wire/server relay and secure transport gate; Mac dedicated connection,
   loopback server, stock noVNC assets, LaptopMinimalIcon, companion tab/menu/paste.
3. Gateway all-execution hold and lease, Xvnc toggles, resume observation, profile
   backup regression; exercise competing devices and disconnect recovery.
4. iOS wire/client/view/loopback relay/assets/lifecycle; direct development build to
   sky (00008150-000005EC21D8401C), followed by coordinated TestFlight and review builds.

## Dirty validation

Test the disposable remote fixture first. The restored test-account Sprite is
available and carries the protocol-89 dirty pilot. Dirty testing stays limited
to wexylum@protonmail.com, with a rollback binary/config and dirty deployment
protected from reconciler replacement. Run core/gateway/Mac/iOS/Cloud required checks,
actual pinned Chromium worker tests, local in-app-browser interruptions, Cloud RFB
watch/control/paste/admission/cancel/release/restart checks, and both clients end to
end. Slim 4 GiB Sprite measured 435 MiB steady with Chromium, Xvnc and Openbox;
repeat memory/cleanup checks against the actual gateway integration.

## Coordinated release

Release Core, gateway and CLI 0.16.7 in dependency order, then Mac 0.3.12 against
the tested Core commit. Bump private Rust helper versions without publishing them.
Use main in every repository and preserve today's UI and persistent-chat commits.
Promote the Mac release, Sparkle feed and website download, and update Homebrew
formulae and the cask through their existing Actions workflows.

Cloud upgrades all accounts through the managed installer, using each installed
gateway's protocol for idle preparation. Preserve an original SQLite backup and
apply the explicit known Bot schema 5→6 change offline. Active gateways defer;
uncertain installation or rollback remains closed for retry. Keep the ordinary
Core storage guard strict. Standard provisioning installs the desktop packages and
reader protocol 89; remove the dirty account pin only when the released artifact
is installed and verified.

Publish iOS 0.10.0 to TestFlight and 1.0.0 for Apple review through Actions, choosing
fresh build numbers and preserving existing review metadata, relationships and
storefront availability. Verify the published artifacts and all Cloud accounts
before considering the release complete.
