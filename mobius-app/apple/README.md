# möbius for iPhone and iPad

One SwiftUI client target builds for iOS and iPadOS 26+. Both device families use the same `AppModel`, `GatewayClient`, pairing flow, and versioned möbius gateway protocol. The marketing version and build number live in the Xcode project settings.

Open `MobiusApp.xcodeproj` and run the shared `MobiusApp` scheme on an iPhone or iPad destination. Command-line builds use:

```sh
xcodebuild -project MobiusApp.xcodeproj -scheme MobiusApp \
  -destination 'generic/platform=iOS Simulator' -skipMacroValidation \
  CODE_SIGNING_ALLOWED=NO build
```

Run the complete test suite on an installed simulator with signing enabled so
Keychain-backed account and persistence tests have their required entitlements:

```sh
xcodebuild -project MobiusApp.xcodeproj -scheme MobiusApp \
  -configuration Debug \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro,OS=26.5' \
  -disableAutomaticPackageResolution -skipMacroValidation \
  -parallel-testing-enabled NO test
```

To connect your own gateway, request a fresh code from an initialized gateway:

```sh
mobius-gateway init
# Later, while the gateway is running:
mobius-gateway connect
```

Choose **Use your own gateway** and paste the displayed setup code, or enter
the public `wss://` address and one-time code. Pairing still requires confirmation.
The same one-use code works through the advertised local `tcp://` endpoint.
Plaintext remote endpoints are rejected; a direct TLS listener remains available
as an advanced option in the gateway guide.

## möbius Cloud beta

The cloud offer uses StoreKit 2 to load both monthly products and render their
storefront-localized prices: `app.mobius.client.cloud.monthly.v2` (Cloud) and
`app.mobius.client.cloud.plus.monthly` (Cloud Plus). Both have the same features;
Cloud Plus includes 4× more usage. Sign in with Apple creates
the Cloud session. The app submits verified transaction and AppTransaction JWS
values and finishes a transaction only after backend acceptance.

Profile shows the verified current plan, billing dates, any scheduled downgrade,
and included usage. Apple’s subscription controls handle upgrades, downgrades,
and cancellation. Credit renews once per verified paid billing period; restore,
cancellation, and a pending plan change do not create another allowance. Changing
plans preserves the account, gateway, and chats. App Store Connect owns product
prices and availability; StoreKit supplies the localized offer shown in the app.

The one-time code is only the first pairing credential. A successful pairing
returns a per-pairing bearer token, which this app stores in device-only Keychain
storage and uses for later connections. Provider credentials are write-only and
are never persisted by this app.

Reconnecting to the same gateway preserves navigation, loaded catalogs, open
forms, and unsaved drafts. Cached chats remain readable while connecting; the
visible chat then resumes from its cached event cursor. Device sign-in resumes
the same gateway-owned attempt, including a result missed while the app was in
the background. Switching gateways clears the previous gateway's setup state.

## Quality checks

Run these commands from `mobius-app/apple`. The separate
`.github/workflows/swift.yml` workflow checks formatting and complexity, runs the
signed simulator tests above, and builds an unsigned device Release. It selects
Xcode 26.6 and iOS Simulator 26.5; missing toolchain/runtime versions fail the job
rather than silently selecting another destination. The app minimum remains iOS 26.
First-party app and test targets use Swift 6, complete strict concurrency, and
warnings-as-errors. Dependency versions remain in `Package.resolved`.

```sh
xcrun swift-format lint --configuration .swift-format --strict --recursive Sources Tests
swift-complexity Sources Tests --recursive --threshold 21 --report-suppressions

xcodebuild -project MobiusApp.xcodeproj -scheme MobiusApp \
  -configuration Release -destination 'generic/platform=iOS' \
  -disableAutomaticPackageResolution -skipMacroValidation \
  CODE_SIGNING_ALLOWED=NO build
```

CI downloads the checksum-pinned `swift-complexity` 1.4.0 CLI; its installation
command is in the workflow. The two documented `GatewayMessages.swift` codec
suppressions are the only complexity exceptions. Do not raise the threshold to
hide a new violation.

