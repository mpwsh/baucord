# baucord for iOS

The iPhone app: the same Rust engine and Slint UI as the desktop and
Android clients, in an Xcode project that has no Swift in it. The app
is `crates/ios`; this folder is only what Xcode needs to bundle,
sign and install it. The plan and every decision are in
[`IOS-SPEC.md`](../IOS-SPEC.md).

## 1. One-time setup (on the Mac)

| what | how | why |
|---|---|---|
| Xcode 26 or later | App Store; open it once to install the iOS platform and accept the licence | the SDK, the simulator, signing, `xcodebuild`, `devicectl` |
| Command-line tools selected | `sudo xcode-select -s /Applications/Xcode.app` | so `xcodebuild` is Xcode's, not the bare CLT's |
| Homebrew | <https://brew.sh> | for the two tools below |
| XcodeGen | `brew install xcodegen` | writes `baucord.xcodeproj` from `project.yml` |
| CMake | `brew install cmake` | libopus (the voice codec) builds with it |
| Rust | <https://rustup.rs> | |
| the iOS targets, **for this repo's toolchain** | `cd <repo> && rustup target add aarch64-apple-ios aarch64-apple-ios-sim` | `rust-toolchain.toml` pins the toolchain; running inside the repo adds them to that one |

An Apple-silicon Mac is assumed (the simulator runs arm64 builds).

## 2. Run it

```sh
ios/run.sh --hello        # first: the M0 smoke test in the simulator
ios/run.sh                # the client in the simulator
ios/run.sh --device       # the client on your iPhone
ios/run.sh --help         # --release, --sim "iPhone 17", --no-build, --no-log, --open
```

If the shell says *permission denied*, the files lost their executable
bit on the way (a commit made on Windows does that): run `bash
ios/run.sh …`, or fix it once in git with `git update-index --chmod=+x
ios/run.sh ios/scripts/*.sh`. Xcode's build phases call their scripts
through `bash`, so they never need the bit.

`run.sh` regenerates the project, builds (Xcode calls cargo through
`scripts/build_with_cargo.sh`), installs, launches and streams the
app's log until Ctrl+C. The first build downloads a prebuilt Skia and
compiles the whole engine for iOS: several minutes. `ios/run.sh --open`
opens the project in Xcode instead, where ⌘R does the same and the
debugger works.

In the simulator, a server on this Mac is `127.0.0.1:51000`, and the
simulator's microphone is the Mac's (macOS asks once).

## 3. Your iPhone

1. Copy `Config/Local.xcconfig.example` to `Config/Local.xcconfig` and
   set `DEVELOPMENT_TEAM`: Xcode → Settings → Accounts → add your
   Apple ID → the team ID. A free Apple ID is enough ("Personal
   Team"): the app then stops opening after **7 days** and needs
   `ios/run.sh --device` again, and one Apple ID can have 3 such apps
   on a phone. The paid Developer Program ($99/year) gives one-year
   profiles and TestFlight (IOS-SPEC.md §11).
2. If Xcode says the bundle id `dev.baucord` is not available, set
   `BAUCORD_BUNDLE_ID = dev.baucord.<yourname>` in the same file.
3. On the iPhone: connect it, unlock it, tap **Trust**; turn on
   **Developer Mode** (Settings → Privacy & Security, then restart).
4. `ios/run.sh --device`. The first launch with a free Apple ID is
   refused until you trust yourself as a developer on the phone:
   Settings → General → VPN & Device Management → your Apple ID →
   Trust. Run it again.

After the first USB run the phone also works over Wi-Fi.

## 4. Logs

- Live: the stream `run.sh` shows (stderr), or Xcode's console.
- On the phone: **Files → On My iPhone → baucord → baucord.log** —
  what users send; the same format as the desktop log, rolled at
  5 MB.

## 5. What is here

```
project.yml                  XcodeGen: target, Info.plist keys, build phases
Config/Base.xcconfig         shared settings; includes Config/Local.xcconfig
Config/Local.xcconfig.example
scripts/build_with_cargo.sh  the cargo build phase (Xcode runs it)
scripts/render_icon.sh       the icon phase: ../assets/icons/icon.png → AppIcon
Resources/Assets.xcassets    the icon (rendered) and the launch colour
run.sh                       generate, build, install, launch, log
```

Generated, not committed: `baucord.xcodeproj/`, `Generated/Info.plist`,
the rendered icon, `build/` (run.sh's derived data). Cargo's output
for Xcode builds is `target/xcode/` at the repo root.
