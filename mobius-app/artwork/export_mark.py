"""Export the flat möbius mark: Icon Composer layers, accent alternates, in-app templates, menu bar.

Run from this directory: uv run --with shapely python export_mark.py
"""

import json
import re
from pathlib import Path

from shapely import affinity
from shapely.geometry import Polygon

HERE = Path(__file__).parent
APP = HERE / "../apple/Sources/MobiusApp"
MENU_BAR = APP / "Assets.xcassets/MobiusMenuBar.imageset/MobiusMenuBar.svg"
RIBBON_SOURCE = HERE / "ribbon.svg"  # the original potrace silhouette

NIGHT = "#2E3440"
TINTS = {
    "blue": "#5E81AC", "teal": "#8FBCBB", "green": "#A3BE8C", "yellow": "#EBCB8B",
    "orange": "#D08770", "red": "#BF616A", "purple": "#B48EAD",
}
DEFAULT_TINT = "blue"

# Artboard units: the icon square is 100 × 100, and the ribbon's longer side spans RIBBON_SPAN.
RIBBON_SPAN = 70


def fold_color(hex_color: str) -> str:
    """AccentTint.artworkTint: the accent mixed 35% toward white in device RGB."""
    channels = (int(hex_color[i:i + 2], 16) for i in (1, 3, 5))
    return "#" + "".join(f"{round(c + (255 - c) * 0.35):02X}" for c in channels)


def potrace_polygons(d: str) -> list[Polygon]:
    """Flatten a potrace path (M, relative c/l with implicit repeats, z) into polygons."""
    tokens = re.findall(r"[A-Za-z]|-?\d+(?:\.\d+)?", d)
    polygons, points, command, i = [], [], None, 0
    x = y = 0.0
    while i < len(tokens):
        token = tokens[i]
        if token.isalpha():
            if token not in "MclzZ":
                raise ValueError(f"Unsupported path command: {token}")
            command = token
            i += 1
            if token in "zZ":
                polygons.append(Polygon(points))
                points = []
            continue
        if command == "M":
            x, y = float(tokens[i]), float(tokens[i + 1])
            points = [(x, y)]
            i += 2
        elif command == "l":
            x, y = x + float(tokens[i]), y + float(tokens[i + 1])
            points.append((x, y))
            i += 2
        elif command == "c":
            c1x, c1y, c2x, c2y, ex, ey = (float(t) for t in tokens[i:i + 6])
            p0, p1, p2, p3 = (x, y), (x + c1x, y + c1y), (x + c2x, y + c2y), (x + ex, y + ey)
            for step in range(1, 9):
                t = step / 8
                points.append(tuple(
                    (1 - t) ** 3 * a + 3 * (1 - t) ** 2 * t * b + 3 * (1 - t) * t * t * c + t ** 3 * e
                    for a, b, c, e in zip(p0, p1, p2, p3)
                ))
            x, y = p3
            i += 6
        else:
            raise ValueError(f"Unexpected path coordinates after {command!r}")
    return polygons


def ribbon_shapes():
    """The macOS silhouette's two pieces, centred in the 100-unit artboard: (fold, body)."""
    d = re.search(r' d="([^"]+)"', RIBBON_SOURCE.read_text()).group(1)
    fold, body = (Polygon([(x, -y) for x, y in p.exterior.coords]) for p in potrace_polygons(d))
    minx, miny, maxx, maxy = body.union(fold).bounds
    k = RIBBON_SPAN / max(maxx - minx, maxy - miny)
    place = lambda piece: affinity.translate(
        affinity.scale(piece, k, k, origin=(0, 0)),
        50 - (minx + maxx) / 2 * k,
        50 - (miny + maxy) / 2 * k,
    )
    return place(fold), place(body)


def path_data(geometry, scale: float) -> str:
    parts = []
    for polygon in getattr(geometry, "geoms", [geometry]):
        for ring in (polygon.exterior, *polygon.interiors):
            coords = ring.simplify(0.02).coords[:-1]
            parts.append("M" + "L".join(f"{x * scale:.2f} {y * scale:.2f}" for x, y in coords) + "Z")
    return "".join(parts)


