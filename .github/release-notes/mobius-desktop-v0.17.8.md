# mobius-desktop 0.17.8

Use the framework's ring-backed HTTP stack for remote interface images. Remove the
unused GPUI TLS dependency that enabled a second crypto backend and caused the
embedded gateway to panic when opening secure model connections.

Memory settings live under Capabilities for both individual Bots and defaults,
using the same controls.

Bundle gateway **0.17.8**. Use protocol **102** with a matching local or remote gateway.
Protocol 101 gateways are incompatible.

Keep GPU-presented animations and recent-chat image caching. Held messages can be
edited from their context menu. Rust 1.99.1 builds retain the experimental
eight-worker compiler frontend.
