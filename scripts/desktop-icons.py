#!/usr/bin/env python3
"""The desktop's small icons from the app icon (review P2-45).

`assets/icons/icon.png` is 1254 px a side: right for the phones' stores,
but the desktop window and the tray each decoded it whole — 6 MiB of
pixels apiece, over Slint's image cache, so both stayed resident — and
the Windows `.ico` built from it carried an uncompressed 256 px bitmap
(726 KB). This writes:

- `assets/icons/icon-256.png`: what `main.slint` and `tray.slint` load;
- `assets/icons/icon.ico`: 16–64 px as bitmaps (what old shell code
  reads) and 256 px as a PNG (Windows scales that one for the sizes in
  between), embedded in the exe by `crates/desktop/build.rs`;
- `web/img/favicon.ico`: the same file.

Run by hand when the icon changes, and commit the output; the build does
not run it. Needs Pillow:

    python3 scripts/desktop-icons.py            # from the repo root
"""

import io
import struct
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
ICONS = ROOT / "assets" / "icons"
BITMAP_SIZES = (16, 24, 32, 48, 64)


def bitmap_entry(image):
    """A 32-bit ICO bitmap: header, bottom-up BGRA rows, an empty mask."""
    width, height = image.size
    pixels = image.tobytes("raw", "BGRA")
    rows = [pixels[y * width * 4 : (y + 1) * width * 4] for y in range(height)]
    colour = b"".join(reversed(rows))
    mask = bytes(((width + 31) // 32) * 4 * height)
    header = struct.pack(
        "<IiiHHIIiiII", 40, width, height * 2, 1, 32, 0, len(colour) + len(mask), 0, 0, 0, 0
    )
    return header + colour + mask


def main():
    source = Image.open(ICONS / "icon.png").convert("RGBA")
    small = source.resize((256, 256), Image.LANCZOS)
    small.save(ICONS / "icon-256.png", optimize=True)

    entries = [
        (size, bitmap_entry(source.resize((size, size), Image.LANCZOS)))
        for size in BITMAP_SIZES
    ]
    png = io.BytesIO()
    small.save(png, format="PNG", optimize=True)
    entries.append((256, png.getvalue()))

    ico = struct.pack("<HHH", 0, 1, len(entries))
    offset = 6 + 16 * len(entries)
    for size, data in entries:
        # A width or height of 0 means 256.
        ico += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    ico += b"".join(data for _, data in entries)
    for path in (ICONS / "icon.ico", ROOT / "web" / "img" / "favicon.ico"):
        path.write_bytes(ico)
        print(f"{path.relative_to(ROOT)}: {len(ico)} bytes")
    print(f"assets/icons/icon-256.png: {(ICONS / 'icon-256.png').stat().st_size} bytes")


if __name__ == "__main__":
    main()
