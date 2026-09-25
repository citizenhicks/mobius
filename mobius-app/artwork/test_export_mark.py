"""Run with: uv run --with shapely python test_export_mark.py"""

from shapely.geometry import Point

from export_mark import BOT_CENTER, BOT_RADIUS, GAP, fold_color, potrace_polygons, ribbon_shapes


square, = potrace_polygons("M0 0l10 0 0 10 -10 0z")
assert square.area == 100
for malformed in ("M0 0H10", "0 0", "M0 0l10 0 0 10z1 1"):
    try:
        potrace_polygons(malformed)
    except ValueError:
        pass
    else:
        raise AssertionError(f"Accepted unsupported path: {malformed}")
assert fold_color("#000000") == "#595959"
assert fold_color("#FFFFFF") == "#FFFFFF"
for piece in ribbon_shapes():
    assert piece.is_valid and not piece.is_empty
    assert piece.distance(Point(BOT_CENTER)) >= BOT_RADIUS + GAP - 0.01
