# Vendored UI dependencies

GPUI Kit 0.9.1 comes from the registry with its `gpui-fast` feature. It still
unconditionally declares `gpui-pre` 0.3.12, although its Fast-selected Rust sources
never use that backend. The empty `gpui-pre` package satisfies only this dependency
metadata; it contains no renderer, API shim or platform implementation. Its empty
inspector/test-support features match Kit's declarations. The app enables Fast's
test support directly. Remove the placeholder when Kit makes the pre dependency
optional. Keep Kit's published sources and assets out of this vendor directory.

## GPU-presented motion: proposed API exception

The two Fast crates are the published Apache-2.0 0.1.14 archives, with a manually
reviewed port of a2e2e1bd. New runtime state, algorithms and tests live under
`src/fast/motion.rs`; upstream files contain qualified hooks, index fields, and
individual exports. The published Metal shader stays unchanged; the existing
Fast build-time shader owner redirects to `fast/motion.metal`. That full shader
copy must be reconciled with upstream shader edits on every upgrade. A build-time
FNV-1a fingerprint of the published shader fails on drift; it is an upgrade
tripwire, not an authenticity check. The archive hashes remain in `provenance.json`.

This is a private proposed exception to GPUI Fast's unchanged-public-API rule,
approved by the application owner, not approved by upstream maintainers:

- `Motion`, `MotionFrame`, `MotionHighlight`, `MotionExt`, `MotionElement` let the
  app supply its own curves while GPUI presents the retained scene. `Window`
  gains only `with_motion` and `refresh_motions`. `Motion::opacity` guarantees fixed
  geometry so fades retain ordinary clipping/batching; it is the only specialized
  constructor, not an unchecked bounds hint.
- `MotionTransform` and `Scene::motion_transforms` are the cross-crate Metal
  renderer ABI; Scene's motion state, scopes and replay bookkeeping stay private.
  Primitive `fast_motion` indices are renderer-facing ABI fields. Existing
  padding (`Shadow`, `Underline`, `MonochromeSprite`) keeps its upstream name and
  layout; Fast interprets it as a motion index. `max_fps` validates once; subnormal positive
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

## Retained motion and scroll layers

Each attachment has its own scope identity even when two `Motion` values share a
sampler. Replayed motion keeps its original view owner for layer eligibility.
A scroll container that paints motion bypasses its cached layer in that frame:
painted motion is remapped into the frame, and carried static rows keep their
order. Replay renews the animation note, so promotion stays off while motion
content remains. Static scroll content still promotes normally.

`Scene::insert_primitive` deliberately splits stamping from the private unstamped
`Scene::insert`: copied primitives receive the current scene's scope, while replay
and rounded-clip scenes explicitly preserve/remap indices with their own tables.
This body split is an additional proposed upstream-file exception, beyond the
qualified hooks. `Window::rendered_scene` lives in Fast's stats owner and is
available only with test-support or bench-support; shipping API is unchanged.

The ordinary app tests execute the motion invariants and virtual scroll/list
windows, checking fresh, scrolled, and retained scenes against a different sibling
motion. Every tested primitive must index its own table and preserve its 7px
translation rather than the sibling's 50px motion. Static content must first
promote to a layer, so the test cannot pass by disabling scroll layers globally.

Content masks and rounded clips remain at laid-out positions; motion inside a
moving clipping ancestor can appear as a wipe. Highlights use window coordinates,
so a horizontal translation does not carry the highlight band with the text.


## Active motion and shared sampling

Only motion stamped onto a drawable primitive drives frames; its enclosing scopes
are marked with it. Empty and fully clipped scopes neither sample nor block static
scroll layers. Rounded-clip compilation shares the immutable entry table. The
Fast instance serializer accepts an optional existing motion binding: rounded-clip
children use their parent's already sampled/uploaded table, while root frames and
static tiles create their own. The upstream Metal function keeps its signature and
delegates through a qualified hook; tile/frame serialization has one Fast owner.

The macOS backend's schedule_frame and frame_waker call the same idempotent
MetalFrames::wake; motion uses schedule_frame once. max_fps caps presents, not
120Hz display-link wakeups. Moving bounds cover the content mask to preserve entering
content and draw order. Opacity-only scopes keep ordinary bounds unless an ancestor
can move them: a virtual 12-row quad/path fixture uses 2 batches at rest, 24 under a
generic motion, and 2 under Motion::opacity. Moving/reordering mixed content retains
the conservative batching cost.
CPU/GPU/memory improvements require representative hardware measurements.
