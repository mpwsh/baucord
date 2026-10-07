#!/bin/bash
# `cargo run` for the iOS app (IOS-SPEC.md §11): generate the Xcode
# project, build, install, launch, and stream the app's log — in the
# simulator by default, on a connected iPhone with --device.
#
#   ios/run.sh                     simulator (a booted iPhone, else the newest iOS), Debug
#   ios/run.sh --device            the iPhone on USB or the network (needs Config/Local.xcconfig)
#   ios/run.sh --release           the Release configuration
#   ios/run.sh --hello             the M0 smoke test instead of the client
#   ios/run.sh --os 18.1           an iPhone simulator on that iOS (any iOS 17 or later works)
#   ios/run.sh --sim "iPhone 16"   a specific simulator, by name or UDID (with --os: on that iOS)
#   ios/run.sh --no-build          reinstall and relaunch the last build
#   ios/run.sh --no-log            launch and return (no log stream)
#   ios/run.sh --open              generate the project and open it in Xcode
#   ios/run.sh --renderer skia-software   draw with Skia on the CPU (a diagnostic; also: software)
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
SIM_OS=""
BUILD=1
STREAM_LOG=1
OPEN=0
RENDERER=""

die() { echo "run.sh: $*" >&2; exit 1; }
say() { echo ">> $*"; }
# The first line of "$1", without a pipe: `cmd | head -n 1` under
# pipefail ends the script — silently — when cmd writes its next line
# after head has left (SIGPIPE, exit 141). `xcodebuild -version` did
# exactly that on the first Mac run (IOS-SPEC.md D27).
first_line() { local s="$1"; echo "${s%%$'\n'*}"; }
# Whatever stops the script says where: under `set -e` a failing command
# would otherwise end it without a word.
trap 'echo "run.sh: stopped near line $LINENO (exit $?)" >&2' ERR
# The teams of the "Apple Development" certificates in the keychain —
# each certificate's subject OU — one per line, sorted, unique. Needs
# $TMP. Every command reads its input to the end (no early-leaving
# reader under pipefail, D27).
signing_teams() {
  local pems f
  pems="$(security find-certificate -a -c "Apple Development" -p 2>/dev/null)" || return 0
  [ -n "$pems" ] || return 0
  printf '%s\n' "$pems" | awk -v dir="$TMP" '/BEGIN CERTIFICATE/ { n++ } n { print > (dir "/cert" n ".pem") }'
  for f in "$TMP"/cert*.pem; do
    [ -f "$f" ] || continue
    openssl x509 -in "$f" -noout -subject 2>/dev/null || true
  done | sed -n 's/.*OU *= *\([A-Z0-9]\{10\}\).*/\1/p' | sort -u
}

while [ $# -gt 0 ]; do
  case "$1" in
    --device) DEVICE=1 ;;
    --release) CONFIG=Release ;;
    --hello) FEATURES=hello ;;
    --sim) [ $# -ge 2 ] || die "--sim needs a simulator name or UDID"; SIM_NAME="$2"; shift ;;
    --os) [ $# -ge 2 ] || die "--os needs an iOS version, e.g. 18.1"; SIM_OS="$2"; shift ;;
    --no-build) BUILD=0 ;;
    --no-log) STREAM_LOG=0 ;;
    --open) OPEN=1 ;;
    --renderer)
      [ $# -ge 2 ] || die "--renderer needs a name: skia (the default), skia-software or software"
      case "$2" in
        skia|skia-software|software) RENDERER="$2" ;;
        *) die "--renderer: skia, skia-software or software (not $2)" ;;
      esac
      shift ;;
    -h|--help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
  shift
done

# -- Tools --------------------------------------------------------------
command -v xcodebuild >/dev/null 2>&1 || die "Xcode is not installed (App Store), or xcode-select points at the Command Line Tools: sudo xcode-select -s /Applications/Xcode.app"
XCODE_OUT="$(xcodebuild -version 2>&1)" \
  || die "xcodebuild -version failed — after an install or update Xcode may need its first-launch setup (sudo xcodebuild -runFirstLaunch) or the licence (sudo xcodebuild -license). It said: $XCODE_OUT"
XCODE="$(first_line "$XCODE_OUT")"
say "$XCODE"
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
# The phone's signing team is settled below, once a phone is found
# (Config/Local.xcconfig, else the keychain's development certificate).

