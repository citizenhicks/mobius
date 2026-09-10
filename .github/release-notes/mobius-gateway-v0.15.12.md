# möbius Gateway 0.15.12

Enabling Computer control downloads a pinned Node and Playwright runtime automatically. Installation verifies the Node checksum, uses the package lock, checks a browser launch, and publishes the completed runtime atomically. Runtime resources remain outside private gateway state.

The Mac menu bar app is now **möbius-app** (`brew install --cask citizenhicks/mobius/mobius-app`). It adds native Mac app inspection, screenshots, pointer and keyboard actions through the existing authenticated gateway connection. Mac control requires the app's Allow Mac control switch, Accessibility and Screen Recording permissions, and the Bot's Full access policy. Stop, lock, disconnect, cancellation, or lost permissions ends control. Headless Linux gateways provide browser control.

Protocol 75 requires matching clients. Checkpoint format 14 is unchanged; upgrading from 0.15.11 requires no conversation conversion. Packages preserve LICENSE and NOTICE.

Validation: Rust workspace checks, fresh runtime download and sandboxed Chromium execution, persistent JavaScript worker tests, and Mac transport, cancellation, observation and Unicode input tests. Native UI interaction still requires an unlocked Mac and granted system permissions.