def svg(size: float, body: str, view: str | None = None) -> str:
    view = view or f"0 0 {size:g} {size:g}"
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{size:g}" height="{size:g}" '
            f'viewBox="{view}">{body}</svg>\n')


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def export_icon(icon: Path, tint: str, fold, body) -> None:
    scale = 10.24
    assets = icon / "Assets"
    for old in assets.glob("*"):
        old.unlink()
    write(assets / "Ribbon.svg", svg(1024, f'<path d="{path_data(body, scale)}" fill="{tint}"/>'))
    write(assets / "Fold.svg", svg(1024, f'<path d="{path_data(fold, scale)}" fill="{fold_color(tint)}"/>'))

    manifest = json.loads((icon / "icon.json").read_text())
    manifest["groups"] = [
        {
            "layers": [{"name": name, "glass": False, "image-name": f"{name}.svg"}],
            "lighting": "combined",
            "shadow": {"kind": "neutral", "opacity": 0.2},
            "translucency": {"enabled": False, "value": 0},
        }
        for name in ("Fold", "Ribbon")
    ]
    write(icon / "icon.json", json.dumps(manifest, indent=2) + "\n")


def export_templates(fold, body) -> None:
    """Single-colour vector layers that MobiusLogo tints from the palette."""
    catalog = APP / "Assets.xcassets"
    scale = 10.24
    inset = (100 - RIBBON_SPAN) / 2
    view = f"{inset * scale:g} {inset * scale:g} {RIBBON_SPAN * scale:g} {RIBBON_SPAN * scale:g}"
    layers = {
        "MobiusLogoRibbon": f'<path d="{path_data(body, scale)}"/>',
        "MobiusLogoFold": f'<path d="{path_data(fold, scale)}"/>',
    }
    for name, content in layers.items():
        folder = catalog / f"{name}.imageset"
        write(folder / f"{name}.svg", svg(RIBBON_SPAN * scale, content, view))
        write(folder / "Contents.json", json.dumps({
            "images": [{"filename": f"{name}.svg", "idiom": "universal"}],
            "info": {"author": "xcode", "version": 1},
            "properties": {"preserves-vector-representation": True, "template-rendering-intent": "template"},
        }, indent=2) + "\n")


def export_menu_bar(fold, body) -> None:
    """18-point two-tone ribbon for the Mac menu bar."""
    tint = TINTS[DEFAULT_TINT]
    inset = (100 - RIBBON_SPAN) / 2
    scale = 18 / RIBBON_SPAN
    view = f"{inset * scale:g} {inset * scale:g} 18 18"
    write(MENU_BAR, svg(18, f'<path d="{path_data(body, scale)}" fill="{tint}"/>'
                        f'<path d="{path_data(fold, scale)}" fill="{fold_color(tint)}"/>', view))


def export_reference(fold, body) -> None:
    tint = TINTS[DEFAULT_TINT]
    ribbon = (f'<path d="{path_data(body, 1)}" fill="{tint}"/>'
              f'<path d="{path_data(fold, 1)}" fill="{fold_color(tint)}"/>')
    inset = (100 - RIBBON_SPAN) / 2
    write(HERE / "MobiusMark.svg", svg(RIBBON_SPAN, ribbon, f"{inset:g} {inset:g} {RIBBON_SPAN} {RIBBON_SPAN}"))
    write(HERE / "MobiusIcon.svg", svg(
        100, f'<rect width="100" height="100" rx="22" fill="{NIGHT}"/>' + ribbon))


if __name__ == "__main__":
    fold, body = ribbon_shapes()
    export_icon(APP / "AppIcon.icon", TINTS[DEFAULT_TINT], fold, body)
    for name, tint in TINTS.items():
        if name != DEFAULT_TINT:
            export_icon(APP / f"AppIcon-{name}.icon", tint, fold, body)
    export_templates(fold, body)
    export_menu_bar(fold, body)
    export_reference(fold, body)
