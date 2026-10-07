#!/bin/bash
# Xcode build phase (ios/project.yml, "Build the app with cargo"):
# build crates/ios for the platform Xcode is building and put the binary
# where Xcode expects the app's executable. Adapted from Slint's
# scripts/build_for_ios_with_cargo.bash (IOS-SPEC.md §11). Lines that
# start with "error:" show up as errors in Xcode.
#
# Knob, a build setting: BAUCORD_CARGO_FEATURES = cargo features instead
# of the default `app` (`hello` = the M0 smoke test). ios/run.sh always
# passes it on the xcodebuild command line, which beats xcconfig files;
# setting it in Config/Local.xcconfig only affects builds from Xcode's
# own window.
set -euo pipefail
# Whatever stops the script says where (in Xcode's build log), instead of
# `set -e` ending it without a word.
trap 'echo "error: build_with_cargo.sh stopped near line $LINENO (exit $?)"' ERR

# Xcode runs build phases with a minimal PATH. cargo lives in
# ~/.cargo/bin; Homebrew's cmake (libopus builds with it) in
# /opt/homebrew/bin on Apple silicon, /usr/local/bin on Intel.
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$PATH"

REPO="$(cd "$SRCROOT/.." && pwd)"
# rust-toolchain.toml is at the repository root: run from there so
# rustup picks the pinned toolchain (and its installed targets).
cd "$REPO"

case "${PLATFORM_NAME:-}" in
  iphoneos) TRIPLE=aarch64-apple-ios ;;
  iphonesimulator) TRIPLE=aarch64-apple-ios-sim ;;
  *) echo "error: unexpected PLATFORM_NAME '${PLATFORM_NAME:-}' (iphoneos or iphonesimulator)"; exit 1 ;;
esac
if [ "${ARCHS:-arm64}" != "arm64" ]; then
  echo "error: only arm64 is built (ARCHS=${ARCHS}); an Intel Mac's simulator is not supported (IOS-SPEC.md D5)"
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found — install Rust with rustup (ios/README.md)"
  exit 1
fi
if ! command -v cmake >/dev/null 2>&1; then
  echo "error: cmake not found — brew install cmake (libopus is built with it)"
  exit 1
fi
# grep without -q: an early exit could make rustup fail with EPIPE,
# which pipefail would report as a missing target.
if ! rustup target list --installed | grep -x "$TRIPLE" >/dev/null; then
  echo "error: the Rust target $TRIPLE is missing from this repository's toolchain — run: cd \"$REPO\" && rustup target add aarch64-apple-ios aarch64-apple-ios-sim"
  exit 1
fi

# The bundle's version must be the crate's: every platform ships the
# crates' version (SESSION-GUIDE.md §4). ios/run.sh writes it into
# Config/Version.xcconfig; a build from Xcode's window before that, or
# after a bump, would otherwise ship a stale number.
# awk reads the file itself and stops at the first match: no pipe into
# `head`, which pipefail would turn into a silent exit (IOS-SPEC.md D27).
CRATE_VERSION="$(awk -F'"' '/^version = "/ { print $2; exit }' "$REPO/crates/ios/Cargo.toml")"
if [ "${MARKETING_VERSION:-}" != "$CRATE_VERSION" ]; then
  echo "error: the app's version is '${MARKETING_VERSION:-}' but crates/ios/Cargo.toml says $CRATE_VERSION — run ios/run.sh (or ios/run.sh --open) once to refresh Config/Version.xcconfig"
  exit 1
fi

if [ "${CONFIGURATION:-Debug}" = "Release" ]; then
  PROFILE=release
  PROFILE_FLAG=--release
else
  PROFILE=debug
  PROFILE_FLAG=
fi

# Debug info in release builds too, so the archive gets a dSYM.
export CARGO_PROFILE_RELEASE_DEBUG="${CARGO_PROFILE_RELEASE_DEBUG:-1}"
# Xcode's builds get their own target directory: its environment
# differs from a terminal's, and sharing one would rebuild the whole
# tree every time the two alternate.
export CARGO_TARGET_DIR="$REPO/target/xcode"

FEATURES=()
if [ -n "${BAUCORD_CARGO_FEATURES:-}" ]; then
  FEATURES=(--no-default-features --features "$BAUCORD_CARGO_FEATURES")
fi

# `-p ios`, never the workspace: a workspace-wide build would unify the
# desktop's features into frontend (IOS-SPEC.md §2.1). The array idiom
# keeps an empty FEATURES legal under `set -u` in macOS's bash 3.2.
echo "cargo build -p ios --bin baucord --target $TRIPLE ${PROFILE_FLAG} ${FEATURES[*]:-}"
cargo build -p ios --bin baucord --target "$TRIPLE" $PROFILE_FLAG ${FEATURES[@]+"${FEATURES[@]}"}

BIN="$CARGO_TARGET_DIR/$TRIPLE/$PROFILE/baucord"
DEST="$TARGET_BUILD_DIR/$EXECUTABLE_PATH"
mkdir -p "$(dirname "$DEST")"
cp "$BIN" "$DEST"

# A dSYM where the configuration asks for one (Release: archives, crash
# reports); Xcode only makes them for what it compiles. Debug builds
# skip it — dsymutil over a debug build costs time on every run.
if [ "${DEBUG_INFORMATION_FORMAT:-}" = "dwarf-with-dsym" ] \
   && [ -n "${DWARF_DSYM_FOLDER_PATH:-}" ] && [ -n "${DWARF_DSYM_FILE_NAME:-}" ]; then
  mkdir -p "$DWARF_DSYM_FOLDER_PATH"
  dsymutil "$DEST" -o "$DWARF_DSYM_FOLDER_PATH/$DWARF_DSYM_FILE_NAME"
fi

# Device builds: sign the fresh executable now (Slint's script does the
# same); Xcode signs the bundle afterwards. Skipped for the simulator
# and for unsigned archives (CODE_SIGNING_ALLOWED=NO).
if [ "$PLATFORM_NAME" = "iphoneos" ] && [ "${CODE_SIGNING_ALLOWED:-YES}" != "NO" ] \
   && [ -n "${EXPANDED_CODE_SIGN_IDENTITY:-}" ]; then
  ENTITLEMENTS="${TARGET_TEMP_DIR:-}/${PRODUCT_NAME:-baucord}.app.xcent"
  if [ -s "$ENTITLEMENTS" ]; then
    codesign --force --sign "$EXPANDED_CODE_SIGN_IDENTITY" --entitlements "$ENTITLEMENTS" "$DEST"
  else
    codesign --force --sign "$EXPANDED_CODE_SIGN_IDENTITY" "$DEST"
  fi
fi
