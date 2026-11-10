# möbius assets

First-party artwork and marketing screenshots for möbius:

| Asset | Use |
| --- | --- |
| [Colour logo](MobiusMark.svg) | Full-colour SVG mark used in the main README |
| [Monochrome logo](MobiusMarkMonochrome.svg) | Single-colour SVG mark, from the app’s menu-bar artwork |
| [macOS screenshot](screenshots/macos-dark.png) | Dark appearance; generated from the `readme-hero` fixture without live user data |

The logo artwork is original möbius work. The colour mark uses the Nord palette;
see the repository [NOTICE](../NOTICE). App fonts, icons and other third-party artwork
remain in [apps/macos/assets](../apps/macos/assets/NOTICE) with their own notices.

To regenerate the macOS screenshot, run the snapshot harness described in the
[macOS README](../apps/macos/README.md#checks) with `MOBIUS_SNAPSHOT_ONLY=readme-hero`.
