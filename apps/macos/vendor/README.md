# Vendored UI dependencies

GPUI Kit's `gpui-base`, `gpui-component` and `gpui-kit-assets` 0.9.1 are the
published Apache-2.0 crates. Only Cargo.toml changes: remove the unused gpui-pre
and gpui-pre-platform dependencies and route inspector/test-support to Fast.
Their Rust code and assets are unchanged. The application selects `gpui-fast`.
Keep licenses and notices when updating these crates.

## GPU-presented motion: proposed API exception

The two Fast crates are the published Apache-2.0 0.1.14 archives, with a manually
reviewed port of a2e2e1bd. New runtime state, algorithms and tests live under
`src/fast/motion.rs`; upstream files contain qualified hooks, index fields, and
individual exports. The published Metal shader stays unchanged; the existing
Fast build-time shader owner redirects to `fast/motion.metal`. That full shader
copy must be reconciled with upstream shader edits on every upgrade.

This is a private proposed exception to GPUI Fast's unchanged-public-API rule,
approved by the application owner, not approved by upstream maintainers:

- `Motion`, `MotionFrame`, `MotionHighlight`, `MotionExt`, `MotionElement` let the
  app supply its own curves while GPUI presents the retained scene. `Window`
  gains only `with_motion` and `refresh_motions`.
- `MotionTransform` and `Scene::motion_transforms` are the cross-crate Metal
  renderer ABI; Scene's motion state, scopes and replay bookkeeping stay private.
  Primitive `fast_motion` indices are renderer-facing ABI fields. Existing
  padding is reused where possible. `max_fps` validates once; subnormal positive
  values saturate to Duration::MAX, and nonpositive/nonfinite values are uncapped.
- The sole test-only export, `assert_motion_invariants`, is gated by test-support.
  It runs the same focused invariants as the Fast unit tests, without depending
  on fonts/dev macros omitted from the published package's complete unit suite.

macOS Metal only. Paths and glyphs apply rotation; axis-aligned primitives move
and scale their bounds. Surface/subpixel primitives are not animated. Layout,
hit testing and accessibility bounds remain at their laid-out positions: use
motion for presentation, not to relocate an interactive target permanently.
No application curves or timing policy belong in the renderer.

The port repairs enclosing-scope replay (including composition), final-frame
presentation, FPS overflow, initial-position culling/order, and path fade opacity.
Focused invariants compare retained/fresh paint and verify clamped completion.
All Fast-specific imports are explicit. Existing upstream style is retained.
The Apple header input mirrors only shader ABI structs, not runtime algorithms.

Upstream review reference: https://github.com/longbridge/gpui-fast/blob/main/docs/upstream-sync.md
Guide revision reviewed: 80be50164adaf4db69eaf4a25037f20b6f447835.
Published-archive hook checks do not replace `script/check-upstream` against
GPUI Fast's imported Zed history; that history is absent from crate archives.

## Image eviction: proposed API exception

The manually reviewed ae2c2ba3 memory slice uses one additional owner API,
`App::take_asset<A>`, implemented entirely in `fast/asset_cache.rs`. It removes
an existing cache entry and returns completed output without starting a load;
pending work is cancelled. This proposed extension is within the approved cache-fix scope, not
upstream approval. Existing public methods keep their contracts. It replaces
fetch-then-remove during image eviction: no speculative load and one cache-key
removal rather than loading/looking up an entry merely to discard it. The app
uses the returned image with the existing `drop_image`, including the current
window, before deleting the discarded thumbnail off-thread. A cache-boundary
test covers absent, completed and pending entries and counts loader calls.

Recent-cache retention follows the real tab owner: one chat plus companion tabs.
Keep open/document references and unsent drafts; keep the four recent chats.
The memory/dreaming overwrite policy is unchanged.
