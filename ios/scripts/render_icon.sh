#!/bin/bash
# Xcode build phase (ios/project.yml): the app icon from the repo's
# assets/icons/icon.png — the picture the desktop and Android use
# (IOS-SPEC.md D11). One 1024×1024 PNG in the asset catalog; Xcode makes
# every size from it. Re-runs only when icon.png or these scripts change.
#
# The picture is drawn onto the app's background (#1A1D21) and written
# without an alpha channel, in every configuration (IOS-SPEC.md D67,
# MOB-3): App Store Connect refuses an icon with one, and iOS shows an
# icon's transparent pixels as black anyway. A square picture fills the
# square; any other shape is fitted inside it and centred.
set -euo pipefail

SRC="$SRCROOT/../assets/icons/icon.png"
OUT="$SRCROOT/Resources/Assets.xcassets/AppIcon.appiconset/icon-1024.png"
HELPER="$SRCROOT/scripts/flatten_icon.swift"

if [ ! -f "$SRC" ]; then
  echo "error: $SRC not found (the app icon every platform uses)"
  exit 1
fi

# The helper is a macOS program, but a build phase's environment is set
# up for the iPhone (SDKROOT, deployment targets): run it in a clean one,
# on the same Xcode.
ENVIRONMENT=(PATH=/usr/bin:/bin HOME="$HOME" TMPDIR="${TMPDIR:-/tmp}")
if [ -n "${DEVELOPER_DIR:-}" ]; then
  ENVIRONMENT+=(DEVELOPER_DIR="$DEVELOPER_DIR")
fi
if ! env -i "${ENVIRONMENT[@]}" xcrun --sdk macosx swift "$HELPER" "$SRC" "$OUT" 1024 1A1D21; then
  echo "error: flatten_icon.swift could not draw the app icon"
  exit 1
fi

# The point of the step: refuse to go on with an icon App Store Connect
# would turn away.
if ! sips -g hasAlpha "$OUT" | grep 'hasAlpha: no' >/dev/null; then
  sips -g hasAlpha "$OUT"
  echo "error: $OUT still has an alpha channel"
  exit 1
fi

W=$(sips -g pixelWidth "$SRC" | awk '/pixelWidth/ { print $2 }')
H=$(sips -g pixelHeight "$SRC" | awk '/pixelHeight/ { print $2 }')
echo "app icon: $SRC (${W}x${H}) -> $OUT (1024x1024, opaque)"
