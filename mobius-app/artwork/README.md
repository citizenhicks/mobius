# Mark

The möbius mark is the triangular ribbon from `ribbon.svg`, the original potrace
silhouette, with a Bot in front of its lower corner. The ribbon's two pieces take two
shades: the body uses the accent, and the folded face uses the accent mixed 35% toward
white, the same rule as `AccentTint.artworkTint`. The Bot is a snow ball with night
`o o` eyes, matching the animated Bot faces in the app. A transparent gap separates the
Bot from the ribbon, so every output works on light and dark backgrounds.

`export_mark.py` generates every output from that geometry. From this directory:

```sh
uv run --with shapely python test_export_mark.py
uv run --with shapely python export_mark.py
```

It writes:

- `AppIcon.icon` and the six `AppIcon-<accent>.icon` alternates: three flat SVG layers
  (`Bot`, `Fold`, `Ribbon`), each in its own group so Icon Composer can apply depth,
  shadows, and the tinted and clear appearances. The exporter keeps each icon's
  background fill in `icon.json`.
- `MobiusLogoRibbon`, `MobiusLogoFold`, `MobiusLogoBot`, and `MobiusLogoEyes`: template
  vectors that `MobiusLogo.swift` tints from the palette, so the in-app logo follows the
  selected accent and theme.
- `MobiusMenuBar.svg`: the 18-point two-tone ribbon for the Mac menu bar. The Bot is too
  small to read at that size.
- `MobiusMark.svg`: the full-colour reference mark used by the repository README.

Xcode includes the alternates through `ASSETCATALOG_COMPILER_ALTERNATE_APPICON_NAMES`,
and `crates/mobius-gateway/macos/build.sh` compiles `AppIcon.icon` and the asset catalog
into the Mac menu bar app.
