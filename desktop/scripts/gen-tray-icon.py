"""Draw the tray icons from PekoHub's favicon (`public/favicon.svg`, the
same file PekoHub serves): two white leaves on a teal rounded tile, the
front leaf set apart from the back one by a teal outline.

- `tray.png` (Windows, Linux): the favicon itself, 32 px, filling the
  square like the icons beside it.
- `tray-template.png` (macOS): the same design as a template image, black
  where the tile shows and clear where the leaves are, so the menu bar
  tints it for light and dark mode. 36 px for an 18 pt slot at 2x, with a
  little air around the tile like other menu bar icons.

Rendered large and scaled down for clean edges.

    python -I scripts/gen-tray-icon.py   # writes into src-tauri/icons/
"""

import math
import sys
from pathlib import Path

from PIL import Image, ImageDraw

SCALE = 32  # supersampling factor
TEAL = (0, 136, 128, 255)  # #008880
WHITE = (255, 255, 255, 255)

# favicon.svg, in its 100x100 viewBox.
TILE_RADIUS = 22.5
GROUP_OFFSET = (7.09, 1.27)
GROUP_SCALE = 0.91
ORIGIN = (50.0, 88.0)
# (rotation in degrees, quadratic control point, tip): the path
# `M0 0 Q cx cy tx 0 Q cx -cy 0 0`, placed at ORIGIN.
BACK_LEAF = (-110, (22.2, 46.0), 74.0)
FRONT_LEAF = (-58, (15.0, 32.0), 50.0)
LEAF_STROKE = 3.0  # each leaf is stroked in its own colour, round joins
GAP_STROKE = 17.0  # the teal outline that sets the front leaf apart


def leaf_outline(rotation, control, tip, steps=128):
    """The leaf as a polygon, in viewBox coordinates."""
    cx, cy = control
    pts = []
    for sign in (1, -1):
        for i in range(steps + 1):
            t = i / steps if sign == 1 else 1 - i / steps
            x = 2 * (1 - t) * t * cx + t * t * tip
            y = 2 * (1 - t) * t * cy * sign
            pts.append((x, y))
    a = math.radians(rotation)
    out = []
    for x, y in pts:
        # rotate, place at ORIGIN, then the group's scale and offset
        gx = ORIGIN[0] + x * math.cos(a) - y * math.sin(a)
        gy = ORIGIN[1] + x * math.sin(a) + y * math.cos(a)
        out.append((GROUP_OFFSET[0] + gx * GROUP_SCALE, GROUP_OFFSET[1] + gy * GROUP_SCALE))
    return out


def fill_stroked(draw, outline, stroke, colour, k):
    """Fill `outline` and stroke it `stroke` wide with round joins
    (stroke in the group's units, like the SVG)."""
    pts = [(x * k, y * k) for x, y in outline]
    draw.polygon(pts, fill=colour)
    r = stroke * GROUP_SCALE * k / 2
    draw.line(pts + [pts[0]], fill=colour, width=max(1, round(2 * r)))
    for x, y in pts:
        draw.ellipse((x - r, y - r, x + r, y + r), fill=colour)


def favicon(size_px):
    """The favicon at `size_px` (supersampled), RGBA."""
    k = size_px / 100
    img = Image.new("RGBA", (size_px, size_px), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle((0, 0, size_px - 1, size_px - 1), radius=TILE_RADIUS * k, fill=TEAL)
    fill_stroked(d, leaf_outline(*BACK_LEAF), LEAF_STROKE, WHITE, k)
    front = leaf_outline(*FRONT_LEAF)
    fill_stroked(d, front, GAP_STROKE, TEAL, k)
    fill_stroked(d, front, LEAF_STROKE, WHITE, k)
    return img


def scaled(img, size):
    # Premultiplied, so clear pixels don't bleed colour into the edges.
    return img.convert("RGBa").resize((size, size), Image.LANCZOS).convert("RGBA")


def tray(size=32):
    return scaled(favicon(size * SCALE), size)


def tray_template(size=36, margin=2):
    """Black where the tile shows, clear where the leaves are."""
    inner = size - 2 * margin
    art = favicon(inner * SCALE)
    # Leaves are white (red 255), the tile teal (red 0).
    r, _, _, a = art.split()
    alpha = Image.eval(r, lambda v: 255 - v)
    alpha = Image.composite(alpha, Image.new("L", art.size, 0), a)
    black = Image.new("RGBA", art.size, (0, 0, 0, 255))
    black.putalpha(alpha)
    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(scaled(black, inner), (margin, margin))
    return out


def main(out_dir: Path) -> None:
    tray().save(out_dir / "tray.png", optimize=True)
    tray_template().save(out_dir / "tray-template.png", optimize=True)


if __name__ == "__main__":
    here = Path(__file__).resolve().parent
    main(Path(sys.argv[1]) if len(sys.argv) > 1 else here.parent / "src-tauri" / "icons")
