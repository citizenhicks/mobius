"""Run with: uv run --with shapely python test_export_mark.py"""

from export_mark import RIBBON_SPAN, fold_color, potrace_polygons, ribbon_shapes

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
fold, body = ribbon_shapes()
for piece in (fold, body):
    assert piece.is_valid and not piece.is_empty
minx, miny, maxx, maxy = fold.union(body).bounds
assert abs(max(maxx - minx, maxy - miny) - RIBBON_SPAN) < 0.01
assert abs((minx + maxx) / 2 - 50) < 0.01 and abs((miny + maxy) / 2 - 50) < 0.01