The native formatter is the sole layout authority. Apply it with:

```sh
xcrun swift-format format --configuration .swift-format --in-place --recursive Sources Tests
```

`.swift-format` uses four spaces and preserves existing line breaks. Its two rule
overrides preserve SwiftUI trailing closures and avoid rewriting `forEach` into
loops. Keep formatting-only commits separate from behavior changes.

## Developer map

Source paths below are relative to `Sources/MobiusApp/`.

| Owner | Responsibility |
| --- | --- |
| `App/AppModel.swift` | Composes the concrete owners; owns navigation, workspace/git presentation, new-chat intent, and cross-feature cleanup. |
| `Gateway/GatewayConnectionModel.swift` | Accounts, pairing, transport generations, reconnects, and shutdown. `GatewayClient.swift` handles framing; `GatewayStore.swift` owns persistence. |
| `Chat/ChatSessionModel.swift` | Session selection/replay/history, transcript, composer/recovery, session attachments, titles, and voice lifecycle. Related code lives in `Chat/` and `Composer/`; workspace-file presentation remains root-owned in `Files/`. |
| `Cloud/MobiusCloudModel.swift` | Authentication, purchases, provisioning, extension catalog, and auth-bound push registration/deduplication. `RemoteNotifications.swift` contains the Cloud-attached app delegate. |
| `Bots/`, `Routines/`, `Settings/`, `Configuration/`, `UI/` | Feature views/configuration and shared presentation primitives. |

Views use the actual owners through `model.gateway`, `model.chat`, and
`model.cloud`; do not add mirrored state or property-forwarding facades to AppModel.
Cross-feature operations still belong at the root: for example, `AppModel.connect`
applies Cloud entitlement policy before starting a gateway connection.

### Lifecycle and stale work

`App/MobiusApp.swift` owns aggregate app lifecycle and shared startup/activation.
`App/AppShell.swift` owns per-window chat-visibility tokens and privacy coverage.
A window losing chat visibility must not tear down the shared connection.

Reject late work at its owner: Gateway uses `connectionGeneration`; Chat uses
transcript/composer generations and request IDs; Cloud uses `operationGeneration`
and session identity. Startup also checks its account and gateway generation.
Keep those checks after suspension points and before applying state or persisting
credentials; cancellation alone does not establish ownership.

### Persistence and deletion

Chat keeps two existing serialized chains in `Chat/ChatSessionPersistence.swift`:
`transcriptIOTask` orders transcript/cache work, while `composerDraftIOTask` orders
draft and edit-recovery saves, loads, and removals. Both use the same concrete
GatewayStore. `chat.drainIO()` awaits both chains.

Gateway removal quiesces and resets selected-session state synchronously, drains Chat
I/O even for an inactive account, then deletes that account through GatewayStore.
Full reset invalidates Cloud work, blocks reconnects, quiesces Chat, invalidates
the gateway generation, resets root state, and captures shutdown **before any
await**. Only after draining/shutdown may it clear persistence. Preserve this
ordering so late work cannot reconnect or recreate deleted data.

### Tests and wire changes

All tests import the canonical app module with `@testable import Mobius`; do not
compile production sources into the test target again. Tests are grouped by
feature under `Tests/MobiusWireTests/`; mixed AppModel suites remain at its root.
Wire records and their existing tests live in the respective `Gateway/Wire/`
folders. Update the owning Swift record and its focused tests together when a
wire change is needed.

Reuse the model/record/request helpers in `AppModelTestSupport.swift` where their
dependencies fit the test. Its basic model helper does not isolate every service
or cache directory. Destructive fixtures must explicitly inject unique
UserDefaults suites, all four temporary GatewayStore directories (catalog,
transcript, thumbnail, draft), test-only account IDs, and stubbed external services.
Cloud credential tests also need a dedicated Keychain service. Never let reset or
removal tests use the default app store or real account data; clean up isolated
resources on every exit path.
