# Orb artwork

`ComposingOrb.blend` preserves the supplied animation and bead material, with the
Nord accent light inside (700 W, 0.65 m radius) and the original white area light
above. The 24 fps loop uses frames 1–149; frame 150 repeats frame 1.

The Apple app plays a 512 × 512 transparent HEVC loop from
`../apple/Sources/MobiusApp/Composer/ComposingOrb.mov`. Render RGBA PNGs with Cycles,
32 samples and denoising, then encode HEVC with alpha. `ComposingOrb.swift` owns
playback, pauses outside the active scene, and uses the static logo for Reduce
Motion.

Rebuild the movie on macOS with Blender and Xcode installed. From this directory:

```sh
mkdir -p /tmp/mobius-orb-frames
blender --background --disable-autoexec ComposingOrb.blend \
  --render-output /tmp/mobius-orb-frames/frame-### --render-anim
xcrun swiftc -parse-as-library -swift-version 6 -warnings-as-errors \
  EncodeOrb.swift -o /tmp/EncodeOrb
/tmp/EncodeOrb /tmp/mobius-orb-frames /tmp/ComposingOrb.mov
```

The encoder requires a new output path. Inspect the loop before replacing the
bundled movie.

`MobiusLogo.imageset` contains frame 1 at 1024 × 1024. `AppIcon.icon` uses four
separate transparent renders of whole beads, grouped by camera depth, over the
Nord background, with subtle native shadows. The frontmost 96 beads form their
own foreground layer. Preserve the complete hidden portions of each group so Apple's
layered icon effects can reveal them. The default/dark icon and logo use the same
camera and lighting as the animation.

Clear and tinted icons use the `-Mono.png` variants. These add two icon-only Nord
area lights (30 W each, 2 × 6 m rectangles at X ±5, Y −0.5, Z 0, aimed at the
origin) so the side beads remain distinct from dark clear backgrounds. Keep these
variants under the native `tinted` image specialization; the base images remain
unchanged.