# -- Version ------------------------------------------------------------
# The crates' version — the public crates move together and every
# platform ships that number (SESSION-GUIDE.md §4) — and a build number
# that grows with every commit (TestFlight refuses one it has seen).
# Base.xcconfig includes the file; it is rewritten on every run, so
# builds from Xcode's window get it too, and the cargo phase refuses to
# build with a version that disagrees with the crate's.
VERSION="$(awk -F'"' '/^version = "/ { print $2; exit }' "$REPO/crates/ios/Cargo.toml")"
[ -n "$VERSION" ] || die "no version line in crates/ios/Cargo.toml"
BUILD_NUMBER="$(git -C "$REPO" rev-list --count HEAD 2>/dev/null || echo 1)"
printf '%s\n' \
  "// Written by ios/run.sh from crates/ios/Cargo.toml and the commit count. Do not edit." \
  "MARKETING_VERSION = $VERSION" \
  "CURRENT_PROJECT_VERSION = $BUILD_NUMBER" > "$IOS/Config/Version.xcconfig"
say "version $VERSION (build $BUILD_NUMBER)"

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
    || die "xcrun devicectl failed — is Xcode 15 or later installed and selected (xcode-select -p)?"
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
  xcrun simctl list devices available --json > "$TMP/sims.json" \
    || die "xcrun simctl list failed (its message is above) — is Xcode's simulator set up? Open Xcode once"
  PICK="$(python3 - "$TMP/sims.json" "$SIM_NAME" "$SIM_OS" <<'PY'
import json, re, sys
data = json.load(open(sys.argv[1]))["devices"]
wanted, wanted_os = sys.argv[2], sys.argv[3]
def version(runtime):
    m = re.search(r"iOS-(\d+)-(\d+)", runtime)
    return (int(m.group(1)), int(m.group(2))) if m else (0, 0)
phones = [
    (version(rt), d)
    for rt, ds in data.items() if "iOS" in rt
    for d in ds if d.get("isAvailable") and "iPhone" in d.get("name", "")
]
if wanted_os:
    # "18.1" → that runtime; "18" → any 18.x.
    parts = tuple(int(p) for p in wanted_os.split(".")[:2] if p.isdigit())
    phones = [p for p in phones if p[0][:len(parts)] == parts]
if wanted:
    phones = [p for p in phones if wanted in (p[1]["name"], p[1]["udid"])]
booted = [p for p in phones if p[1].get("state") == "Booted"]
pick = max(booted or phones, key=lambda p: p[0], default=None)
if pick:
    print("%s|%s (iOS %d.%d)" % (pick[1]["udid"], pick[1]["name"], pick[0][0], pick[0][1]))
PY
)"
  [ -n "$PICK" ] || die "no iPhone simulator${SIM_NAME:+ \"$SIM_NAME\"}${SIM_OS:+ on iOS $SIM_OS}: add one in Xcode → Window → Devices and Simulators (xcrun simctl list devices shows what is there)"
  TARGET_ID="${PICK%%|*}"
  say "simulator ${PICK#*|} $TARGET_ID"
  xcrun simctl boot "$TARGET_ID" >/dev/null 2>&1 || true   # already booted is fine
  open -a Simulator --args -CurrentDeviceUDID "$TARGET_ID" || true
  SDK=iphonesimulator
  DESTINATION="id=$TARGET_ID"
fi

