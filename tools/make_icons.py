"""Renders Replay's icons into assets/*.ico (requires Pillow).

The mark is a circular "replay" arrow on a filled circle:
  replay.ico     app icon (coral, with the record dot)
  tray_idle.ico  tray icon while waiting (neutral grey, no dot)
  tray_rec.ico   tray icon while recording (coral, with the record dot)

Each size is rendered separately at 8x supersampling so small sizes stay crisp.
"""

import math
from pathlib import Path

from PIL import Image, ImageDraw

CORAL = (232, 54, 74, 255)
CORAL_LIGHT = (255, 92, 108, 255)
GREY = (88, 96, 110, 255)
GREY_LIGHT = (120, 128, 142, 255)
WHITE = (255, 255, 255, 255)

APP_SIZES = [16, 20, 24, 32, 40, 48, 64, 256]
TRAY_SIZES = [16, 20, 24, 32, 40, 48, 64]


def render(size, top, bottom, dot):
    ss = 8
    n = size * ss
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))

    # Vertical gradient disc.
    grad = Image.new("RGBA", (n, n))
    gd = ImageDraw.Draw(grad)
    for y in range(n):
        t = y / max(1, n - 1)
        c = tuple(round(top[i] + (bottom[i] - top[i]) * t) for i in range(4))
        gd.line([(0, y), (n, y)], fill=c)
    mask = Image.new("L", (n, n), 0)
    ImageDraw.Draw(mask).ellipse([n * 0.02, n * 0.02, n * 0.98, n * 0.98], fill=255)
    img.paste(grad, (0, 0), mask)

    d = ImageDraw.Draw(img)
    cx = cy = n / 2
    # Thicker strokes at small sizes so the glyph survives downscaling.
    small = size <= 24
    r = n * (0.255 if small else 0.25)
    stroke = n * (0.11 if small else 0.085)
    start, end = 290, 205  # degrees, clockwise from 3 o'clock; gap at the upper left
    # PIL draws arc width inward from the box edge; pad the box so the stroke centres on r.
    ro = r + stroke / 2
    box = [cx - ro, cy - ro, cx + ro, cy + ro]
    d.arc(box, start=start, end=end, fill=WHITE, width=round(stroke))
    # Round cap at the tail end.
    a = math.radians(end)
    tx, ty = cx + r * math.cos(a), cy + r * math.sin(a)
    d.ellipse([tx - stroke / 2, ty - stroke / 2, tx + stroke / 2, ty + stroke / 2], fill=WHITE)

    # Arrowhead at the start, pointing counter-clockwise (into the gap).
    a = math.radians(start)
    px, py = cx + r * math.cos(a), cy + r * math.sin(a)
    # Tip direction: the counter-clockwise tangent, turned slightly inward to follow the curve.
    t = a - math.pi / 2 + math.radians(14)
    tangent = (math.cos(t), math.sin(t))
    radial = (-tangent[1], tangent[0])
    head = n * (0.19 if small else 0.18)
    half = n * (0.12 if small else 0.10)
    tip = (px + tangent[0] * head, py + tangent[1] * head)
    b1 = (px + radial[0] * half - tangent[0] * head * 0.15, py + radial[1] * half - tangent[1] * head * 0.15)
    b2 = (px - radial[0] * half - tangent[0] * head * 0.15, py - radial[1] * half - tangent[1] * head * 0.15)
    d.polygon([tip, b1, b2], fill=WHITE)

    if dot:
        dr = n * (0.085 if small else 0.075)
        d.ellipse([cx - dr, cy - dr, cx + dr, cy + dr], fill=WHITE)

    return img.resize((size, size), Image.LANCZOS)


def save_ico(path, sizes, top, bottom, dot):
    images = [render(s, top, bottom, dot) for s in sizes]
    largest = images[-1]
    largest.save(path, format="ICO", sizes=[(s, s) for s in sizes], append_images=images[:-1])


def main():
    out = Path(__file__).resolve().parent.parent / "assets"
    out.mkdir(exist_ok=True)
    save_ico(out / "replay.ico", APP_SIZES, CORAL_LIGHT, CORAL, True)
    save_ico(out / "tray_idle.ico", TRAY_SIZES, GREY_LIGHT, GREY, False)
    save_ico(out / "tray_rec.ico", TRAY_SIZES, CORAL_LIGHT, CORAL, True)
    # Preview strip for the README.
    strip = Image.new("RGBA", (256 * 3 + 64, 256 + 32), (0, 0, 0, 0))
    for i, (top, bottom, dot) in enumerate([(CORAL_LIGHT, CORAL, True), (GREY_LIGHT, GREY, False), (CORAL_LIGHT, CORAL, True)]):
        strip.paste(render(256 if i == 0 else 128, top, bottom, dot), (i * 288 + (0 if i == 0 else 64), 16 if i == 0 else 80))
    strip.save(out / "icons-preview.png")
    print("wrote", ", ".join(p.name for p in sorted(out.glob("*.ico"))))


if __name__ == "__main__":
    main()
