# möbius for iPhone and iPad

One SwiftUI client target builds for iOS and iPadOS 26+. Both device families use the same `AppModel`, `GatewayClient`, pairing flow, and versioned möbius gateway protocol. The next TestFlight release is version 0.9.3.

Open `MobiusApp.xcodeproj` and run the shared `MobiusApp` scheme on an iPhone or iPad destination. Command-line builds use:

```sh
xcodebuild -project MobiusApp.xcodeproj -scheme MobiusApp \
  -destination 'generic/platform=iOS Simulator' -skipMacroValidation \
  CODE_SIGNING_ALLOWED=NO build
```

On an iPhone or iPad, select **Quick Connect** during first-time setup, or request
a fresh code from an initialized gateway:

```sh
mobius-gateway init
# Later, while the gateway is running:
mobius-gateway connect
```

Choose **Pair self-hosted gateway** and paste the displayed setup code, or scan its QR with
the iPhone/iPad Camera. The QR opens möbius with the public `wss://` address and
one-time code prefilled; pairing still requires confirmation. The same one-use
code works through the advertised local `tcp://` endpoint. Plaintext remote
endpoints are rejected; a direct TLS listener remains available as an advanced
option in the gateway guide.

## möbius Cloud beta

The cloud offer uses StoreKit 2 to request `app.mobius.client.cloud.monthly.v2`
and render its storefront-localized `displayPrice`. Configure that product in App
Store Connect as a one-month auto-renewable subscription with a seven-day free
introductory offer before distributing through TestFlight. Sign in with Apple
creates the Cloud session; the app sends the locally verified transaction and
AppTransaction JWS values to the Cloud backend and finishes the transaction only
after backend acceptance.
The backend account remains the subscription authority before the app waits for
the hosted gateway and pairs with its one-time grant.

The one-time code is only the first pairing credential. A successful pairing
returns a per-pairing bearer token, which this app stores in device-only Keychain
storage and uses for later connections. Provider credentials are write-only and
are never stored by this app.
