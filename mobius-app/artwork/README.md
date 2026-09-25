# Mark

The möbius mark is the triangular ribbon from `ribbon.svg`, the original potrace
silhouette, centred in the icon. Its two pieces take two shades: the body uses the
accent, and the folded face uses the accent mixed 35% toward white, the same rule as
`AccentTint.artworkTint`. The mark has no background of its own, so every output works
on light and dark backgrounds.

`export_mark.py` generates every output from that geometry. From this directory:

```sh
uv run --with shapely python test_export_mark.py
uv run --with shapely python export_mark.py
```

It writes:

- `AppIcon.icon` and the six `AppIcon-<accent>.icon` alternates: two flat SVG layers
  (`Fold`, `Ribbon`), each in its own group so Icon Composer can apply depth,
  shadows, and the tinted and clear appearances. The exporter keeps each icon's
  background fill in `icon.json`.
- `MobiusLogoRibbon` and `MobiusLogoFold`: template vectors that `MobiusLogo.swift` tints from the palette, so the in-app logo follows the
  selected accent and theme.
- `MobiusMenuBar.svg`: the 18-point two-tone ribbon for the Mac menu bar.
- `MobiusMark.svg`: the full-colour mark, used by the repository README.
- `MobiusIcon.svg`: the mark on a rounded night background for browser tabs.

The public website (`thinkingsand/mobius`) and cloud console (`mobius-cloud`) vendor
`MobiusMark.svg` as `public/mobius-mark.svg` and `MobiusIcon.svg` as `app/icon.svg`.
Copy these generated files when updating the web branding; keep the geometry here.

Xcode includes the alternates through `ASSETCATALOG_COMPILER_ALTERNATE_APPICON_NAMES`,
and `crates/mobius-gateway/macos/build.sh` compiles `AppIcon.icon` and the asset catalog
into the Mac menu bar app.
