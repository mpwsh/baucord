#!/bin/bash
# Builds baucord.app — one universal bundle from the two release
# binaries — and what is published from it (macos/README.md):
#
#   baucord-v<version>-macos.dmg   the download: the app and an
#                                  Applications shortcut to drag it onto
#   baucord-v<version>-macos.zip   the same bundle, for the updater of a
#                                  copy installed as baucord.app
#
# Usage: macos/package.sh VERSION ARM64_BINARY INTEL_BINARY OUT_DIR
#
# Signing, from the environment:
#   SIGN_IDENTITY  a "Developer ID Application" identity (its SHA-1 or
#                  name): the app and the disk image are signed with it,
#                  under the hardened runtime. Empty: signed ad hoc,
#                  which macOS runs only after the user allows it.
#   KEYCHAIN       the keychain that holds it (optional).
#   NOTARIZE=1     also notarize both with Apple and staple the tickets;
#                  needs SIGN_IDENTITY and an App Store Connect API key:
#                  ASC_KEY_PATH (the .p8), ASC_KEY_ID, ASC_ISSUER_ID.
set -euo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: $0 VERSION ARM64_BINARY INTEL_BINARY OUT_DIR" >&2
  exit 2
fi
VERSION="$1"
ARM64_BIN="$2"
INTEL_BIN="$3"
OUT_DIR="$4"
SIGN_IDENTITY="${SIGN_IDENTITY:-}"
NOTARIZE="${NOTARIZE:-}"

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() {
  echo "::error::$*" >&2
  exit 1
}

if [ "$NOTARIZE" = 1 ]; then
  [ -n "$SIGN_IDENTITY" ] || fail "notarizing needs a Developer ID identity (SIGN_IDENTITY)"
  for NAME in ASC_KEY_PATH ASC_KEY_ID ASC_ISSUER_ID; do
    [ -n "${!NAME:-}" ] || fail "notarizing needs $NAME (the App Store Connect API key)"
  done
fi

APP="$WORK/baucord.app"
CONTENTS="$APP/Contents"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources" "$OUT_DIR"

# ── The bundle ────────────────────────────────────────────────────────
lipo -create -output "$CONTENTS/MacOS/baucord" "$ARM64_BIN" "$INTEL_BIN"
chmod 755 "$CONTENTS/MacOS/baucord"
ARCHS="$(lipo -archs "$CONTENTS/MacOS/baucord")"
case " $ARCHS " in
  *" arm64 "*) ;;
  *) fail "the universal binary has no arm64 slice ($ARCHS)" ;;
esac
case " $ARCHS " in
  *" x86_64 "*) ;;
  *) fail "the universal binary has no x86_64 slice ($ARCHS)" ;;
esac
echo "baucord: $ARCHS"

sed "s/__VERSION__/$VERSION/g" "$HERE/Info.plist" > "$CONTENTS/Info.plist"
plutil -lint "$CONTENTS/Info.plist" >/dev/null
printf 'APPL????' > "$CONTENTS/PkgInfo"

