#!/bin/bash
# `cargo run` for the iOS app (IOS-SPEC.md §11): generate the Xcode
# project, build, install, launch, and stream the app's log — in the
# simulator by default, on a connected iPhone with --device.
#
#   ios/run.sh                     simulator (a booted iPhone, else the newest), Debug
#   ios/run.sh --device            the iPhone on USB or the network (needs Config/Local.xcconfig)
#   ios/run.sh --release           the Release configuration
#   ios/run.sh --hello             the M0 smoke test instead of the client
#   ios/run.sh --sim "iPhone 17"   a specific simulator, by name
#   ios/run.sh --no-build          reinstall and relaunch the last build
#   ios/run.sh --no-log            launch and return (no log stream)
#   ios/run.sh --open              generate the project and open it in Xcode
#
# Ctrl+C ends the log stream (and, in the simulator, the app).
# Written for macOS's bash 3.2.
set -euo pipefail

IOS="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$IOS/.." && pwd)"

DEVICE=0
CONFIG=Debug
FEATURES=""
SIM_NAME=""
BUILD=1
STREAM_LOG=1
OPEN=0

die() { echo "run.sh: $*" >&2; exit 1; }
say() { echo ">> $*"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --device) DEVICE=1 ;;
    --release) CONFIG=Release ;;
    --hello) FEATURES=hello ;;
    --sim) [ $# -ge 2 ] || die "--sim needs a simulator name"; SIM_NAME="$2"; shift ;;
    --no-build) BUILD=0 ;;
    --no-log) STREAM_LOG=0 ;;
    --open) OPEN=1 ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
  shift
done

# -- Tools --------------------------------------------------------------
command -v xcodebuild >/dev/null 2>&1 || die "Xcode is not installed (App Store), or xcode-select points at the Command Line Tools: sudo xcode-select -s /Applications/Xcode.app"
command -v xcodegen >/dev/null 2>&1 || die "xcodegen not found: brew install xcodegen"
command -v python3 >/dev/null 2>&1 || die "python3 not found (it comes with Xcode's command line tools: xcode-select --install)"
if [ $BUILD -eq 1 ]; then
  command -v cargo >/dev/null 2>&1 || die "cargo not found: install Rust with rustup (ios/README.md)"
  command -v cmake >/dev/null 2>&1 || die "cmake not found: brew install cmake"
  for t in aarch64-apple-ios aarch64-apple-ios-sim; do
    # grep without -q: an early exit could make rustup fail with EPIPE,
    # which pipefail would report as a missing target.
    (cd "$REPO" && rustup target list --installed | grep -x "$t" >/dev/null) \
      || die "Rust target $t missing for this repository's toolchain: cd \"$REPO\" && rustup target add aarch64-apple-ios aarch64-apple-ios-sim"
  done
fi
if [ $DEVICE -eq 1 ] && [ ! -f "$IOS/Config/Local.xcconfig" ]; then
  die "no Config/Local.xcconfig: copy Config/Local.xcconfig.example and set DEVELOPMENT_TEAM (your Apple ID's team in Xcode → Settings → Accounts)"
fi

# -- Project ------------------------------------------------------------
say "xcodegen generate"
(cd "$IOS" && mkdir -p Generated && xcodegen generate --quiet)
if [ $OPEN -eq 1 ]; then
  open "$IOS/baucord.xcodeproj"
  exit 0
fi

# -- Destination --------------------------------------------------------
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [ $DEVICE -eq 1 ]; then
  # The first paired iPhone that is reachable (USB or network); iPads
  # report the platform "iOS" too, hence the device type.
  xcrun devicectl list devices --json-output "$TMP/devices.json" >/dev/null 2>&1 \
    || die "xcrun devicectl failed — is Xcode 26 or later installed?"
  TARGET_ID="$(python3 - "$TMP/devices.json" <<'PY'
import json, sys
devices = json.load(open(sys.argv[1])).get("result", {}).get("devices", [])
for d in devices:
    hw = d.get("hardwareProperties", {})
    conn = d.get("connectionProperties", {})
    if hw.get("platform") == "iOS" and hw.get("deviceType") == "iPhone" \
            and conn.get("pairingState") == "paired" \
            and conn.get("tunnelState") != "unavailable" and hw.get("udid"):
        print(hw["udid"])
        break
