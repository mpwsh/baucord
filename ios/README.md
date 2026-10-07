# baucord for iOS

The iPhone app: the same Rust engine and Slint UI as the desktop and
Android clients, in an Xcode project that has no Swift in it. The app
is `crates/ios`; this folder is only what Xcode needs to bundle,
sign and install it. The plan and every decision are in
[`IOS-SPEC.md`](../IOS-SPEC.md).

## 1. One-time setup (on the Mac)

| what | how | why |
|---|---|---|
| Xcode 16 or later | App Store; open it once to install the iOS platform and accept the licence | the SDK, the simulator, signing, `xcodebuild`, `devicectl`. Nothing in the project needs a newer one for the simulator. The phone may: Xcode refuses to install on an iPhone whose iOS it does not know yet (update Xcode then). TestFlight uploads need Xcode 26 or later |
| Command-line tools selected | `sudo xcode-select -s /Applications/Xcode.app` | so `xcodebuild` is Xcode's, not the bare CLT's |
| **Xcode's own iOS platform** | `xcodebuild -downloadPlatform iOS`, or Xcode → Settings → Components | Xcode 15+ builds for no iOS destination — simulator included — until the platform matching its SDK is installed (Xcode 16.2 → iOS 18.2). An older simulator (18.1) does not count; it runs the app once the platform is there. `run.sh` says so when it happens |
| Homebrew | <https://brew.sh> | for the two tools below |
| XcodeGen | `brew install xcodegen` | writes `baucord.xcodeproj` from `project.yml` |
| CMake | `brew install cmake` | libopus (the voice codec) builds with it |
| Rust | <https://rustup.rs> | |
| the iOS targets, **for this repo's toolchain** | `cd <repo> && rustup target add aarch64-apple-ios aarch64-apple-ios-sim` | `rust-toolchain.toml` pins the toolchain; running inside the repo adds them to that one |

An Apple-silicon Mac is assumed (the simulator runs arm64 builds). Any
iPhone simulator on **iOS 17 or later** runs the app — the iOS 18.1 one
included.

## 2. Run it

```sh
ios/run.sh --hello        # first: the M0 smoke test in the simulator
ios/run.sh                # the client in the simulator
ios/run.sh --device       # the client on your iPhone
ios/run.sh --os 18.1      # the client on an iOS 18.1 iPhone simulator
ios/run.sh --help         # --release, --sim "iPhone 16" (or a UDID), --no-build, --no-log, --open
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

Without `--os` or `--sim`, `run.sh` uses a booted iPhone simulator, else
the newest iOS installed; it prints which (`>> simulator iPhone 16 (iOS
18.1) …`). `xcrun simctl list devices` lists them all.

In the simulator, a server on this Mac is `127.0.0.1:51000`, and the
simulator's microphone is the Mac's (macOS asks once). There is no
echo cancellation on iOS yet: use headphones in voice.

To try **Attach** in the simulator: Photos comes with Apple's sample
pictures, and Files starts empty — drag a file from the Mac onto the
simulator window to put it there (a picture goes into Photos). Files
you save from a chat land in Files → On My iPhone → baucord →
Downloads. There is no Quit on iOS: close the app from the app
switcher (in the simulator Device → App Switcher, ⌃⇧⌘H, then swipe it
up).

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

Normal in the **simulator's** log, and not errors of ours (IOS-SPEC.md
D28): wgpu's `Missing downlevel flags …` with a capabilities dump and
`MAPPABLE_PRIMARY_BUFFERS enabled on a discrete gpu` (the simulator's
GPU is a reduced one), and now and then Skia's `Compilation took
longer than 1000 ms` on a first launch (a shader compiled late; the
next frame draws it). They mean the app is drawing on the GPU. winit's
iOS warnings (`processing non RedrawRequested event …`, `… is ignored
on iOS`) are filtered out of the log (D32).

Watching a stream: turn the phone (⌘← / ⌘→ in the simulator) for
the big picture; in landscape a double-tap, or the zoom button in the
bar, switches between the whole picture and filling the screen.

Lines worth looking for: `VideoToolbox: decoding H.264 to BGRA` and
`VideoToolbox: first picture WxH` when you watch a share;
`photo pick: loading …` and `picked …` when you attach;
`lifecycle: background` when you leave the app.

## 5. The version

The app's version is the crates' — `crates/ios/Cargo.toml`, which moves
together with every other platform's (1.1.7 now). `run.sh` writes it
into `Config/Version.xcconfig` on every run, with the commit count as
the build number; the cargo build phase stops with an error if the two
ever disagree (a bump, then a build from Xcode's window before
`run.sh`: run `ios/run.sh --open` once).

## 6. What is here

```
project.yml                  XcodeGen: target, Info.plist keys, build phases
Config/Base.xcconfig         shared settings; includes Version.xcconfig and Local.xcconfig
Config/Local.xcconfig.example
scripts/build_with_cargo.sh  the cargo build phase (Xcode runs it)
scripts/render_icon.sh       the icon phase: ../assets/icons/icon.png → AppIcon
Resources/Assets.xcassets    the icon (rendered) and the launch colour
run.sh                       generate, build, install, launch, log
```

Generated, not committed: `baucord.xcodeproj/`, `Generated/Info.plist`,
`Config/Version.xcconfig`, the rendered icon, `build/` (run.sh's
derived data). Cargo's output
for Xcode builds is `target/xcode/` at the repo root.