# The permission prompts in the app's other languages: the iPhone's
# strings, which say the same thing.
for LPROJ in "$REPO"/ios/Resources/*.lproj; do
  mkdir -p "$CONTENTS/Resources/$(basename "$LPROJ")"
  cp "$LPROJ/InfoPlist.strings" "$CONTENTS/Resources/$(basename "$LPROJ")/"
done

# The icon, from the picture every platform uses (IOS-SPEC.md D11).
ICONSET="$WORK/baucord.iconset"
mkdir -p "$ICONSET"
for SIZE in 16 32 128 256 512; do
  sips -s format png -z "$SIZE" "$SIZE" "$REPO/assets/icons/icon.png" \
    --out "$ICONSET/icon_${SIZE}x${SIZE}.png" >/dev/null
  DOUBLE=$((SIZE * 2))
  sips -s format png -z "$DOUBLE" "$DOUBLE" "$REPO/assets/icons/icon.png" \
    --out "$ICONSET/icon_${SIZE}x${SIZE}@2x.png" >/dev/null
done
iconutil -c icns -o "$CONTENTS/Resources/baucord.icns" "$ICONSET"

# ── Signing ───────────────────────────────────────────────────────────
KEYCHAIN_ARGS=()
if [ -n "${KEYCHAIN:-}" ]; then
  KEYCHAIN_ARGS=(--keychain "$KEYCHAIN")
fi
if [ -n "$SIGN_IDENTITY" ]; then
  codesign --force --timestamp --options runtime \
    --entitlements "$HERE/baucord.entitlements" \
    --sign "$SIGN_IDENTITY" ${KEYCHAIN_ARGS[@]+"${KEYCHAIN_ARGS[@]}"} "$APP"
else
  echo "::warning::no Developer ID: baucord.app is signed ad hoc, and macOS asks users to allow it in System Settings → Privacy & Security before it opens"
  codesign --force --sign - "$APP"
fi
codesign --verify --strict --verbose=2 "$APP"
codesign --display --verbose=2 "$APP" 2>&1 | grep -E "^(Identifier|Authority|TeamIdentifier|Runtime)" || true

# ditto without the extended attributes, resource forks, quarantine and
# ACLs: none of them belong in the archive, and AppleDouble files
# unpacked into a bundle break its seal.
zip_app() {
  ditto -c -k --norsrc --noextattr --noqtn --noacl --keepParent "$APP" "$1"
}

# notarize FILE: send it to Apple's notary service and wait for its
# verdict; on a refusal, print Apple's log of why.
notarize() {
  local file="$1" result="$WORK/notary.json" status id
  xcrun notarytool submit "$file" \
    --key "$ASC_KEY_PATH" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER_ID" \
    --wait --timeout 30m --output-format json > "$result" || true
  status="$(plutil -extract status raw -o - "$result" 2>/dev/null || echo unknown)"
  id="$(plutil -extract id raw -o - "$result" 2>/dev/null || echo unknown)"
  echo "notarization of $(basename "$file"): $status ($id)"
  if [ "$status" != "Accepted" ]; then
    cat "$result" >&2 || true
    if [ "$id" != unknown ]; then
      xcrun notarytool log "$id" \
        --key "$ASC_KEY_PATH" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER_ID" >&2 || true
    fi
    fail "Apple did not notarize $(basename "$file") ($status)"
  fi
}

if [ "$NOTARIZE" = 1 ]; then
  zip_app "$WORK/notarize.zip"
  notarize "$WORK/notarize.zip"
  xcrun stapler staple "$APP"
  xcrun stapler validate "$APP"
fi

# ── The updater's zip ─────────────────────────────────────────────────
ZIP="$OUT_DIR/baucord-v$VERSION-macos.zip"
rm -f "$ZIP"
zip_app "$ZIP"

# ── The disk image ────────────────────────────────────────────────────
DMG="$OUT_DIR/baucord-v$VERSION-macos.dmg"
STAGE="$WORK/dmg"
mkdir -p "$STAGE"
ditto "$APP" "$STAGE/baucord.app"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
# hdiutil now and then fails on a busy runner ("Resource busy"): retry.
for ATTEMPT in 1 2 3; do
  if hdiutil create -volname "baucord" -srcfolder "$STAGE" -fs HFS+ \
       -format UDZO -imagekey zlib-level=9 -ov "$DMG"; then
    break
  fi
  [ "$ATTEMPT" -lt 3 ] || fail "hdiutil could not create the disk image"
  sleep 5
done

if [ -n "$SIGN_IDENTITY" ]; then
  codesign --force --timestamp --sign "$SIGN_IDENTITY" ${KEYCHAIN_ARGS[@]+"${KEYCHAIN_ARGS[@]}"} "$DMG"
fi
if [ "$NOTARIZE" = 1 ]; then
  notarize "$DMG"
  xcrun stapler staple "$DMG"
  xcrun stapler validate "$DMG"
  # What a user's Mac decides about the download and the app in it.
  spctl --assess --type open --context context:primary-signature --verbose=2 "$DMG"
  spctl --assess --type execute --verbose=2 "$APP"
fi

ls -la "$OUT_DIR"