PY
)"
  [ -n "$TARGET_ID" ] || die "no iPhone found: connect it, unlock it, tap Trust, and turn on Developer Mode (Settings → Privacy & Security)"
  say "device $TARGET_ID"
  SDK=iphoneos
  DESTINATION="id=$TARGET_ID"
else
  xcrun simctl list devices available --json > "$TMP/sims.json"
  TARGET_ID="$(python3 - "$TMP/sims.json" "$SIM_NAME" <<'PY'
import json, re, sys
data = json.load(open(sys.argv[1]))["devices"]
wanted = sys.argv[2]
def version(runtime):
    m = re.search(r"iOS-(\d+)-(\d+)", runtime)
    return (int(m.group(1)), int(m.group(2))) if m else (0, 0)
phones = [
    (version(rt), d)
    for rt, ds in data.items() if "iOS" in rt
    for d in ds if d.get("isAvailable") and "iPhone" in d.get("name", "")
]
if wanted:
    phones = [p for p in phones if p[1]["name"] == wanted]
booted = [p for p in phones if p[1].get("state") == "Booted"]
pick = max(booted or phones, key=lambda p: p[0], default=None)
print(pick[1]["udid"] if pick else "")
PY
)"
  [ -n "$TARGET_ID" ] || die "no iPhone simulator${SIM_NAME:+ named \"$SIM_NAME\"}: add one in Xcode → Window → Devices and Simulators"
  say "simulator $TARGET_ID"
  xcrun simctl boot "$TARGET_ID" >/dev/null 2>&1 || true   # already booted is fine
  open -a Simulator --args -CurrentDeviceUDID "$TARGET_ID" || true
  SDK=iphonesimulator
  DESTINATION="id=$TARGET_ID"
fi

APP="$IOS/build/Build/Products/$CONFIG-$SDK/baucord.app"

# -- Build --------------------------------------------------------------
if [ $BUILD -eq 1 ]; then
  say "xcodebuild $CONFIG ($SDK${FEATURES:+, features $FEATURES})"
  ARGS=(-project "$IOS/baucord.xcodeproj" -scheme baucord -configuration "$CONFIG"
        -destination "$DESTINATION" -derivedDataPath "$IOS/build")
  # Signing: Xcode creates or updates the development profile itself,
  # registering a phone the team has not seen yet (the Apple ID must be
  # added in Xcode → Settings → Accounts).
  [ $DEVICE -eq 1 ] && ARGS+=(-allowProvisioningUpdates -allowProvisioningDeviceRegistration)
  # Read by scripts/build_with_cargo.sh. Always passed, so switching
  # between --hello and the app takes effect.
  ARGS+=("BAUCORD_CARGO_FEATURES=$FEATURES")
  xcodebuild "${ARGS[@]}" -quiet build || die "build failed (the cargo output is above)"
fi
[ -d "$APP" ] || die "no app at $APP — build first (drop --no-build)"

BUNDLE_ID="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP/Info.plist")"

# -- Install + launch ---------------------------------------------------
if [ $DEVICE -eq 1 ]; then
  say "installing on the iPhone"
  xcrun devicectl device install app --device "$TARGET_ID" "$APP" >/dev/null \
    || die "install failed (a free Apple ID allows 3 apps per device; Xcode's Devices window shows why)"
  LAUNCH=(xcrun devicectl device process launch --device "$TARGET_ID" --terminate-existing)
  [ $STREAM_LOG -eq 1 ] && LAUNCH+=(--console) && say "log (Ctrl+C to stop)"
  "${LAUNCH[@]}" "$BUNDLE_ID" || die "launch failed — the first time with a free Apple ID, trust the developer on the iPhone: Settings → General → VPN & Device Management"
else
  say "installing in the simulator"
  xcrun simctl install "$TARGET_ID" "$APP"
  if [ $STREAM_LOG -eq 1 ]; then
    say "log (Ctrl+C to stop)"
    xcrun simctl launch --console-pty --terminate-running-process "$TARGET_ID" "$BUNDLE_ID"
  else
    xcrun simctl launch --terminate-running-process "$TARGET_ID" "$BUNDLE_ID" >/dev/null
  fi
fi
