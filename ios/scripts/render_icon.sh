#!/bin/bash
# Xcode build phase (ios/project.yml): the app icon from the repo's
# assets/icons/icon.png — the picture the desktop and Android use
# (IOS-SPEC.md D11). One 1024×1024 PNG in the asset catalog; Xcode makes
# every size from it. Re-runs only when icon.png changes.
#
# A square picture is scaled to 1024; any other shape is fitted inside
# 1024 and centred on the app's background (#1A1D21). Transparency is
# kept: fine for development builds, but App Store Connect refuses an
# icon with an alpha channel — the release step flattens it (spec M5).
set -euo pipefail

SRC="$SRCROOT/../assets/icons/icon.png"
OUT="$SRCROOT/Resources/Assets.xcassets/AppIcon.appiconset/icon-1024.png"

if [ ! -f "$SRC" ]; then
  echo "error: $SRC not found (the app icon every platform uses)"
  exit 1
fi

W=$(sips -g pixelWidth "$SRC" | awk '/pixelWidth/ { print $2 }')
H=$(sips -g pixelHeight "$SRC" | awk '/pixelHeight/ { print $2 }')

if [ "$W" = "$H" ]; then
  sips -s format png -z 1024 1024 "$SRC" --out "$OUT" >/dev/null
else
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  sips -s format png --resampleHeightWidthMax 1024 "$SRC" --out "$TMP/fit.png" >/dev/null
  sips -p 1024 1024 --padColor 1A1D21 "$TMP/fit.png" --out "$OUT" >/dev/null
fi
echo "app icon: $SRC (${W}x${H}) -> $OUT"
