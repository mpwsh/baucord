#!/usr/bin/env python3
"""The tray's two icons: a flat glyph that still reads at 16 px.

The app icon is a painted dog in headphones; shrunk to a tray slot (16
px on a Windows taskbar at 100 %, 18 pt in macOS's menu bar) it was a
grey blur. This draws the same idea as three flat shapes — a disc, a
white head, dark headphones — with the eyes and nose only where there
are pixels for them (32 px and up). No ears: under the band, at tray
size, they were stray white specks. Two of them:

- `assets/icons/tray.png`: the disc in the app icon's slate — also
  the desktop windows' icon (`main.slint`, `viewer.slint`);
- `assets/icons/tray-quiet.png`: the disc red — we are muted or
  deafened (`tray.slint` switches between the two).

64 px a side: Windows, macOS and the Linux trays all scale it down to
their slot, and every shape sits on whole pixels at 16, 32 and 64.

Run by hand when the design changes, and commit the output; the build
does not run it. Needs Pillow:

    python3 scripts/tray-icons.py            # from the repo root
    python3 scripts/tray-icons.py --preview  # also a sheet at 16–64 px
"""

import sys
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
ICONS = ROOT / "assets" / "icons"

SIZE = 64
# Drawn at 16x and filtered down: smooth edges without a vector library.
SCALE = 16

SLATE = (74, 82, 102, 255)      # the app icon's headphones, a step lighter
QUIET = (224, 30, 90, 255)      # Theme.danger
HEAD = (246, 244, 241, 255)     # the dog's white
GEAR = (40, 45, 58, 255)        # headphones, eyes and nose


def draw(disc):
    s = SCALE
    img = Image.new("RGBA", (SIZE * s, SIZE * s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    def box(x0, y0, x1, y1):
        return [round(x0 * s), round(y0 * s), round(x1 * s), round(y1 * s)]

    # The disc, the whole tile.
    d.ellipse(box(0.5, 0.5, 63.5, 63.5), fill=disc)
    # The head: a round crown and a fuller beard below it.
    d.ellipse(box(17, 15, 47, 45), fill=HEAD)
    d.rounded_rectangle(box(19, 30, 45, 55), radius=round(11 * s), fill=HEAD)
    # The headband over the crown, then the cups over the cheeks.
    d.arc(box(12, 7, 52, 47), start=195, end=345, fill=GEAR, width=round(5 * s))
    d.rounded_rectangle(box(10, 26, 20, 45), radius=round(4.5 * s), fill=GEAR)
    d.rounded_rectangle(box(44, 26, 54, 45), radius=round(4.5 * s), fill=GEAR)
    # The face: eyes and a nose.
    d.ellipse(box(25.5, 31, 30.5, 36), fill=GEAR)
    d.ellipse(box(33.5, 31, 38.5, 36), fill=GEAR)
    d.ellipse(box(28, 40, 36, 46), fill=GEAR)
    return img.resize((SIZE, SIZE), Image.LANCZOS)


def preview(icons):
    """Each icon at the tray's sizes, on a light and a dark bar."""
    sizes = (16, 18, 22, 24, 32, 64)
    pad = 12
    width = pad + sum(n + pad for n in sizes)
    row = 64 + 2 * pad
    sheet = Image.new("RGBA", (width, row * 2 * len(icons)), (0, 0, 0, 255))
    y = 0
    for icon in icons:
        for bar in ((236, 236, 236, 255), (32, 32, 32, 255)):
            band = Image.new("RGBA", (width, row), bar)
            x = pad
            for n in sizes:
                small = icon.resize((n, n), Image.LANCZOS)
                band.alpha_composite(small, (x, pad + (64 - n) // 2))
                x += n + pad
            sheet.alpha_composite(band, (0, y))
            y += row
    return sheet


def main():
    normal = draw(SLATE)
    quiet = draw(QUIET)
    normal.save(ICONS / "tray.png", optimize=True)
    quiet.save(ICONS / "tray-quiet.png", optimize=True)
    if "--preview" in sys.argv:
        rest = sys.argv[sys.argv.index("--preview") + 1 :]
        out = Path(rest[0]) if rest else Path("tray-preview.png")
        sheet = preview([normal, quiet])
        sheet.resize((sheet.width * 3, sheet.height * 3), Image.NEAREST).save(out)
        print(f"wrote {out}")
    print("wrote assets/icons/tray.png and tray-quiet.png")


if __name__ == "__main__":
    main()
