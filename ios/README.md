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
ios/run.sh --hello        # the smoke test in the simulator (the toolchain alone)
ios/run.sh                # the client in the simulator
ios/run.sh --device       # the client on your iPhone
ios/run.sh --os 18.1      # the client on an iOS 18.1 iPhone simulator
ios/run.sh --help         # --release, --sim "iPhone 16" (or a UDID), --no-build, --no-log, --open
ios/run.sh --device --renderer skia-software   # a diagnostic: draw on the CPU (see §4)
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
simulator's microphone is the Mac's (macOS asks once). In voice on the
loudspeaker the app cancels echo with Apple's voice-processing unit
(IOS-SPEC D47); with headphones it does not need to.

To try **Attach** in the simulator: Photos comes with Apple's sample
pictures, and Files starts empty — drag a file from the Mac onto the
simulator window to put it there (a picture goes into Photos). Files
you save from a chat land in Files → On My iPhone → baucord →
Downloads. There is no Quit on iOS: close the app from the app
switcher (in the simulator Device → App Switcher, ⌃⇧⌘H, then swipe it
up).

## 3. Your iPhone

Two ways onto the phone, both over USB the first time. iOS only runs
apps signed for it, which is what decides between them:

| | 3a. Built on this Mac | 3b. Built by GitHub Actions |
|---|---|---|
| Apple account | a free Apple ID is enough | the paid Developer Program ($99/year) |
| How | `ios/run.sh --device` | publish a release (or run the workflow), install the `.ipa` |
| Lasts | 7 days with a free Apple ID, then run it again | a year (the profile's life) |
| Who | your own phones | any iPhone whose UDID is in the profile (up to 100 a year) |

### 3a. From this Mac (`run.sh --device`)

1. Xcode → Settings → Accounts → **+** → Apple ID. Then Manage
   Certificates → **+** → *Apple Development*, so the Mac has a
   signing certificate.
2. The signing team: `ios/run.sh --device` takes it from that
   certificate and writes it into `Config/Local.xcconfig` (made from
   the `.example` if missing) when the file has none or still has the
   example's `ABCDE12345`. To set it by hand, it is the certificate's
   "OU":
   ```sh
   security find-certificate -c "Apple Development" -p | openssl x509 -noout -subject
   ```
   (the 10 characters after `OU=`). A free Apple ID is a "Personal
   Team": the app stops opening after **7 days** and needs
   `ios/run.sh --device` again, and one Apple ID can have 3 such apps
   on a phone.
3. If Xcode says the bundle id `dev.baucord` is not available, set
   `BAUCORD_BUNDLE_ID = dev.baucord.<yourname>` in the same file.
4. Connect the iPhone by USB, unlock it, tap **Trust**. Open Xcode →
   Window → Devices and Simulators once with it connected: only then
   does the phone offer **Developer Mode** (Settings → Privacy &
   Security → Developer Mode → on → restart → confirm).
5. `ios/run.sh --device`. With a free Apple ID the first launch is
   refused until the phone trusts you as a developer: Settings →
   General → VPN & Device Management → your Apple ID → Trust. Run it
   again. `--device --release` installs an optimised build.

After the first USB run the phone also works over Wi-Fi.

If Xcode refuses the phone because of its iOS version (a phone on
iOS 26 or 27 and Xcode 16), update Xcode: 26.x needs macOS Sequoia
15.6 or later, 27 needs macOS Tahoe 26.6.

### 3b. From GitHub Actions (`.github/workflows/ios-release.yml`)

The workflow runs when a release is published, like the Android APK,
or by hand (Actions → ios-release → Run workflow). With the signing
secrets below it attaches a signed `baucord-ios-<tag>.ipa` to the
release; without them it only proves the tag builds and leaves an
unsigned `.ipa` as a workflow artifact, which no iPhone installs.

Once, in the Developer Program (developer.apple.com → Certificates,
IDs & Profiles):

1. **Identifiers → +** → App IDs → App → explicit bundle id
   `dev.baucord` (or your own — then add a repository *variable*
   `IOS_BUNDLE_ID` with it). No capabilities are needed.
2. **Devices → +**: each iPhone that should install it, by UDID. With
   the phone connected: Finder → the phone in the sidebar → click the
   line under its name until it shows the UDID → right-click → Copy.
3. A distribution certificate: Xcode → Settings → Accounts → Manage
   Certificates → **+** → *Apple Distribution*. Then Keychain Access →
   login → My Certificates → *Apple Distribution: …* (with its key
   underneath) → right-click → Export → `.p12`, with a password.
4. **Profiles → +** → *Ad Hoc* → the app id → that certificate → the
   devices → name it → download the `.mobileprovision`. A new device
   later means editing the profile (add it), downloading it again and
   replacing the secret.
5. Repository → Settings → Secrets and variables → Actions → *New
   repository secret*, three times:
   ```sh
   base64 -i baucord-distribution.p12 | pbcopy        # → IOS_CERTIFICATE_P12_BASE64
                                                      #   IOS_CERTIFICATE_PASSWORD: the .p12's password
   base64 -i baucord_Ad_Hoc.mobileprovision | pbcopy  # → IOS_PROFILE_BASE64
   ```

Then publish a release (`v1.2.0`, the crates' version — the workflow
checks), or run the workflow by hand to try it; a manual run leaves
the `.ipa` under the run's *Artifacts*. The first build takes a while
(the whole engine for iOS); run the workflow once by hand on the
default branch and later releases start from its cache.

**Installing the `.ipa` over USB** (an Ad Hoc build is not
development-signed, so Developer Mode should not be needed; if the
phone says it is, turn it on as in 3a, step 4):

- Xcode → Window → **Devices and Simulators** → the phone → under
  *Installed Apps* press **+** → choose `baucord-ios-<tag>.ipa`; or
  drag the file onto that list.
- Or **Apple Configurator** (free, Mac App Store): drag the `.ipa`
  onto the phone's tile.

A phone that is not in the profile refuses it ("unable to install");
add its UDID (steps 2 and 4). To give it to people without USB, the
next step is TestFlight (spec M5).

## 4. Logs

- Live: the stream `run.sh` shows (stderr), or Xcode's console.
- On the phone: **Files → On My iPhone → baucord → baucord.log** —
  the same format as the desktop log, rolled at 5 MB.
- From anyone's phone: **Settings → Debug → Send log to server** sends
  the end of it (up to 2 MB, once per 10 minutes) to the server they
  are connected to, which keeps it in its identity folder's `logs/`
  (`<name>-<id>-<time>.log`; the server's own log names the file). The
  same tab shows the last 100 lines and can clear the log.

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
bar, switches between the whole picture and filling the screen. Pinch
to zoom in (in the simulator hold ⌥ and drag for two fingers), drag
to move around, tap the "2.4×" chip top-right to go back. The bar's
first two buttons are your own microphone and headphones. The screen
stays on while the stream page is open.

Lines worth looking for: `VideoToolbox: decoding H.264 to BGRA` and
`VideoToolbox: first picture WxH` when you watch a share;
`photo pick: loading …` and `picked …` when you attach;
`lifecycle: background` and `host: app hidden` when you leave the app
(in voice it keeps talking, and stops drawing); `safe area: top 20
left 0 bottom 0 right 0 pt` — what `UIKit` says the system bars cover,
which the pages keep clear of (on an iPhone SE, 20 at the top).

**Text drawn half-wrong now and then** (glyphs with their top missing
— IOS-SPEC.md D40): launch with `--renderer skia-software` (Skia on
the CPU) and use the app as before. If it never happens that way, it
is the GPU path; if it still does, try `--renderer software` (Slint's
own renderer). The log's first lines say `renderer: … (a diagnostic
launch)`. Only that launch is affected: opened from the home screen,
the app draws on the GPU as normal.

**Debug or Release on the phone:** `--device` builds Debug, with the
pixel and audio code optimised all the same; `--device --release`
is what users get, and the one to judge speed and battery by.

## 5. The version

The app's version is the crates' — `crates/ios/Cargo.toml`, which moves
together with every other platform's (1.2.0 now). `run.sh` writes it
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
