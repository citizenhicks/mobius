"""Export the flat möbius mark: Icon Composer layers, accent alternates, in-app templates, menu bar.

Run from this directory: uv run --with shapely python export_mark.py
"""

import json
import re
from pathlib import Path

from shapely.geometry import Point, Polygon

HERE = Path(__file__).parent
APP = HERE / "../apple/Sources/MobiusApp"
MENU_BAR = APP / "Assets.xcassets/MobiusMenuBar.imageset/MobiusMenuBar.svg"
RIBBON_SOURCE = HERE / "ribbon.svg"  # the original potrace silhouette

NIGHT, SNOW = "#2E3440", "#ECEFF4"
TINTS = {
    "blue": "#5E81AC", "teal": "#8FBCBB", "green": "#A3BE8C", "yellow": "#EBCB8B",
    "orange": "#D08770", "red": "#BF616A", "purple": "#B48EAD",
}
DEFAULT_TINT = "blue"

# Artboard units: the icon square is 100 × 100.
RIBBON_ORIGIN, RIBBON_SIZE = (7, 5), 72
BOT_CENTER, BOT_RADIUS, GAP = (73, 74), 15.5, 2.4


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


def ribbon_uncut():
    """The macOS silhouette's two pieces in artboard units: (fold, body)."""
    d = re.search(r' d="([^"]+)"', RIBBON_SOURCE.read_text()).group(1)
    k = RIBBON_SIZE / 5120
    ox, oy = RIBBON_ORIGIN
    place = lambda poly: Polygon([(ox + px * k, oy + RIBBON_SIZE - py * k) for px, py in poly.exterior.coords])
    fold, body = (place(p) for p in potrace_polygons(d))
    return fold, body


def ribbon_shapes():
    """The ribbon with a transparent gap cut around the Bot."""
    cut = Point(BOT_CENTER).buffer(BOT_RADIUS + GAP, quad_segs=32)
    return tuple(piece.difference(cut) for piece in ribbon_uncut())


def path_data(geometry, scale: float) -> str:
    parts = []
    for polygon in getattr(geometry, "geoms", [geometry]):
        for ring in (polygon.exterior, *polygon.interiors):
            coords = ring.simplify(0.02).coords[:-1]
            parts.append("M" + "L".join(f"{x * scale:.2f} {y * scale:.2f}" for x, y in coords) + "Z")
    return "".join(parts)


def eyes(scale: float) -> list[tuple[float, float, float, float]]:
    """(cx, cy, radius, stroke) for the Bot's two `o` eyes; the near one is larger."""
    cx, cy = BOT_CENTER
    r = BOT_RADIUS
    return [((cx - 0.12 * r + dx * r) * scale, (cy - 0.2 * r) * scale, rr * r * scale, 0.15 * r * scale)
            for dx, rr in ((-0.3, 0.15), (0.3, 0.175))]


def svg(size: float, body: str, view: str | None = None) -> str:
    view = view or f"0 0 {size:g} {size:g}"
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{size:g}" height="{size:g}" '
            f'viewBox="{view}">{body}</svg>\n')


def bot_body(scale: float, ball: str, eye: str) -> str:
    cx, cy = BOT_CENTER
    ring = "".join(f'<circle cx="{x:.2f}" cy="{y:.2f}" r="{r:.2f}" fill="none" stroke="{eye}" '
                   f'stroke-width="{w:.2f}"/>' for x, y, r, w in eyes(scale))
    return f'<circle cx="{cx * scale:g}" cy="{cy * scale:g}" r="{BOT_RADIUS * scale:g}" fill="{ball}"/>{ring}'


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
    write(assets / "Bot.svg", svg(1024, bot_body(scale, SNOW, NIGHT)))

    manifest = json.loads((icon / "icon.json").read_text())
    manifest["groups"] = [
        {
            "layers": [{"name": name, "glass": False, "image-name": f"{name}.svg"}],
            "lighting": "combined",
            "shadow": {"kind": "neutral", "opacity": 0.2},
            "translucency": {"enabled": False, "value": 0},
        }
        for name in ("Bot", "Fold", "Ribbon")
    ]
    write(icon / "icon.json", json.dumps(manifest, indent=2) + "\n")


def export_templates(fold, body) -> None:
    """Single-colour vector layers that MobiusLogo tints from the palette."""
    catalog = APP / "Assets.xcassets"
    scale = 10.24
    view = f"{2 * scale:g} {2 * scale:g} {94 * scale:g} {94 * scale:g}"
    layers = {
        "MobiusLogoRibbon": f'<path d="{path_data(body, scale)}"/>',
        "MobiusLogoFold": f'<path d="{path_data(fold, scale)}"/>',
        "MobiusLogoBot": f'<circle cx="{BOT_CENTER[0] * scale:g}" cy="{BOT_CENTER[1] * scale:g}" r="{BOT_RADIUS * scale:g}"/>',
        "MobiusLogoEyes": "".join(f'<circle cx="{x:.2f}" cy="{y:.2f}" r="{r:.2f}" fill="none" stroke="#000" '
                                  f'stroke-width="{w:.2f}"/>' for x, y, r, w in eyes(scale)),
    }
    for name, content in layers.items():
        folder = catalog / f"{name}.imageset"
        write(folder / f"{name}.svg", svg(94 * scale, content, view))
        write(folder / "Contents.json", json.dumps({
            "images": [{"filename": f"{name}.svg", "idiom": "universal"}],
            "info": {"author": "xcode", "version": 1},
            "properties": {"preserves-vector-representation": True, "template-rendering-intent": "template"},
        }, indent=2) + "\n")


def export_menu_bar() -> None:
    """18-point two-tone ribbon for the Mac menu bar. The Bot is too small to read there."""
    tint = TINTS[DEFAULT_TINT]
    fold, body = ribbon_uncut()
    scale = 18 / RIBBON_SIZE
    x0, y0 = RIBBON_ORIGIN
    view = f"{x0 * scale:g} {y0 * scale:g} 18 18"
    write(MENU_BAR, svg(18, f'<path d="{path_data(body, scale)}" fill="{tint}"/>'
                        f'<path d="{path_data(fold, scale)}" fill="{fold_color(tint)}"/>', view))


def export_reference(fold, body) -> None:
    tint = TINTS[DEFAULT_TINT]
    write(HERE / "MobiusMark.svg", svg(
        94, f'<path d="{path_data(body, 1)}" fill="{tint}"/><path d="{path_data(fold, 1)}" fill="{fold_color(tint)}"/>'
            f'{bot_body(1, SNOW, NIGHT)}', "2 2 94 94"))


if __name__ == "__main__":
    fold, body = ribbon_shapes()
    export_icon(APP / "AppIcon.icon", TINTS[DEFAULT_TINT], fold, body)
    for name, tint in TINTS.items():
        if name != DEFAULT_TINT:
            export_icon(APP / f"AppIcon-{name}.icon", tint, fold, body)
    export_templates(fold, body)
    export_menu_bar()
    export_reference(fold, body)
