#!/usr/bin/env python3
"""The notifications' status-bar icon from the app icon (ANDROID-SPEC.md D52).

Android draws a notification's small icon as a white silhouette (it uses
only the alpha channel), so the full-colour app icon cannot be used as
is. This cuts the dog with its headphones out of `assets/icons/icon.png`
— everything inside the round icon that is not its blue-grey backdrop —
keeps the eyes and the nose as holes so it still reads as a face at 24 dp,
and writes `res/drawable-<density>/ic_stat_baucord.png` (24 dp, the
content in the middle 22 dp, as Android's guidelines ask).

It is tuned to the current picture (the backdrop colour and where the
eyes and nose are), so it is run by hand when the icon changes, and its
output is committed; the build does not run it. Needs Pillow:

    python3 android/notification-icon.py            # from the repo root
"""

import math
import sys
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "assets" / "icons" / "icon.png"
RES = ROOT / "android" / "app" / "src" / "main" / "res"
DENSITIES = {"mdpi": 24, "hdpi": 36, "xhdpi": 48, "xxhdpi": 72, "xxxhdpi": 96}

# Tuned to icon.png, in the coordinates of its 1024 px rendering.
BACKDROP = (178, 182, 195)  # the circle behind the dog
BACKDROP_DISTANCE = 30  # farther than this (RGB) = the dog or the headphones
RIM = 14  # px kept clear inside the circle's edge (its antialiasing)
DARK = 95  # luminance below this = eyes, nose, headphones
FEATURES = [(380, 443), (640, 443), (510, 620)]  # left eye, right eye, nose
FEATURE_GROW = 9  # px the holes are widened by, to survive 24 dp


def silhouette(img):
    """The white-on-clear mask of the dog and its headphones at 1024 px."""
    w, h = img.size
    px = img.load()
    bbox = img.getchannel("A").getbbox()
    cx, cy = (bbox[0] + bbox[2]) / 2, (bbox[1] + bbox[3]) / 2
    radius = min(bbox[2] - bbox[0], bbox[3] - bbox[1]) / 2 - RIM
    mask = Image.new("L", (w, h), 0)
    dark = Image.new("L", (w, h), 0)
    mp, dp = mask.load(), dark.load()
    for y in range(h):
        for x in range(w):
            r, g, b, a = px[x, y]
            if a < 128 or math.hypot(x - cx, y - cy) > radius:
                continue
            if math.dist((r, g, b), BACKDROP) > BACKDROP_DISTANCE:
                mp[x, y] = 255
            if 0.3 * r + 0.59 * g + 0.11 * b < DARK:
                dp[x, y] = 255
    # Fill the small holes the fur's shading leaves (anything enclosed
    # and under 6000 px; the gap under the headband is bigger and stays).
    outside = ImageChops.invert(mask)
    ImageDraw.floodfill(outside, (0, 0), 128)
    holes = outside.point(lambda v: 255 if v == 255 else 0)
    for part in components(holes):
        if len(part) < 6000:
            for p in part:
                mp[p] = 255
    mask = mask.filter(ImageFilter.MaxFilter(5)).filter(ImageFilter.MinFilter(5))
    # The eyes and the nose: the dark blobs at the seeds, cut out.
    features = Image.new("L", (w, h), 0)
    fp = features.load()
    for seed in FEATURES:
        start = nearest(dark, seed)
        if start is None:
            sys.exit(f"no dark feature near {seed}: re-tune FEATURES for this icon")
        for p in flood(dark, start):
            fp[p] = 255
    mask = ImageChops.subtract(mask, features.filter(ImageFilter.MaxFilter(FEATURE_GROW)))
    # Keep the large parts only (the head, the headband): no specks.
    keep = Image.new("L", (w, h), 0)
    kp = keep.load()
    for part in components(mask.point(lambda v: 255 if v > 127 else 0)):
        if len(part) > 20000:
            for p in part:
                kp[p] = 255
    return ImageChops.darker(keep.filter(ImageFilter.MaxFilter(3)), mask)


def components(mask):
    """The 4-connected white regions of a mask, as lists of points."""
    w, h = mask.size
    mp = mask.load()
    seen = bytearray(w * h)
    for y in range(h):
        for x in range(w):
            if mp[x, y] and not seen[y * w + x]:
                seen[y * w + x] = 1
                yield list(flood(mask, (x, y), seen))


def flood(mask, start, seen=None):
    """The white region of `mask` around `start`."""
    w, h = mask.size
    mp = mask.load()
    seen = seen if seen is not None else bytearray(w * h)
    seen[start[1] * w + start[0]] = 1
    stack = [start]
    while stack:
        x, y = stack.pop()
        yield (x, y)
        for nx, ny in ((x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)):
            if 0 <= nx < w and 0 <= ny < h and mp[nx, ny] and not seen[ny * w + nx]:
                seen[ny * w + nx] = 1
                stack.append((nx, ny))


def nearest(mask, seed, reach=40):
    """The white pixel of `mask` closest to `seed`, within `reach`."""
    mp = mask.load()
    for r in range(reach):
        for dy in range(-r, r + 1):
            for dx in range(-r, r + 1):
                x, y = seed[0] + dx, seed[1] + dy
                if mp[x, y]:
                    return (x, y)
    return None


def main():
    img = Image.open(SOURCE).convert("RGBA")
    if img.size != (1024, 1024):
        side = max(img.size)
        square = Image.new("RGBA", (side, side), (0, 0, 0, 0))
        square.paste(img, ((side - img.width) // 2, (side - img.height) // 2))
        img = square.resize((1024, 1024), Image.LANCZOS)
    mask = silhouette(img)
    crop = mask.crop(mask.getbbox())
    side = max(crop.size)
    square = Image.new("L", (side, side), 0)
    square.paste(crop, ((side - crop.width) // 2, (side - crop.height) // 2))
    # The content in the middle 22 of 24 dp.
    canvas_side = round(side * 24 / 22)
    canvas = Image.new("L", (canvas_side, canvas_side), 0)
    offset = (canvas_side - side) // 2
    canvas.paste(square, (offset, offset))
    for density, px in DENSITIES.items():
        alpha = canvas.resize((px, px), Image.LANCZOS)
        white = Image.new("RGBA", (px, px), (255, 255, 255, 255))
        clear = Image.new("RGBA", (px, px), (255, 255, 255, 0))
        out_dir = RES / f"drawable-{density}"
        out_dir.mkdir(parents=True, exist_ok=True)
        Image.composite(white, clear, alpha).save(out_dir / "ic_stat_baucord.png", optimize=True)
        print(f"{out_dir.relative_to(ROOT)}/ic_stat_baucord.png ({px} px)")


if __name__ == "__main__":
    main()