# -- Signing (the phone) ------------------------------------------------
# The team whose certificate signs the app. Without Config/Local.xcconfig,
# or with the example's placeholder still in it, Xcode fails late with
# "No Account for Team" and "No profiles for 'dev.baucord'" — so check
# here, and when the keychain holds the development certificate of
# exactly one team, use it and write it down (IOS-SPEC.md D38).
if [ $DEVICE -eq 1 ]; then
  LOCAL="$IOS/Config/Local.xcconfig"
  PLACEHOLDER="$(awk -F= '/^[[:space:]]*DEVELOPMENT_TEAM[[:space:]]*=/ { gsub(/[[:space:]]/, "", $2); print $2; exit }' "$IOS/Config/Local.xcconfig.example")"
  TEAM=""
  if [ -f "$LOCAL" ]; then
    TEAM="$(awk -F= '/^[[:space:]]*DEVELOPMENT_TEAM[[:space:]]*=/ { gsub(/[[:space:]]/, "", $2); print $2; exit }' "$LOCAL")"
  fi
  if [ -z "$TEAM" ] || [ "$TEAM" = "$PLACEHOLDER" ]; then
    if [ -z "$TEAM" ]; then
      WHY="ios/Config/Local.xcconfig sets no DEVELOPMENT_TEAM"
    else
      WHY="ios/Config/Local.xcconfig still has the example team ($TEAM)"
    fi
    TEAMS="$(signing_teams)"
    COUNT="$(printf '%s\n' "$TEAMS" | grep -c . || true)"
    if [ "$COUNT" -eq 0 ]; then
      die "$WHY, and the keychain has no Apple Development certificate to take a team from. Xcode → Settings → Accounts → + → your Apple ID, then Manage Certificates → + → Apple Development; run this again"
    elif [ "$COUNT" -gt 1 ]; then
      die "$WHY, and your Apple Development certificates belong to $COUNT teams: $(printf '%s\n' "$TEAMS" | paste -s -d ' ' -). Set DEVELOPMENT_TEAM = <one of them> in ios/Config/Local.xcconfig"
    fi
    TEAM="$TEAMS"
    SRC="$LOCAL"
    [ -f "$SRC" ] || SRC="$IOS/Config/Local.xcconfig.example"
    awk -v team="$TEAM" '
      /^[[:space:]]*DEVELOPMENT_TEAM[[:space:]]*=/ && !done { print "DEVELOPMENT_TEAM = " team; done = 1; next }
      { print }
      END { if (!done) print "DEVELOPMENT_TEAM = " team }' "$SRC" > "$TMP/local.xcconfig"
    cat "$TMP/local.xcconfig" > "$LOCAL"
    say "signing team $TEAM, from your Apple Development certificate (written to ios/Config/Local.xcconfig)"
  else
    say "signing team $TEAM"
  fi
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
  if ! xcodebuild "${ARGS[@]}" -quiet build 2>&1 | tee "$TMP/xcodebuild.log"; then
    # Since Xcode 15 the iOS platform matching Xcode's own SDK must be
    # installed before it builds for *any* iOS destination: older
    # simulator runtimes do not count, and the simulator picked above is
    # then not even listed (IOS-SPEC.md D26).
    if grep "first download and install the platform" "$TMP/xcodebuild.log" >/dev/null; then
      MISSING="$(first_line "$(sed -n 's/.*error:\(iOS [0-9.]*\) is not installed.*/\1/p' "$TMP/xcodebuild.log")")"
      die "$XCODE builds against ${MISSING:-a newer iOS} and needs that platform installed before it builds for any iPhone or simulator; older simulators do not count. Install it once — xcodebuild -downloadPlatform iOS (or Xcode → Settings → Components); it may fetch a newer simulator of the same major version, which is Apple's pairing for this Xcode — then run this again."
    fi
    # Signing, on the phone: the usual first-run stops, in plain words.
    if [ $DEVICE -eq 1 ]; then
      if grep "No Account for Team" "$TMP/xcodebuild.log" >/dev/null; then
        die "Xcode is not signed in to team $TEAM: Xcode → Settings → Accounts → + → the Apple ID whose certificate this is; then run this again"
      fi
      if grep "cannot be registered to your development team" "$TMP/xcodebuild.log" >/dev/null; then
        die "the bundle id is taken by another team: set BAUCORD_BUNDLE_ID = dev.baucord.<yourname> in ios/Config/Local.xcconfig and run this again"
      fi
      if grep -E "Developer Mode" "$TMP/xcodebuild.log" >/dev/null; then
        die "the iPhone needs Developer Mode: Settings → Privacy & Security → Developer Mode → on → restart (it is listed once the phone has been connected to Xcode — Window → Devices and Simulators)"
      fi
    fi
    die "xcodebuild failed (its output is above)"
  fi
fi
[ -d "$APP" ] || die "no app at $APP — build first (drop --no-build)"

BUNDLE_ID="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP/Info.plist")"

# -- Install + launch ---------------------------------------------------
# Diagnostic switches reach the app as environment variables of this
# launch only (IOS-SPEC.md §14 D40); opened from the home screen it runs
# as normal. The values are fixed words, so the JSON needs no escaping.
ENV_JSON=""
add_env() {
  ENV_JSON="${ENV_JSON:+$ENV_JSON, }\"$1\": \"$2\""
  export "SIMCTL_CHILD_$1=$2"   # what simctl hands the app
}
[ -n "$RENDERER" ] && add_env BAUCORD_RENDERER "$RENDERER"
[ -n "$ENV_JSON" ] && say "diagnostic launch: {$ENV_JSON}"

if [ $DEVICE -eq 1 ]; then
  say "installing on the iPhone"
  xcrun devicectl device install app --device "$TARGET_ID" "$APP" >/dev/null \
    || die "install failed (its message is above). Usual causes: Developer Mode off (Settings → Privacy & Security → Developer Mode, listed once the phone has been connected to Xcode), or a free Apple ID's limit of 3 apps per device; Xcode's Devices window shows more"
  LAUNCH=(xcrun devicectl device process launch --device "$TARGET_ID" --terminate-existing)
  [ -n "$ENV_JSON" ] && LAUNCH+=(--environment-variables "{$ENV_JSON}")
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
