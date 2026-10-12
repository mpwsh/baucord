# baucord — Android technical spec

> **Status (2026-10-09):** 1.2.0, sideloaded and on the website (D55);
> Google Play is next (PLAY-STORE.md). §0–§13 describe the client as
> it is now; §14 holds every decision taken since the first draft
> (D10…D66, the newest at the top of its second table), each with its
> reason. What is left: `PRODUCTION-PLAN.md` (the MOB items). Start
> from `START-HERE.md`.

## 0. Purpose and scope

An Android client for the baucord server, running the **same Rust
engine** and the **same Slint UI** as the desktop: voice with the
platform's echo cancellation, General and DMs, files, viewing screen
shares, the Settings page, notifications while connected. It is
distributed as a **sideloaded APK** (and, next, through Google Play),
for arm64 phones on Android 10 or later.

Not on Android, by choice or not yet: sharing the phone's screen (no
encoder; the button is hidden), the admin panel (a desktop window;
the slash commands and `/reports` stand in), keybinds and
push-to-talk, an updater (an APK installs over the last one; Play
will update it), push notifications (PRODUCTION-PLAN PRD-1), tablet
layouts, 32-bit ARM.

## 1. Decisions of the first draft

| # | decision | why |
|---|---|---|
| D1 | UI in Slint via `backend-android-activity-06` | one UI codebase; `frontend` glue shared with desktop; the team's toolkit |
| D2 | `NativeActivity` (Slint's `native-activity` feature), not `GameActivity` — subclassed since D30 (`BaucordActivity`) | no Kotlin Activity at all; the platform supplies the Activity class |
| D3 | v1 = voice + chat — since grown: stream viewing (D23), files (D30), notifications (D52) | as agreed |
| D4 | Sideloaded APK, signed with our own key — the same key signs the Play bundle (D62) | no Play review; Play's API-36 / 16 KB rules still followed so a store build is a config change later |
| D5 | The platform language kept to the irreducible minimum: three Java classes (§10.1), the manifest, Gradle — Java, not Kotlin (D13, D30) | everything else is Rust calling the platform over JNI |
| D6 | `minSdk 29` (Android 10), `targetSdk 36`, `compileSdk 36` | AAudio's floor is 26; 29 is where permissions and audio behave consistently; 36 is current |
| D7 | Only `arm64-v8a` on devices, `x86_64` for the emulator | 64-bit phones since Android 10; halves build time |
| D8 | The Rust engine lives once per process, not per Activity — *not done*: the process ends with the Activity (D11), so whatever destroys the Activity ends the call (MOB-8) | Android recreates Activities; the connection must not follow them |
| D9 | The desktop's Slint glue is extracted into a shared `frontend` crate before any Android code depends on it — done in 1.1.6 (§5) | the only way to get parity without a second 8k-line host |

Open: whether RNNoise stays on under the platform's own noise
suppression (§6.4). It is on, as on the desktop.

## 2. Where things live

```
baucord/
  crates/
    android/                  the cdylib: lib.rs (android_main), java.rs (every
                              JNI call), host.rs (the frontend's Hooks), logging.rs
                              (logcat + baucord.log), hello.rs (the smoke test)
    frontend/                 the UI logic every host runs (§5)
    audio/src/native/         aaudio.rs, android.rs, pump.rs: the call path (§6)
    video/src/mediacodec.rs   stream decoding (D23, D57, D59)
  android/                    the Gradle project (no Rust in here)
    settings.gradle.kts, build.gradle.kts   AGP 8.13.0
    gradle.properties         cargoTargets / cargoFeatures knobs
    gradle/ gradlew gradlew.bat              Gradle 8.14.3 wrapper
    app/build.gradle.kts      runs cargo-ndk before pre<Variant>Build; renders the
                              launcher icon (D36)
    app/src/main/AndroidManifest.xml
    app/src/main/java/dev/baucord/   BaucordActivity, VoiceService, Notices (§10.1)
    app/src/main/res/         theme, strings, the status-bar icon (D52)
    app/src/main/jniLibs/     written by cargo-ndk, git-ignored
    run.ps1, run.cmd          build, install, launch, logcat (§11)
    notification-icon.py      cuts the status-bar silhouette from the icon (D52)
  .github/workflows/android-release.yml   the signed APK and AAB on a published release (D62)
```

The Rust side of Android is a normal workspace member (`crates/android`)
so `cargo ndk … build -p android` and every rule in the session guide
apply to it unchanged. The Gradle project sits beside `crates/` because
it is not Rust and Cargo must not see it; it consumes what cargo-ndk
drops into `jniLibs`.

### 2.1 Workspace Cargo.toml

- `members = ["crates/*"]` picks the crate up; `resolver = "2"` is
  set (a virtual workspace without it falls back to resolver 1, which
  activates features for dependencies of every target at once).
- The `[workspace.dependencies] slint` entry (`default-features =
  false`, `std`, `backend-winit`, `renderer-skia`, `renderer-software`,
  `compat-1-2`) serves Android as it is. `i-slint-backend-winit` and
  `i-slint-renderer-femtovg` are `cfg(not(target_os = "android"))`
  dependencies of the selector and of `slint` itself, so they resolve
  to nothing on the Android target (`cargo tree -p android --target
  aarch64-linux-android` shows no winit, femtovg, softbuffer or wgpu;
  D12, D38). The Android backend brings the Skia renderer itself;
  `android` only adds `backend-android-activity-06`.
- **Always build the Android crate with `-p android`.** Its `frontend`
  dependency has `default-features = false`; a workspace-wide build
  (`--workspace`, or `-p desktop -p android`) would unify
  `frontend/desktop` back in and pull arboard, global-hotkey, rfd and
  the rest for the phone.

### 2.2 The library name

The crate is called `android` but the library must not be
`libandroid.so` — that is the NDK's own library and the loader would
pick the system one. `crates/android/Cargo.toml`:

```toml
[lib]
name = "baucord_android"
crate-type = ["cdylib"]
```

and the manifest's `android.app.lib_name` meta-data is
`baucord_android`.

## 3. Toolchain: what to install, from where

Everything below is what the Rust engine, Slint's Android backend and
Gradle need; nothing else. android/README.md §1 is the same list as a
checklist for a Windows machine.

| what | where | notes |
|---|---|---|
| Android Studio | <https://developer.android.com/studio> | brings the SDK Manager, the emulator, `adb` (its JBR is a JDK 25 — not usable for Gradle 8, see below) |
| SDK Platform **36** (Android 16) | SDK Manager → SDK Platforms | `android.jar` for `compileSdk 36`; Slint's backend compiles its Java helper against it |
| Build-Tools **36.x** and **35.0.0** | SDK Manager → SDK Tools | `d8` for the same helper; AGP 8.13's default build tools are 35.0.0 and it will not use 36 in their place (it downloads 35.0.0 itself once the SDK licences are accepted) |
| NDK **r29** (or r28+) | SDK Manager → SDK Tools → NDK (Side by side) | r28+ aligns ELF segments to 16 KB by default |
| CMake (+ Ninja) | SDK Manager → SDK Tools → CMake | `opusic-sys` builds libopus with cmake through the NDK toolchain file; it picks Ninja when `ninja` is on `PATH`, and on Windows Ninja is required (the Visual Studio generator cannot cross-compile). The SDK package ships both in `cmake/<ver>/bin` |
| Platform-Tools | SDK Manager → SDK Tools | `adb` for sideloading and `logcat` |
| Emulator + a **Google APIs x86_64, API 36** system image | SDK Manager → SDK Tools / Virtual Device Manager | optional; the emulator can pass the host microphone through |
| Rust stable + targets | `rustup target add aarch64-linux-android x86_64-linux-android` | Rust ≥ 1.85 (cpal's MSRV) |
| `cargo-ndk` | `cargo install cargo-ndk` (<https://github.com/bbqsrc/cargo-ndk>) | 4.x; finds the newest NDK under `$ANDROID_HOME/ndk` on its own, sets `CC_*`/`AR_*`/the linker per target, exports the cmake toolchain path |
| a JDK **21** | Temurin (`winget install EclipseAdoptium.Temurin.21.JDK`), not Android Studio's `jbr` | Gradle 8.14 runs on JDK 17–24 — the JBR shipped with 2026 Studio builds is 25 and fails the Kotlin DSL compile with just the version (`What went wrong: 25.0.3`) as the message. Slint's backend `build.rs` needs `javac` and build-tools' `d8`; both fine on 21 with build-tools ≥ 35 |
| Gradle + AGP | nothing to install: `android/gradlew` fetches Gradle 8.14.3, AGP 8.13.0 comes from Google's Maven | the first `gradlew` run downloads ~200 MB |

Environment (persist them; Gradle, cargo-ndk and Slint's `build.rs`
all read them):

```
ANDROID_HOME      = <sdk root>            (the SDK Manager's "Android SDK Location";
                                           Windows default %LOCALAPPDATA%\Android\Sdk —
                                           never the Studio install dir or its jbr)
ANDROID_SDK_ROOT  = same value            (the older name some tools still read)
ANDROID_NDK_HOME  = %ANDROID_HOME%\ndk\<r29 version>   (cargo-ndk, opusic-sys)
ANDROID_NDK       = same value            (skia-bindings, only if Skia builds from source)
JAVA_HOME         = <a JDK 21>              (NOT <android-studio>\jbr — see the table)
CMAKE_GENERATOR   = Ninja                 (only for `cargo ndk` by hand — the Gradle task sets
                                           it per build; a global value also flips the desktop
                                           build's libopus and breaks its cached VS build)
ANDROID_JAR       = %ANDROID_HOME%\platforms\android-36\android.jar
                                          (only for `cargo ndk` run by hand — the Gradle
                                           task sets it from compileSdk: cargo-ndk exports
                                           ANDROID_PLATFORM=29, which Slint's build script
                                           would otherwise read as the SDK platform)
PATH             += %ANDROID_HOME%\platform-tools ; %ANDROID_HOME%\cmake\<ver>\bin
```

`rust-toolchain.toml` pins 1.95, so add the targets to *that* toolchain
— run `rustup target add aarch64-linux-android x86_64-linux-android`
inside the repo (rustup follows the pin) or pass `--toolchain 1.95`.

On Windows set them as *user* environment variables (System Properties
→ Environment Variables), not in one PowerShell session: Android
Studio's Gradle daemon inherits the user environment, and
`%USERPROFILE%\.cargo\bin` must be on that `PATH` too for the
`cargo` Exec task. Restart Studio after changing them.

Verify: `cargo ndk --version`, `adb devices` (phone in developer mode
with USB debugging), `javac -version`, and `ls $ANDROID_HOME/platforms`
shows `android-36`.

Slint's Android backend draws with Skia: `skia-bindings` downloads a
prebuilt Skia for the target on the first build (network needed;
minutes). If it ever builds Skia from source instead, see §13.

## 4. Runtime architecture on Android

### 4.1 Process, activity, service

- The APK loads `libbaucord_android.so`; `android_main` (in
  `crates/android/src/lib.rs`) runs on the Activity's native thread and
  is the Slint event loop. It sets up the log and the panic hook (once
  per process), points `client::config` at the app's private folder,
  asks for the runtime permissions (§4.4), then runs
  `frontend::app::run()` — and ends the process when that returns
  (D11): the engine lives as long as the Activity, not beyond it (D8,
  MOB-8). The manifest's `configChanges` list keeps rotation, the
  keyboard, density, locale and the UI mode from recreating it.
- **Off screen** the process and the connection are kept by
  `VoiceService`, a foreground service started while connected (D34):
  of type `microphone` in a voice room and `specialUse` in the lobby
  (D63), with a partial wake lock in a room only, and a notification
  that says so. A reconnect pending in the background keeps it.
- What only feeds the screen — stats lines, GIFs, stream frames, the
  device poll — stops while the app is off screen and comes back with
  it (`set_host_visible`, D43).

### 4.2 Threads

Same as desktop (`ARCHITECTURE.md`): Slint on the main thread, the
network thread, the voice worker, the screen thread while a stream is
watched (decoding only), plus AAudio's own callback threads for
capture and render. The network thread sleeps until a datagram, a
command or the transport's next timer (a `poll(2)` watcher on the
socket, D43) — no idle polling. The audio workers run at Android's
`THREAD_PRIORITY_AUDIO` (D24).

### 4.3 Lifecycle events

`slint::android::init_with_event_listener` receives the
`android-activity` poll events; `lib.rs` handles:

- `Start` / `Stop` — `frontend::app::set_host_visible(true/false)`
  (§4.1).
- `Resume` — re-checks the runtime permissions and logs any still
  missing.
- `Destroy` — logged; the process ends after `run()` returns (D11).
- `Pause`, `LowMemory`, focus and configuration changes — logged.

### 4.4 Permissions

Runtime: `RECORD_AUDIO`, and `POST_NOTIFICATIONS` from Android 13,
asked for together at startup (D15) through
`BaucordActivity.askPermissions` (D33) and re-checked on every
`Resume`. A grant while connected restarts the service so its type
and notification apply (D34). Install-time (the manifest): `INTERNET`,
`MODIFY_AUDIO_SETTINGS`, `WAKE_LOCK`, `FOREGROUND_SERVICE`,
`FOREGROUND_SERVICE_MICROPHONE`, `FOREGROUND_SERVICE_SPECIAL_USE`.
(`ACCESS_NETWORK_STATE` was declared and never used; it comes back
with the network-change callback of MOB-10, if that needs it.)

## 5. The crate split

Done in 1.1.6. `crates/frontend` holds the client's whole UI logic
(`AppState`, the event pump, the models, Settings, the stream viewer,
`fill_tr`), shared by every host. What only a desktop has — the tray,
global hotkeys, the updater, native dialogs, the system clipboard, the
single instance — is behind its `desktop` feature, with same-API
twins under `src/stubs/` for the phones; `cargo check -p frontend
--no-default-features` builds the phones' version. `crates/desktop` is
`main.rs` alone (ARCHITECTURE §7.1).

Where a phone needs the platform, the first draft's `Platform` trait
became `frontend::host::Hooks` (`host.rs`), which each phone
host installs at startup: pick a file, save a download, open a file
or a link, keep alive (the service), keep the screen on, copy text,
post and cancel notifications, play a chime. Android's implementation
is `crates/android/src/host.rs` over `java.rs` (§10).

The draft's separate mobile `.slint` root (a `mobile` feature in `ui`)
was not needed: the one `main.slint` has a narrow layout and in-window
pages (§9).

## 6. Audio on Android

### 6.1 Backend

Two paths, chosen per session by the echo-cancellation plan
(AEC-SPEC E3):

- **The call path** — `audio/src/native/aaudio.rs` on `ndk::audio`,
  when echo cancellation is wanted (the loudspeaker or the earpiece
  under Automatic, or Always): both streams `Usage::VoiceCommunication`,
  capture with `InputPreset::VoiceCommunication`, **performance mode
  None** and an allocated audio session (D46: a low-latency stream
  without a session takes AAudio's MMAP path, which skips the
  platform's effects), 48 kHz mono `f32` asked for and read back.
  Capture and render feed the same rings as every backend
  (`native/pump.rs`).
- **The media path** — cpal's own AAudio host, as on the first builds,
  for headphones under Automatic and for Off.

The playout cushion follows the device's callback size (D24), seeded
from the render stream's `framesPerBurst` before the first callback
(D51). In the lobby nothing is open at all, and a muted microphone is
closed (D53).

### 6.2 Echo cancellation and the communication mode

On the call path `native/android.rs` (JNI) sets
`MODE_IN_COMMUNICATION`, waits until the call is routed to the chosen
device (D51), and creates an `AcousticEchoCanceler` on the capture
session, switches it on and reads it back: that, not the device's
claims, is what Settings → Audio and the log report (D46, AEC-SPEC
E9). Leaving restores `MODE_NORMAL`.

### 6.3 Routes and device changes

Done (D31): Settings → Audio → *Play sound on* lists the routes
(Automatic, Speaker, Phone earpiece, headsets by name); `audio::route`
decides, `native::android` applies it with `setCommunicationDevice`
(API 31+) or `setSpeakerphoneOn` plus `startBluetoothSco` (29–30). A
route change re-plans and reopens both streams on the worker thread;
a failed reopen is retried.

### 6.4 Processing chain

The desktop chain (RNNoise → VAD → Opus, the bitrate ladder, RED/FEC,
the jitter buffer) runs unchanged. Open: with the platform already
suppressing noise on the call path, RNNoise on top is double
processing and CPU; it stays on until a recording says otherwise
(AEC-SPEC D6 makes the same call on Windows).

### 6.5 Focus

**Not done** (MOB-9): no audio focus and no call-state handling, so a
cellular call plays over the room and the room hears silence. Planned:
`AUDIOFOCUS_GAIN_TRANSIENT` (usage `VoiceCommunication`) on join,
abandoned on leave; losing it mutes capture and the UI says "paused —
call"; regaining it resumes.

## 7. Networking on Android

The transport is the transport. `UdpIo` binds `[::]:0`/`0.0.0.0:0`,
marks EF through `socket2` (carriers strip it, harmless), probes
GSO/GRO and falls back. Nothing in `transport` is `cfg`'d for Android.
Servers listen on 51000 by default (`SESSION-GUIDE.md` §4, the
Movistar case).

**Network change** (Wi-Fi ↔ cellular) changes the client's address:
QUIC migration covers small moves, and otherwise the existing
loss/timeout path reconnects with backoff. Not done (MOB-10): noticing
the change itself — rebinding on `Resume` when the default route
moved, or a `ConnectivityManager` callback. The keep-alive (5 s since
1.2.1, D68) holds NAT bindings on cellular.

## 8. Config, identity, logs

- `client::config::set_config_dir` points at
  `AndroidApp::internal_data_path()`: `settings.json` and the device
  identity key live there, app-private.
- `android:allowBackup="false"`: the device key is a credential and
  must not travel through cloud backup to another device (the server
  would see two devices with one key). Device-to-device transfer is
  not covered by it for apps targeting 31+; `dataExtractionRules` is
  MOB-7.
- `baucord.log` in the same folder (rolled at 5 MB) *and* logcat via
  `android_logger`; `adb logcat -s baucord` is the live view. Users
  reach the file through Settings → Debug: *Send log to server*, *Copy
  log*, the last 100 lines (D47, D58).
- Locale: `sys-locale` reads the system's on Android (seen on the
  first phone build).

## 9. The UI on Android

### 9.1 One root, a narrow layout

The same `main.slint` as the desktop. Under 600 px of width (a phone
upright) `MainWindow` shows the sidebar and the chat as two pages
(D26); Settings (D22, D28) and the stream viewer (D23) are pages
inside the one window, because Slint's Android backend has one window
per process (D16). A separate mobile root (`ui/slint/mobile/`, the
draft's bottom bar and voice bar) is not needed yet; it is PRD-7, for
when the shared one stops fitting.

### 9.2 Touch

`State.touch-host` turns on what a finger needs: drag-to-pan in every
scroll view (D27), controls that do not wait for a hover, long press
for a chat row's menu (D64), and no push-to-talk, screen share or
Keyboard section (D32). The pages keep inside the safe area and above
the keyboard (`safe-area-insets`, `virtual-keyboard-*`; D35); Back
reaches the UI's handlers through the focus chain (D26, D29).

### 9.3 Rules

Every new string goes in all three Fluent catalogs; a `Tr` one is also
a name in `tr.slint`, which `frontend/build.rs` turns into its
`fill_tr` line (the build fails when a catalog lacks it). Every
`.slint` change is checked with `slint-viewer --check` and rendered at
a phone's width as well as 1000×640.

### 9.4 Parity table

| desktop | Android |
|---|---|
| sidebar (channels, users, DMs) | the sidebar as a full-width page, the chat as the second (D26) |
| chat, history paging, unread, images inline, context menu | the same; long press opens a row's menu (D64) |
| files, GIF picker, drag/paste | attachments of any kind through the system picker (images preview, the rest upload as cards), received files and saved pictures into Downloads, Open and links through the system (D30); no drag/paste |
| voice: join, mute, deafen, per-user volume, device pickers | the same; the output picker is the route control (D31); echo cancellation on the call path (D46); no push-to-talk (D32) |
| settings window | the Settings page: a section list and section pages at a phone's width (D22, D28) |
| admin panel, slash commands | slash commands only; `/reports` and `/resolve` for reports (D64) |
| share, viewers | **viewing**: the viewer as a full-window page, decoded by MediaCodec (D23); sharing from the phone is absent (no encoder; the button is hidden on a touch host, D32) |
| tray, keybinds, updater, single instance | absent; the foreground notification is the tray (D34) |

## 10. The JNI surface and the Java that remains

### 10.1 Java (all of it)

`android/app/src/main/java/dev/baucord/`, compiled by Gradle with the
app; no Kotlin (D13). Their methods are called only from native code,
so release builds keep R8 off.

- `BaucordActivity` — the `NativeActivity` subclass (D30): permissions
  (D33), the file picker and its result (`nativeFilePicked`,
  registered from Rust), Downloads through MediaStore, opening files
  and links, the voice service, keeping the screen on (D45), the
  clipboard (D58), notifications through `Notices`, and the
  conversation a notification's tap opens.
- `VoiceService` — the foreground service (D34, D63): its type from
  the room, the notification, the wake lock in a room.
- `Notices` — every notification's look, the message and
  voice-activity notifications, and the ringer / Do Not Disturb check
  before a chime (D52).

`AndroidManifest.xml`: the permissions in §4.4; the activity
`dev.baucord.BaucordActivity` with `android.app.lib_name =
baucord_android`, `launchMode="singleTask"`, `windowSoftInputMode=
"adjustResize"`, the `configChanges` list (§4.1),
`enableOnBackInvokedCallback="false"` (D29), the launcher intent
filter; the service `dev.baucord.VoiceService` with
`foregroundServiceType="microphone|specialUse"` and the special-use
explanation Play reviews (D63), `exported="false"`;
`allowBackup="false"`. Scripts start the app the way the launcher
does — resolve the LAUNCHER Activity, then start it by name — never by
a hard-coded class (D37).

`app/build.gradle.kts`: `compileSdk 36`, `minSdk 29`, `targetSdk 36`,
`ndk.abiFilters` `arm64-v8a` (+ `x86_64` in debug), the Exec task that
runs `cargo ndk` before `preBuild`, the launcher icon (D36), the
version from the crates (D39), the native symbol table in release
bundles (D62).

### 10.2 Rust → Java calls

`crates/android/src/java.rs`, through the `jni` crate on the Activity
`java::init` holds (D33), not on `ndk-context`'s Application:
permissions, the voice service, the screen-on flag, the clipboard, the
file picker, Downloads, opening files and links, notifications and the
chime check. The audio crate makes its own calls from
`audio/src/native/android.rs` (`AudioManager`'s mode, routes and
devices, `AcousticEchoCanceler`) on `ndk-context`'s object, which is
enough for those. Everything else the app does is Rust.

## 11. Build and sideload

```
# Everything in one: Gradle runs cargo-ndk, then packages the APK.
cd android
./gradlew assembleDebug -PcargoTargets=x86_64          # emulator loop
./gradlew assembleDebug                                 # arm64-v8a + x86_64
./gradlew assembleRelease -PcargoTargets=arm64-v8a      # a phone build
#   → app/build/outputs/apk/debug/app-debug.apk
#   → app/build/outputs/apk/release/app-release.apk (signed if the
#     BAUCORD_KEYSTORE properties are set, else -unsigned)

# The smoke test instead of the client (the toolchain alone):
./gradlew assembleDebug -PcargoTargets=x86_64 -PcargoFeatures=hello

# Only the Rust half, by hand (what the Exec task runs; cargo-ndk 4 syntax).
# ANDROID_JAR must be set (§3).
cargo ndk --target x86_64 --platform 29 \
  --output-dir android/app/src/main/jniLibs build -p android

# install / logs
adb install -r android/app/build/outputs/apk/debug/app-debug.apk
adb logcat -s baucord            # logcat; baucord.log is in the app's files dir
adb shell run-as dev.baucord cat files/baucord.log   # debug builds only
```

`android/run.ps1` (or `run.cmd`) chains all of this — build, start the
AVD if needed, wait for boot, install, launch, `logcat` — with
`-Device`, `-Release`, `-Hello`, `-Avd`, `-NoBuild`, `-ColdBoot`
switches: the `cargo run` of the Android host (android/README.md §1b).

Emulator: a **Google APIs x86_64, API 36** image. Extended controls →
Microphone → "Virtual microphone uses host audio input" for voice. The
Windows host is `10.0.2.2` from inside the emulator, so a local server
is `10.0.2.2:51000`.

Signing: one keystore generated once (`keytool`), kept out of the repo,
referenced from `~/.gradle/gradle.properties`; a sideloaded update only
installs over the previous one if the signature matches, so the key is
permanent. It is also the key Play signs with and the upload key Play
checks bundles against (PLAY-STORE.md §4): CI signs the APK and the
bundle with it, after the build (D62). Versioning: `versionCode` counts
up every build (the run number in CI), `versionName` is the crates'
version — `crates/android/Cargo.toml`'s, the same number every
platform ships (D39).

16 KB check before any release: `llvm-readelf -l libbaucord_android.so`
shows `LOAD` alignment `0x4000`; NDK r28+ does this by default and the
release workflow checks it.

## 12. Milestones

All five of the first draft's are behind us; what they left open is
in PRODUCTION-PLAN.md.

| M | was | became |
|---|---|---|
| M0 | toolchain + hello | done 2026-10-05; `--features hello` (`hello.rs`) is still the smoke test for the pipeline |
| M1 | the `frontend` split | done in 1.1.6 (§5) |
| M2 | connect and chat, a phone layout | done with the narrow layout and in-window pages (D22, D26, D28) instead of a mobile root |
| M3 | voice: AAudio backend, AEC, routes, the service, focus, permissions | done (D25, D31, D34, D46) except audio focus (MOB-9) and asking for the microphone at the first join (D15) |
| M4 | resilience, Settings, notifications | Settings and notifications done (D22, D52); the network-change rebind is MOB-10 |
| M5 | release | done: signed APK and AAB in CI (D21, D54, D62), the version from the crates (D39) |

Later, if wanted: a Leave/Mute action on the notification (MOB-13),
tablet layouts, an in-app APK update (moot with Play), push-to-talk on
hardware buttons. An updater for a sideloaded APK is not planned:
Play updates the app (`frontend/src/stubs/updater.rs` is the
no-update twin).

## 13. Risks

| risk | where it stands |
|---|---|
| Skia has no prebuilt for our NDK/target and builds from source (30+ min, disk) | retired: the prebuilt downloads for `aarch64-linux-android` and `x86_64-linux-android`. If it ever builds from source, pin `SKIA_BINARIES_URL` |
| Slint's Android IME: composition, autocorrect, Spanish accents | the composing word is handled (D48); typing works on the phones in use |
| AEC quality varies by OEM | the canceller is read back on its own session (D46) and confirmed on a Galaxy S24 FE (2026-10-07); more makers as reports come |
| Android kills the process in voice despite the service (OEM battery managers) | open; the foreground service holds on the phones tested. Document the "unrestricted battery" setting if a report comes |
| the network loop's wakes on battery | retired (D43, D67): none in a quiet second beyond the socket watcher's two |
| GSO on Android kernels | `quinn-udp` probes; no misbehaviour seen. If it ever does, cap `max_gso_segments` to 1 under `cfg(target_os = "android")` |
| DSCP EF on cellular | expected to be stripped; no effect on correctness |

## 14. Decisions taken after the first draft

**2026-10-05 — M0 written (awaiting its first build).** The
`crates/android` cdylib, the `android/` Gradle project and the CI
workflow exist; `frontend` gained the four guards a single-window host
needs. Nothing is built yet: the sandbox has no NDK, so the checks that
ran are `cargo tree -p android --target aarch64-linux-android` (no
desktop crate resolves; the C builds are opus via cmake,
android-activity's glue, ring's asm, and Skia's prebuilt), a host
`cargo check` of the android manifest (`hello`), `ui`, `i18n`,
`app-core`, `protocol`, and reading the real APIs (Slint 1.17.1's
`android` module and backend, android-activity 0.6.1, jni 0.22.4 as
cpal 0.18.2 uses it, cargo-ndk 4.1.2, android_logger 0.15.1). Decisions:

| # | decision | why |
|---|---|---|
| D10 | **M0 runs the real client, not only a hello.** `frontend::app::run()` is the default (`app` feature); `--features hello` is the toolchain smoke test kept for diagnosing the pipeline | the desktop UI is one `Window`, which is what the Android backend renders; on a tablet-sized emulator it is usable as is, so connect + voice can be tested before any mobile `.slint` exists |
| D11 | **`android_main` ends the process when the Activity is destroyed** (`std::process::exit(0)` after `run()`, as the desktop does) | Slint's platform is set once per process and Android may call `android_main` again in the same process (Back on the root page, then relaunch); a fresh process is correct until the foreground service makes the engine outlive the Activity (D8, M3), when Back must move the task to the background instead of finishing |
| D12 | The workspace `slint` entry stays unchanged (§2.1) | Slint 1.17 target-gates winit and femtovg; nothing desktop reaches the Android target |
| D13 | **No Kotlin in M0–M2**: the Gradle module has no Kotlin plugin and no source; `VoiceService.kt` arrives with M3 and the `kotlin-android` plugin with it | one fewer version to pin and one fewer thing to break on the first build |
| D14 | The `ui` crate's `mobile` feature is deferred to M2 with the mobile root itself (in the end not needed, §5) | a placeholder root re-exporting the desktop one adds a feature with no behaviour |
| D15 | `RECORD_AUDIO` is requested at startup for now; M3 moves it to the first voice join (§4.4) | one fewer step to test voice on the emulator |
| D16 | The auxiliary windows (Settings, admin panel, stream viewer) are refused on Android with a toast (`host-single-window`, in the three catalogs) via `frontend::app::SINGLE_WINDOW_HOST` | the backend hands every `Window` the same adapter; opening a second one would collide with the main window. Settings became a page (D22), the stream viewer too (D23); the toast now covers only the admin panel |
| D17 | JNI lives in `crates/android/src/java.rs` (not `jni.rs`) | a module named like the crate it uses invites path ambiguity |
| D18 | Versions pinned by the Gradle project: Gradle 8.14.3 (wrapper, verified to exist), AGP 8.13.0 (from memory — Studio's upgrade assistant corrects it if wrong), compileSdk/targetSdk 36, minSdk 29 | the last 8.x pair; AGP 9 needs Gradle 9 and a Kotlin-DSL migration nobody needs yet |
| D22 | **Settings is a page inside the main window on Android** (2026-10-05): `settings.slint`'s body became `SettingsPanel`, which `SettingsWindow` wraps on desktop and `MainWindow` shows under `if State.settings-open`; `settingsui.rs` drives either through one `SettingsSurface` enum (`surface(app)`: the window when it exists, the page always), wired at startup by `app::run` on a single-window host. The D16 toast now covers only the admin panel and the stream viewer | the same `.slint` and host code on both hosts; the viewer page (D23) follows the same pattern |
| D21 | **CI builds Android only on a published GitHub Release** (`android-release.yml`: signed arm64 APK attached to the release; secrets for the keystore; `workflow_dispatch` as a dry run). No push/PR build — testing is local, and the desktop `release.yml` stays desktop-only | the user's call on 2026-10-05: the release workflow holds release things, and a PR build buys nothing while one person builds locally |
| D20 | `Cargo.lock` ships with the Android entries already resolved (android-activity 0.6.1, i-slint-backend-android-activity 1.17.1, android_logger 0.15.1, ndk-context 0.1.1; jni 0.22.4 and ndk 0.9.0 were already locked through cpal) | the first `cargo ndk` run resolves nothing new, and the versions the APIs were read against are the ones that build |
| D19 | Stream viewing on Android goes through **MediaCodec via the `ndk` crate's `media` feature** as a third `VideoDecoder` (done 2026-10-06, see D23) | hardware H.264 decode with no FFmpeg build for Android; the trait, `Quality` and the jitter buffer already compile there |
| D70 | **Settings → Audio opens the microphone; the meters are eased** (2026-10-12, 1.2.1, shared; IOS-SPEC.md D60 for the whole of it; amends D53). While the Audio tab is on screen the microphone is open for its meter: not connected, on an engine with no session whose packets are never sent and which mute and push-to-talk do not close; in the lobby or the AFK room, on the session's engine with the packets and the speaking light kept here. That means communication mode and the call streams while the tab shows, as a room would open them — the route picker on that tab is then choosing among what a call offers. The foreground service is not retyped for it (`sync_keep_alive` is unchanged): the tab's microphone exists only while the Activity is on screen, where Android needs no service for one, and `MainEvent::Stop` closes it through `set_host_visible(false)`. The meters are read 20 times a second, fall over 300 ms and glide; a bar that did not move is not drawn. A microphone test plays the voice back half a second late on a phone (a second at first), as on iOS and for the same reason. **Not run on a phone** | the same reason as iOS's |
| D69 | **The review's seventh batch on Android** (2026-10-12, 1.2.1; IOS-SPEC.md D59 for what is shared; amends D30, D43; review P2-27, P2-28, P3-44, P3-45, P3-46, P3-50). *The wake lock* (amends D43): the app's own partial wake lock for a room is taken for 10 minutes (`VoiceService.WAKE_LOCK_MS`) each time the service is started in a room, not without limit. While voice flows the audio streams hold the system's own (`AudioMix`, `AudioIn`); ours covers the join and, because the frontend names the service again when a connection is lost with a retry pending, the retries — the streams are closed then. Without a limit it was hours a day of wake lock for someone who lives in a room, which Play reports as excessive. **Unrun: if voice stops ten minutes into a room with the screen off, this is why, and the constant is the fix.** *Saving a file* (amends D30): `saveDownload(name, mime, byte[])` is `beginDownload(name, mime)` → a pending `MediaStore` row and a detached file descriptor, the bytes written by Rust, then `endDownload(uri, keep)`; the frontend saves on a thread of its own, so `with_activity` attaches that thread. Nothing crosses into the Java heap. *Shared code*: the log tee writes through `app_core::logfile::FileSink` and rolls `baucord.log` over at 5 MB as it grows, not only at launch; the panic hook, the pick slot (`app_core::files::PickSlot`) and the CPU and memory figures (`app_core::usage`: `getrusage`, `/proc/self/statm` times the page size it asks for) are shared too, and the phone build no longer has `sysinfo`. `app-core` is a dependency of the hello build as well. *The build*: with `cargoTargets` unset a release task builds arm64-v8a alone (`gradle.properties` named both ABIs for every build); `cargo-ndk` in the release workflow is a pinned prebuilt. *Checked without a phone*: `java.rs`, `host.rs` and `logging.rs` type-check in a scratch crate against the real `jni`, `android_logger` and `app-core`; the Java compiles against API 34's `android.jar`. |
| D68 | **A phone in the lobby is left alone** (2026-10-12, 1.2.1, shared; IOS-SPEC.md D58; amends D43; review P2-49). D43 kept the 2 s QUIC keep-alive and named a longer one "a possible next step". It is 5 s now, on every platform (`transport::config::CLIENT_KEEP_ALIVE`): inside any NAT's UDP timeout, and inside the 30 s idle timeout still five pings and their retransmissions — the simulator holds an idle connection through twenty minutes of 40 % loss, three seeds. (10 s was tried first: an idle client then took up to 40 s to notice a dead link, against 35 s now and 32 s before.) The keep-alive alone would have changed nothing: the server sent every session a `VoiceReceiverReport` every 2 s, the lobby included, and a client reads a missing one as a stalled link. A new capability bit, `QUIET_REPORTS`, says the client needs no report for an interval in which the relay expected none of its voice; the server then sends none, and the client's stall detector counts silence only once it has been sending voice for as long as a stall takes to show (6 s). The UI is not woken for the 2 s link stats while the app is off screen either (`client::network::set_ui_visible`, from `set_host_visible`), and the workers' wakes of the UI thread are coalesced. In a room nothing changes: voice flows, the keep-alive never fires, the reports come. Against a server older than 1.2.1 the reports keep coming every 2 s; the 10 s telemetry sample is unchanged | on a quiet server a phone in the lobby now exchanges a packet about every five seconds instead of two each way every two; not measured on a device |
| D67 | **The 1.2.1 review: what changed under the phone** (2026-10-10, 1.2.1, shared; IOS-SPEC.md D57; amends D43, D53, D61). *Wakes* (amends D43): `QUIET_TICK` and `Driver::wants_tick` are gone — the network loop waits for a datagram, a command, the voice encoder's next packet or `Driver::next_deadline()` (review P1-2): a quiet second is 0 loop wakes plus the watcher's 2 (`network_idle`, bounded at 25 on every platform). Received voice goes from the network thread straight into the engine and the encoder's packets straight out; the voice thread wakes on commands and speaking edges, else once a second (P1-5; it ticked every 20 ms in a room, 250 ms out of one). The audio workers wait on their device's latch with a 200 ms safety wait instead of 5 and 10 ms timers (P1-4). The screen thread sleeps until a playout slot, a capture tick, `VideoDecoder::next_poll` (MediaCodec: every 4 ms for at most 100 ms after the last frame) or a log line — the "4 ms jitter-buffer drain" D43 kept is gone (P1-7). *Background* (amends D43): a watched stream's frames are dropped before the jitter buffer while the app is off screen, so nothing is decoded; its sound plays on, and back on screen it waits for a keyframe and asks for one (P1-7). The main screen is hidden under the stream and Settings pages instead of drawn beneath them (P1-10). *Rooms* (amends D53): the AFK room counts as out of a room — the microphone closes and the call path is left there too, since the relay drops voice from it (P1-6). *Stream sound* (amends D61): the prebuffer, catch-up and trim live in `stream::pace`, which the desktop's own output reads too; the levels here are unchanged (P1-4) | the review's wakeup audit: each was a wake nobody heard or saw; the relay throws away AFK voice anyway |
| D66 | **The chat keeps to the newest message by the reader's input, not by the scroll position** (2026-10-09, 1.2, shared; `chat.slint` `ChatPane`, `tools/chat-scroll-sim`). Slint's ListView estimates the rows it has not laid out (the average of those on screen, times the count) and rewrites `content-y` itself whenever the estimate moves — a picture laid out, a row added, the end reached after a jump — up as often as down. The pane used to read an upward `content-y` as the reader scrolling up, so it let go of the bottom right after the jump button and on new messages with nobody touching anything (and a DM opened at its oldest message: the first page landed after the pin). Now the reader is known by input: the list's `scrolled` (wheel, drag, fling) and the pane's own scrollbar (the stock one moves the list on a wheel or its arrows without reporting it, so it is off and a look-alike thumb is drawn); while their hand is on the list (until the position is still for 200 ms) moving off the end lets go and coming within 48 px of it holds. Otherwise only reaching the end changes anything (it holds); while held, every height change is followed, by a 16 ms timer that stops after 8 frames at the end. The jump button and the new "N new messages" pill (centered at the jump button's height; `State.chat-new-count`/`-label`, counted by the host from others' messages appended while scrolled up, zeroed at the end) both pin. `tools/chat-scroll-sim` runs the real pane through 48 checks — appends with pictures, wheel, scrollbar drag and wheel, touch fling, jump from far and from the top, a picture growing in place, the keyboard resizing the window, Load earlier, a DM's first page — and the code before this fails 9 | the bug outlived every position-based rule (slack, direction, exact bottom): the position is not the reader's |
| D65 | **General's Activity view** (2026-10-09, 1.2, shared; `frontend` `app/activity.rs`, `chat.slint` `HeaderBtn`). Joins, leaves, moves, shares, watching, renames, level-ups, AFK moves and device notices no longer go into General's conversation, where they buried the messages: they go to a second view behind an icon at the trailing edge of General's header: a page to open the activity, a speech bubble to go back to the messages. Nothing marks either view as having something new (a standing "unseen" dot read as a notification that never goes away). *Clear chat* sits beside it on every width, and in the DM header; the composer keeps only emoji and attach. The view is a model of its own, so its lines never push messages out of General's 500-row window. It is deliberately not a record: memory only, emptied on every connect and disconnect, 200 lines at most, nothing in the log, no menu on its rows. Lines this session needs to see (a disconnect, a restriction, a refusal, history paging) stay in the conversation. Sending from the activity view (a message, a command, a picture, a file) goes to General and turns back to the messages | the spam was the complaint; keeping the lines (rather than dropping them) keeps "who just left?" answerable while connected, without building a presence history |
| D64 | **Blocks, reports, deleting the account, and the terms, on the phone** (2026-10-09, 1.2, shared; PERSISTENCE-SPEC §12 S1; IOS-SPEC.md D56). Everything is the shared `frontend`: a person's menu (a tap on the roster row) has Block/Unblock and Report…; a chat row's menu has Report message… and Block sender; the report dialog (reason, note, also block) is a main-window overlay; Settings → Profile lists who is blocked and has Delete my account (twice); the connect card says connecting is agreeing to the terms, and links them. A chat row now opens its menu on a **long press** (a `Timer` per row in `chat.slint`, 500 ms of the row's TouchArea hover — on touch, "a finger is down here": Slint sends an exit after every release and when a scroll takes over), so the phones finally reach Copy, Report and the rest; the release after a long press does not open a picture. Known limit: a mouse or a pen *hovering* still over a row on a touch host (a tablet with a trackpad) also counts as a press after 500 ms — Slint gives a parent no other sign of a press its child took. The admin panel is still a desktop window; on a phone `/reports` and `/resolve` stand in for its Reports tab, and the admin channel shows each report as it comes | the store requirements (Play's user-content policy) without a phone-only UI; the long press was the parity table's missing piece |
| D63 | **The foreground service's type follows the room** (2026-10-09, 1.2; `VoiceService.java`, `BaucordActivity.java`, `java.rs`, `host.rs`, `frontend` `sync_keep_alive`; PRODUCTION-PLAN MOB-11; PLAY-STORE.md §5). `microphone` in a voice room (with the permission), `specialUse` in the lobby — with the manifest's `PROPERTY_SPECIAL_USE_FGS_SUBTYPE` explaining it for review — and no type at all below API 34, where types are optional; `mediaPlayback` is gone, with its permission. The service is started again whenever the session enters or leaves a room (`KeepAlive.in_room`) and when the app comes back on screen. A microphone type refused because the start came from the background (a reconnect's rejoin) falls back to the lobby's type and is asked for again when the app is shown; a refused promotion no longer stops a running service (that ended the keep-alive on a reconnect), and a first start never stops itself (that was the system's "did not call startForeground" crash). The CPU wake lock is held in a room only | Play reviews every declared type against what the service does; the lobby records nothing, so it may not claim the microphone. The fallback keeps the connection, which is what the service is for; the microphone in the background after a background rejoin waits for the app to be opened, as on iOS (IOS-SPEC D51) |
| D62 | **A Play bundle beside the APK, both signed after the build** (2026-10-08, 1.1.9; `android-release.yml`, `app/build.gradle.kts`; PRODUCTION-PLAN MOB-5, REL-3 in part; PLAY-STORE.md §4). Play takes only app bundles from new apps. One Gradle run (`assembleRelease bundleRelease`) now makes both from the same library, versionCode and versionName, unsigned. Then, in steps of their own: the APK is page-aligned (zipalign `-P 16`), signed with the release key (apksigner, no v4 `.idsig`), checked for 16 KB alignment, and published (artifact, release, website) — before the bundle is touched, so a problem with the bundle never holds the APK back. The bundle is signed with the same release key (jarsigner, as Play asks), which is also the upload key registered with Play: one key and one password, by choice (PLAY-STORE.md §4). It is kept as an artifact. The key's SHA-256 is printed in Play Console's format. Artifacts upload unzipped (`archive: false`), so they download as the `.apk` and `.aab` themselves. The key exists only in each signing step, as a file in RUNNER_TEMP deleted on exit; the job no longer carries the keystore in its environment, and checkout no longer leaves the token in `.git/config`. That keeps the key and its passwords out of the environment cargo and the build scripts run in, but on the same runner a malicious build script could still tamper with the tools the signing steps run; a separate signing job is REL-3's remaining step. Dry runs are signed too (they install over a release for testing). The release build type sets `ndk.debugSymbolLevel = "SYMBOL_TABLE"`: AGP takes the Rust library's symbol table before stripping it and puts it in the bundle's `BUNDLE-METADATA`, so Play's crash and ANR reports name Rust functions; the run warns if none are there. Not built here (no Android SDK in the sandbox): both signing scripts were run on stand-ins with the real jarsigner and keytool, signed and unsigned, and the workflow passes actionlint | one more artifact per release; the key out of the build's environment; Play's first upload possible |
| D61 | **A stream backlog drains by playing faster, not by a skip** (2026-10-08, 1.1.9; `audio/src/stream/mix.rs` `Source::mix_into`; IOS-SPEC.md D55; amends D56). A source past 300 ms queued dropped its oldest 150 ms at once, heard as a skip, and one network burst left the A07 at 250–290 ms queued — late against the picture — for a minute. Now past 180 ms a source plays faster, by linear interpolation between samples, in proportion to the excess over 120 ms (1 % per 40 ms, between 1 % and 4 %), until it is back at 120 ms: 160 ms over takes ~10 s. The last fast hop ends on a whole sample, so plain reading carries on with nothing skipped or repeated (host test: a ramp through a burst, steps checked one by one). Only past 400 ms is the oldest still dropped at once, to 200 ms | a pitch up to two-thirds of a semitone higher for a second or two, easing to a sixth; no skip, and the stream back near its picture within seconds |
| D60 | **A stream's stereo becomes mono without cancelling** (2026-10-08, 1.1.9; `audio/src/stream/downmix.rs`, `stream/decoder.rs` `play_mixed`; IOS-SPEC.md D54). On the call path (D56) the stream was downmixed as `(L + R) / 2`, which silences whatever is opposite in phase between the channels: audiocheck.net's out-of-phase guitar played nothing on the A07 while its left/right test played both sides, and a game's surround or spatial sound folded into stereo came and went. Now each 21 ms window (1024-point FFT, half overlap, √Hann so overlap-add rebuilds the signal) is mixed per frequency: the average of the two channels' strengths at the phase of the sum, or of the difference where that is stronger. Identical channels and one-sided sound come out exactly as `(L + R) / 2` did; out-of-phase sound at its own level. `realfft` (already in the tree through rubato) | one window (~21 ms) of delay and a few µs per window, on the phones only — the desktop plays streams in stereo |
| D59 | **MediaCodec's pictures are paced, not dropped** (2026-10-08, 1.1.9; `video/src/held.rs`, `mediacodec.rs`, `client/src/screen.rs` `emit_picture`; PRODUCTION-PLAN MOB-18). After D57 the A07 showed ~51 of 60 fps with its decode thread half idle: `c2.mtk.avc.decoder` often finishes no picture during a call and two by the next, and the drain kept only the newest — 3,799 of 25,539 pictures (15 %) thrown away. Every finished picture is now copied out into a two-deep queue and each decode call hands out the oldest, so pictures leave at the frames' own cadence (the jitter buffer releases those at the sharer's); a third waiting means falling behind, and the oldest is passed over. A call with a picture already waiting does not wait the 4 ms for this frame's. When frames stop (a still screen), `VideoDecoder::poll` — asked by the screen thread on every pass without a frame — hands out the last one once the gap passes 1.5× the stream's spacing (10–100 ms); before, a picture that missed its call waited for the next frame, however late | a burst costs at most one frame of latency; dropping costs smoothness on every phone whose codec is uneven, and the decoder had the time to spare |
| D58 | **Settings → Debug: Copy log** (2026-10-08, shared; IOS-SPEC.md D53; `settingsui.rs` `copy_log`, `frontend::host::Hooks::copy_text`, `BaucordActivity.copyText`). The log's end goes onto the clipboard under a first line naming the build, the platform, the time and the log file's path — prompted by a PC log taken from a stale copy in another folder, which nothing in it gave away. 128 KB on a phone, 512 KB on the desktop: Android's clip crosses one binder transaction (1 MB, the text as UTF-16), and a session's stream lines at info level fit either way. The hook is the phones' first clipboard write, so an invite code's "copied" now really is on Android and iOS; `copyText` runs on the calling thread and says whether the system took it. On a phone Send gets its own row above Copy and Clear | the user pastes logs into the conversation with the assistant; a copy is quicker than a file, works without a server, and says where it came from |
| D57 | **Only the picture that will be shown is converted** (2026-10-08; `video/src/{yuv,pool,mediacodec,buffer}.rs`, `frontend/src/present.rs`; PRODUCTION-PLAN MOB-18). The first `viewing` lines from a Galaxy A07 watching 1080p60: codec 7–10 ms a frame, our YUV → RGBA conversion 24–30 ms on the decode thread, one core — every picture converted, shown or not, so the decoder fell behind and the picture drifted behind its sound; and freezes, the jitter buffer's 90-frame cap (1.5 s at 60 fps) emptying itself inside a 2 s keyframe interval. Now `MediaCodecDecoder` copies only the planes out (`DecodedFrame.format = PictureFormat::Yuv420 { layout, color }`); the present thread drains its queue, keeps the newest frame per stream and converts that one with `yuv::to_rgba_into` — bands of 32 rows over up to four threads (half the cores), fast paths for I420 and NV12/NV21 — straight into the `SharedPixelBuffer` the UI draws, from a ring of three per stream; frames it cannot get to are skipped unconverted. Decoder buffers go round `video::pool`. The jitter buffer holds 3 s of frames at the stream's rate and jumps to a buffered keyframe once a frame is ≥ 1 s behind live. The summary line says `codec + output` and `preparing X ms each` | fewer frames rather than late ones is the rule for every phone, not a fix for one: the picture stays live with its sound, and the conversion now costs what the shown frames cost, spread over the cores a phone has. A Surface-backed decode (D23) would remove the conversion entirely; not needed if the A07 now shows *preparing* under a frame period |
| D56 | **A watched stream plays through the voice output on the call path** (2026-10-08; `audio/src/stream/mix.rs`, `stream/decoder.rs` `Sink`, `output.rs` `mix_streams`; IOS-SPEC.md D52; was PRODUCTION-PLAN MOB-12). A user on speakers heard voice but never a stream's audio (log `bausung`, 1.1.8, a MediaTek phone): in a room on speakers the phone is in communication mode (D46) and voice plays as the call (`VoiceCommunication`), while the stream had an output of its own — cpal, no usage set, so *media*. Media beside a call has the media volume, which the volume buttons no longer reach during a call (they move the call volume), and some phones silence it. The same output died for good after a reroute (`Stream audio output error: Disconnected`) until the stream was watched again. Now, while a voice output on the call path runs, each stream decoder downmixes to mono, applies the stream's own volume and mute, and hands the samples to `stream::mix`; the voice worker adds them to every hop after deafen and the voice volume (a stream ignores both, as before) and before the limiter. So the stream is the call: the buttons control it, it follows the call's route, and it is part of what the echo canceller removes from the microphone (the canceller's reference is what this output plays). Per source: 100 ms prebuffer (again after running dry — network jitter plus one 40 ms Android callback), at most 300 ms queued (trimmed to 150 ms: drift, a reopening output; since D61 a backlog drains by playing faster instead); a source whose decoder let go leaves at once, and nothing is queued during the grace below. Only when the call stream itself opened (AAudio/VPIO), not on the cpal fallback, which is media anyway. Off the call path — headphones, echo cancellation Off, the lobby, the desktop — the stream keeps its own stereo output, now opened on the decoder's thread and reopened 300 ms after its device goes away or the stream is invalidated (every 2 s while it keeps failing). The decoder picks per frame, with 1.5 s of grace after the voice output stops mixing, so a re-plan does not bounce it between the two. Cost: mono on the call path (a phone speaker is). Untested on a device | one stream, one volume, one route, inside the echo canceller — on the path where the phone is in a call |
| D55 | **The APK on the website** (2026-10-07; `android-release.yml`, `web/index.html`, `web.yaml`). The signed release APK goes to the same R2 bucket as the desktop zips and the page, named like them — `baucord-v1.1.8-android-arm64.apk`, also the GitHub asset's name from now on (it was `baucord-android-v…-arm64.apk`) — with an explicit `application/vnd.android.package-archive` type. The page lists it in the download table and the platform menu; an Android visitor's main button is the APK, with a line on installing (Android 10+, allow installs from the browser once, a new version installs over the old one keeping servers and identity); an iPhone visitor sees "no iPhone version yet". Updating stays by hand: no in-app updater on Android (Play does not allow an app to update itself outside Play, and the Play build is the plan — PLAY-STORE.md). The page's version comes from `release.yml`'s run, so for the few minutes between the desktop build finishing and this one, the APK link can 404 (REL-5). Also fixed on the page: phones scrolled sideways (the hero's single column was `1fr`, so the 2000-px screenshot set its minimum) and the 360-px nav overflowed; the translations' `__VERSION__` was never stamped (`web.yaml` now stamps `web/i18n/*.json` too) | the site is where people get baucord; a phone visitor was told it is desktop-only |
| D54 | **The release workflow on maintained actions, and a way to rebuild a release** (2026-10-07; `android-release.yml`; amends D21). The v1.1.7 release run failed in `android-actions/setup-android@v3`, before any of our code: v3 installs the obsolete `tools` package by default and Google's repository no longer lists it ("Failed to find package 'tools'"). Now `setup-android@v4` with `packages: platform-tools` spelled out, and the other Node 20 actions moved to what `ios-release.yml` already runs (`checkout@v7`, `upload-artifact@v7`; `setup-java@v5`). A release run uses the workflow as it was at the tag, so re-running the failed v1.1.7 job would fail the same way: *Run workflow* with a release tag (`v1.1.7`) now checks that tag out, builds it with the branch's workflow and attaches the signed APK to the existing release; any other input stays a dry run | a CI break after tagging should not need a new tag or a moved one |
| D53 | **No microphone in the lobby or while muted** (2026-10-07; `audio/src/lib.rs` `set_presence`, `client/src/voice.rs` `SetInRoom`, `frontend/src/app/session.rs` `sync_voice_room`, `Notices.playChime`; IOS-SPEC.md D51). Users leave the app connected in the lobby to hear when someone arrives (D52's notice) and saw the microphone indicator and the battery go: every connect started the whole voice engine — capture, the echo canceller, communication mode — though the server drops lobby voice (D43 still assumed "connected = in a voice room", true before the 1.1.5 lobby), and mute only discarded samples from a capture that kept running, which Android rightly shows as the microphone in use: the indicator follows an open recording, not the permission. Now, phones only (`audio::RELEASES_MIC`; the desktop is unchanged): the frontend tells the voice thread whether we are in a voice room (or testing the microphone) whenever the roster changes, and the engine keeps open only what that needs. **In the lobby nothing is open** — no stream, no communication mode, the voice thread ticking at 250 ms instead of 20 ms; the foreground service still keeps the process and the connection (its microphone type stays, so a reconnect's rejoin may open the microphone from the background). A DM chime there is played by `Notices.playChime` as a notification sound (`USAGE_NOTIFICATION_EVENT`, after the ringer check) — a mention too, with the DM's chime; the other sounds (the connect ding, people coming and going) need a stream and are silent in the lobby. **Joining a room** re-plans as before (communication mode, the voice-call streams, the canceller). **Muted or deafened in a room** the capture stream is closed — playback and the call route stay — and unmuting reopens it with a new session and canceller (a fraction of a second; the canceller re-adapts for about a second). Closing the microphone clears the speaking flag and the meter; a reopened output keeps the volume and deafen it had. The indicator goes off after Android's few-second linger. Settings → Audio's echo line reads "Takes effect when you join voice" while the microphone is closed. Untested on a device | an open stream is what the system shows and what drains the battery; the lobby needs neither |
| D52 | **Notifications: the app's icon, the phone's silent mode, DMs and a call starting** (2026-10-07; `Notices.java`, `BaucordActivity`, `VoiceService`, `crates/android/src/{java,host}.rs`, `frontend/src/app/alerts.rs`, `frontend/src/notice.rs`; IOS-SPEC.md D50). Asked for: the app's icon on the notification (it showed a generic headset), the DM sound respecting the phone's silent mode (it played on silent: the chime is mixed into the voice call's audio, which the ringer does not govern), a notification for a DM, and "X joined Y" when someone joins an empty server. (1) **Look**: every notification, the service's included, goes through `Notices.styled` — a white silhouette of the app icon (the dog with its headphones, cut from `assets/icons/icon.png` by `android/notification-icon.py`, committed as `res/drawable-*/ic_stat_baucord.png`; Android uses only a small icon's alpha), the accent colour, and the launcher icon itself as the large icon. (2) **Chimes**: `host::chime_route()` asks `chimeAllowed()` before each DM or mention chime — the ringer mode must be normal and Do Not Disturb off, else no chime; join/leave and share chimes are part of the call and unchanged. (3) **Off screen** (`MainEvent::Stop`), a DM is a notification on the `messages` channel (high importance; the system plays its sound under the phone's modes, so the engine does not chime under it) titled with the sender, the message (or "Sent a file: name") as text, replaced per conversation (tag `dm:<user hex>`); a tap reaches `onNewIntent` → `nativeOpenConversation` → the conversation opens. Opening it in the app, or coming back with it open, takes the notification down. (4) **A call starting**: off screen, the first person into a voice room while every room (the lobby and AFK aside) was empty is "Ana joined General" on the `activity` channel (default importance), at most once per 10 minutes, taken down when the app is shown or the session ends. Both have a switch in Settings → Sounds → Notifications (phones only; `settings.notifications`). The in-app DM sound switch also silences the notification (a quiet group, NotificationCompat's `setSilent` done by hand). All of it needs the app connected: a closed app gets nothing until push (PRODUCTION-PLAN.md PRD-1). The old `drawable/ic_stat_voice.xml` is unused — delete it | the voice call's audio is not a notification, so the system's sound modes never applied to it; the system's own notifications are the way to get them right, in the background and (for the mode check) on screen |
| D51 | **Voice starts on the right device, without first-second dropouts** (2026-10-07; `native/android.rs`, `native/aaudio.rs`, `native/pump.rs`, `lib.rs`; from a user's log on a Galaxy S24 FE). (1) `setCommunicationDevice` says yes at once but the call moves a few hundred ms later: the speaker stream opened on the earpiece and was disconnected a quarter second after, a reopen at every join. `CommunicationMode::enter` now waits — at most 800 ms, polling `getAudioDevicesForAttributes(USAGE_VOICE_COMMUNICATION)` every 20 ms (API 33+) — until the call is routed to the chosen device, and logs `call routed to device N after T ms` (or `not confirmed; opening anyway`). (2) The playout cushion was learned from the first callbacks, at the price of 4–6 underruns per join; the render stream's `framesPerBurst` (1280 frames there) seeds it before the first callback, and the pump only grows from that. (3) The report's reference (`ref=voice speaker (device N)`) is refreshed when playback reopens under a running capture | the only change is a bounded wait before the streams open; everything else as before |
| D50 | **Rotate the stream on a phone whose rotation is locked** (2026-10-07, shared `viewer.slint`; IOS-SPEC.md D49). Users asked to watch in landscape without unlocking rotation. The stream page's bar has a Rotate button (left of the fit/fill one) while the screen is upright: it turns the picture and its chrome a quarter clockwise — a stage inside the panel with `transform-rotation`, laid out at the screen's height × width, its insets taken from the screen edges they now face. Slint turns taps back through the transform, so zoom, pan, double-tap and the buttons work as on a screen that turned by itself; in the turned view fit/fill is offered as in landscape. Kept for the session (`State.watch-rotated`); a screen that turns by itself undoes it | the OS lock is the user's choice; the stream page can still be wide |
| D49 | **The sidebar's header in one row** (2026-10-07, shared `sidebar.slint`). The server's name, then Invite users, New channel and Admin, each only with the permission — they used to take a second row under the name (except on short screens). Each chip is an icon and its word while the name keeps ~96 px beside them, else the icon alone (the word as a tooltip under a mouse); the words are measured per language, so Spanish and Portuguese fall back to icons sooner | one row more for the channel list |
| D48 | **Typing on Android: a word the keyboard is composing counts** (2026-10-07; `chat.slint`, `widgets.slint`). Users: the text "gets highlighted as they type" and a message "won't send until a space". Android keyboards compose a word before committing it; Slint keeps the composition outside the field's `text` and draws it with the *selection* colours (its Android backend marks the whole composition as the composition's selection), and keyboards finish the word on a space — but on Enter or a tap on Send they finish it without telling Slint, so `text` (the draft) lacked the word and the word stayed in the field. Slint commits a composition when the field loses focus; so the composer (Send, Enter) and `Field` (Enter) drop and retake focus first when a composition is pending (Android only), and a `Btn` tap takes the focus to the button (Android only) — "Connect" or "Save" see the whole name, and the keyboard closes as on any Android form. A composing word is drawn as plain text (no selection colours), and the composer's placeholder no longer covers a first word being composed | the fix lives where Slint can do it; the rest is the IME's |
| D47 | **Settings → Debug: the log, readable and sendable from the app** (2026-10-07, shared; IOS-SPEC.md D48). A release APK's log sits in the app's private folder where no user can reach it; the server's "send my log" upload existed with nothing calling it. The Debug tab sends the log's end (2 MB, once per 10 minutes) to the connected server, which files it under its identity folder's `logs/`; it also shows the last 100 lines, clears the log, and holds the log level (moved from General) | the two echo reports came without logs because there was no way to get one |
| D46 | **Echo cancellation that is really on** (2026-10-07; `audio/src/native/aaudio.rs`, `native/android.rs`, `native/pump.rs`; AEC-SPEC.md E9). Two users heard echo with the setting on Always. The voice-call streams asked for the low-latency mode with no audio session, which on phones with an MMAP path (most current ones) is the path that skips the platform's effects — the canceller with them — while the report said `platform` because the preset stuck and the phone *has* a canceller. Now both streams use performance mode **None** (the legacy path, the one calls use; the policy can then pick the VoIP input and output), capture gets **its own audio session**, and an **`AcousticEchoCanceler`** is created on it, switched on and read back: `AAudio voice capture: echo canceller on (AcousticEchoCanceler, session N)` in the log, or `would not switch on` with `setEnabled`'s status and the Settings line "the echo canceller wouldn't switch on". The effect is released with the stream. A period or two of extra latency on the call path only; headphones under Automatic keep cpal's low-latency streams | the verdict must be what the phone did (AEC-SPEC D5); WebRTC's Android audio attaches the effect the same way |
| D45 | **The screen stays on while a stream is watched** (2026-10-07, IOS-SPEC.md D45): `frontend::host::keep_screen_on(true)` when the stream page opens, `false` when it closes; the Android hook calls `BaucordActivity.keepScreenOn`, which sets or clears the window's `FLAG_KEEP_SCREEN_ON` on the UI thread. The flag holds only while the window is visible, so Home or the lock button still lets the phone sleep; chat and the voice lists never set it. No permission (a window flag, not a wake lock) | a video player's behaviour, for exactly as long as there is a picture |
| D44 | **The stream page: your own mic and headphones, and zoom** (2026-10-07, shared `viewer.slint`; IOS-SPEC.md D43, D44). The bar starts with the session's mute and deafen (no need to leave the stream to mute), then the stream's own speaker and level ("Mute the stream"); it tightens under 480 px of room (an ✕ for Stop watching, a shorter slider). The picture zooms: pinch, a double-tap upright (×2.5 where it lands; on its side the double-tap keeps fit ⇄ fill), a drag pans, a chip top-right shows the zoom and resets it. Slint 1.18's touch pinch overshoots (`scale` ≈ e^(r−1)); the page corrects it with `1 + ln(scale)`. Faded controls no longer take taps | watching full screen is where these are needed |
| D43 | **Battery: the workers sleep, the background draws nothing** (2026-10-07, shared with iOS — IOS-SPEC.md D41, D42). Asked "are we doing something or just blasting through battery life?", the audit found the waste in *wakes*: the network thread polled its socket every 1 ms for the whole session (~1000 wakes/s, chat-only included), the voice loop ticked every 5 ms while connected (200/s), a muted microphone's capture worker every 10 ms (100/s); and in the background the UI kept producing frames (stats lines every 2 s, GIFs at 30 fps, a watched stream's every frame). Now: a `poll(2)` watcher thread rings the network loop when a datagram arrives and the loop sleeps up to 100 ms when the transport has no per-tick work (`Driver::wants_tick`) — 10 wakes in a quiet second, measured, and a datagram reaches the voice thread in 0.25 ms; the voice tick is 20 ms, the muted wait 50 ms. `MainEvent::Stop`/`Start` call `frontend::app::set_host_visible(false/true)`: no stats lines, GIFs, stream frames or device poll while off screen; voice, chat and the roster carry on under `VoiceService`. Unchanged, and why: the partial wake lock and the foreground service (voice off screen needs both, and an open audio stream keeps the CPU up anyway); the capture and playback streams stay open while connected, muted or not — this app's "connected" is "in a voice channel", so the cost is a phone call's; the 2 s QUIC keep-alive (keeps NAT bindings and the RTT reading fresh; on cellular it also keeps the radio up — a mobile-only longer interval is a possible next step); the 4 ms jitter-buffer drain while watching a stream (decoding dominates there) | a thousand wakes a second kept the CPU out of deep idle for as long as a session lasted; the fix is also faster |
| D42 | **A compact sidebar for short screens** (2026-10-06, shared with iOS — IOS-SPEC.md D35): below 600 px of height (a phone on its side, a small desktop window) the sidebar tightens its gaps, puts the *New channel* / *Invite* chips on the server-name row, caps Messages at 30 % of the height and at its rows, shows status and latency on one line, shortens the control bar to 44 px and turns *Disconnect* into a red hang-up icon in it — so mute, deafen, settings and disconnect stay on screen; the full stack needs ~600 px | on its side, with Messages open, the controls were pushed off the bottom of the screen |
| D41 | **The stream page fills the screen** (2026-10-06, shared with iOS — IOS-SPEC.md D33): the picture goes edge to edge (under the status and navigation bars and the display cutout), only the name chip and the control bar keep to the safe area; in landscape a double-tap or the bar's zoom button switches fit ⇄ fill (`ImageFit.cover`, cropping the edges), kept for the session in `State.watch-fill`; upright there is no fill, and the bar's stats column now elides so *Stop watching* stays on screen | a 4:3 or 16:9 stream on a 20:9 phone left bands, and the safe area took the screen's edges from the picture as well |
| D40 | **Two shared-UI changes from the iOS port** (2026-10-06, IOS-SPEC.md D29, D31): Settings → General's *Open config folder* is hidden on touch hosts — no phone app opens another app's private folder (on Android the `ACTION_VIEW` of an app-private path had nothing to open it; on iOS the share sheet would have offered the device key); the path above it still says where the folder is. The admin panel's `host-single-window` toast says "phones" instead of "Android". Quit stays on Android (`HOST_CAN_QUIT` is false only on iOS) | the frontend is shared; a change made for iOS is recorded where it also lands |
| D39 | **`versionName` is the crates' version** (2026-10-06, 1.1.7; IOS-SPEC.md D25): the Gradle default reads `crates/android/Cargo.toml` (`baucordCrateVersion`, through `providers.fileContents` so the file is a configuration input) instead of a literal "1.1.6"; `-PbaucordVersionName` still wins, and `android-release.yml` first checks that a `v…` tag equals the crate's version (manual dry runs skip it) | the public crates move together and every platform ships their number; a tag that disagrees fails the release instead of labelling the APK wrongly |
| D38 | **Slint 1.18.1** (2026-10-06, for iOS — IOS-SPEC.md D6, D14): the Android tree is unchanged in shape (`cargo tree --target aarch64-linux-android`: still no winit, femtovg, softbuffer or wgpu; skia 0.99 → 0.153.3 through the Android backend). 1.18.0 also fixes `Key.Back` handlers for apps targeting Android 14+, adds the display cutout to the safe-area insets, wakes the event loop for pending redraws, and follows the display's refresh rate. D29's `enableOnBackInvokedCallback="false"` opt-out stays until a phone confirms Back without it; then it can go | one Slint for every platform; the Android fixes come for free and are worth a check on the next phone build |
| D37 | **`run.ps1` launches the app the way the icon does** (2026-10-06): it still started `dev.baucord/android.app.NativeActivity`, gone since D30 ("Error type 3 … does not exist"), and `am` reports that on stderr with exit code 0, so the script carried on into logcat. First fix, wrong: an implicit `am start -a MAIN -c LAUNCHER dev.baucord` — starting an Activity always resolves with `MATCH_DEFAULT_ONLY` (`ActivityTaskSupervisor.resolveIntent`), so it only finds filters that also list `category.DEFAULT`, and a launcher entry does not ("unable to resolve Intent"). Now: `cmd package resolve-activity --brief -a MAIN -c LAUNCHER dev.baucord` (query flags 0, as the launcher does; the component is the last line), then `am start -a MAIN -c LAUNCHER -n <component>`. `2>&1` inside the device shell (a local one throws under `ErrorActionPreference = "Stop"` in Windows PowerShell 5.1); both outputs checked for `Error`/`Exception`/a `dev.baucord/` component. Tested in PowerShell 7.4 against a fake adb: launch, CRLF output, package missing, `am` failure | survives renaming the Activity; a failed launch stops the script with Android's own message |
| D36 | **The launcher icon is the repo's `assets/icons/icon.png`** (2026-10-06): `app/build.gradle.kts` registers `baucord{Debug,Release}LauncherIcon` (a typed task, `@InputFile`/`@OutputDirectory`, one per variant in `androidComponents.onVariants`, wired with `sources.res.addGeneratedSourceDirectory`) that renders `mipmap-{m,h,xh,xxh,xxxh}dpi/ic_launcher_foreground.png` (108 dp at each density) with plain `javax.imageio` + `Graphics2D` (imported: in a build script `java.awt.…` resolves `java` to the `java` extension; the daemon runs `-Djava.awt.headless=true`): opaque corners → a full-bleed square covering the layer; transparency → scaled so the farthest visible pixel lies on the 66 dp circle every launcher mask shows; over `@color/baucord_bg`. `mipmap-anydpi-v26/ic_launcher.xml` points at it (minSdk 29: adaptive icons everywhere); the placeholder vector is gone. Also `compileOptions` Java 17 (JDK 21 warned about the default 8) | one picture for every platform, re-rendered when it changes; no image tooling beyond the JDK Gradle already runs on |
| D35 | **The pages keep clear of the system bars and the keyboard** (2026-10-06): Android 15+ draws every app edge to edge and `adjustResize` no longer shrinks the window, so the keyboard covered the composer. Slint reports both on the `Window` (`safe-area-insets`, `virtual-keyboard-position/size`, from the IME and system-bar insets its Java helper reads); `MainWindow` derives `inset-*` and `page-width/height` (bottom = max(navigation bar, keyboard overlap)) and places the pages in that rectangle — the connect and startup views, the main screen, the stream page, the Settings page — while their backgrounds still fill the window; dialogs and menus stay window-wide. All zero on the desktop (its 1000×640 render is pixel-identical). Verified headless: with a 300 px keyboard set on the window, Send answers 300 px higher | the toolkit already measures it; one rectangle for every page |
| D34 | **Staying connected off screen**: Android freezes an app's process soon after it leaves the screen (cached-app freezer, Android 14+) and blocks background microphone use, so Home, the lock button or a link (the browser comes up) dropped the session. `VoiceService` (Java, §10.1's design): a foreground service, `foregroundServiceType="microphone|mediaPlayback"` (microphone only when `RECORD_AUDIO` is granted — Android 14 refuses the type otherwise — and from API 30), a low-importance notification "Connected to <server>" (channel and words from the catalogs; tap returns to the app), a partial wake lock, `START_NOT_STICKY`. Started by `frontend::host::keep_alive(Some(server))` on entering `Phase::Main`, stopped by `keep_alive(None)` in `show_connect_phase` — except while an automatic reconnect is pending (`reconnect_hold`), so a network blip in the background can still recover; the process exiting (D11) takes it along. `POST_NOTIFICATIONS` is asked for with the microphone from Android 13; a grant while connected restarts the service so the notification and the microphone type apply. Back on the list now backgrounds the app (Android 12+ moves a root task back) and the session stays | the platform's one sanctioned way to keep a call running; the notification is also the user's sign that it is |
| D33 | **Java calls go to the Activity, not ndk-context's object** (2026-10-06): android-activity 0.6 registers the *Application* with `ndk-context`. `AudioManager` lookups are fine on it (cpal, the audio crate), but it has no `requestPermissions` — the microphone prompt never appeared, users had to grant it in Settings — and none of `BaucordActivity`'s methods ("Method not found pickFile (Ljava/lang/String;J)V"; `nativeFilePicked` was never bound). `java::init(app.vm_as_ptr(), app.activity_as_ptr())` first thing in `android_main` (android-activity's own global reference, alive with the `AndroidApp`); `with_activity` uses it. Permissions go through `BaucordActivity.askPermissions`, which posts `requestPermissions` to the UI thread | the documented accessor for the Activity; nothing else changes |
| D32 | **The phone review** (2026-10-06, the `better-interface` pass over the narrow pages): *Reset to defaults* wiped the saved servers, the nickname and the picture on one tap — the Settings footer now turns into the question (consequence in one line, `Cancel` / `Reset everything` in the danger fill), the same on the desktop window; push-to-talk is not offered on a touch host and a saved one is turned off at startup (no key to hold: the microphone would never open); the screen-share button and the Keyboard section are gone on a touch host (they could only fail); the narrow composer drops *Clear chat* (one tap from Attach) for a button at the chat header's trailing edge; an empty conversation says so (`chat-empty-*`); `Save and close` / `Reset to defaults` in sentence case like the rest; the narrow Settings list keeps Credits/About a group gap under the rest instead of a screen lower | each was a control that misled or a destructive action without a way back; the fixes are deletions and existing pieces (the danger `Btn`, `State.touch-host`) first |
| D31 | **Where call audio plays on a phone** (2026-10-06): in communication mode Android routes the call *and media* — a watched stream's sound — to the communication device, the earpiece unless the app picks; `setSpeakerphoneOn` alone (D25) did not hold (reported: voice in the earpiece, streams silent). `audio::route` (platform-free, unit-tested) decides: the output picker's choice, else a connected headset (wired > USB > BLE > SCO > hearing aid > A2DP), else the loudspeaker; `native::android` applies it with `setCommunicationDevice` from `getAvailableCommunicationDevices()` on API 31+ (`setSpeakerphoneOn`, plus `startBluetoothSco` for a Bluetooth headset, on 29–30), clears it and returns to `MODE_NORMAL` (never the mode found on entry — it is per app since API 31) through a guard that exists as soon as the mode is set, re-plans when the route changes (a headset in or out, another pick: both streams reopen; a failed reopen retries), and logs `playing to …` once a call stream exists. Automatic counts media-only Bluetooth (A2DP, never a communication device) as headphones, so it does not pull the sound onto the loudspeaker; a pick holds communication mode even with echo cancellation Off (plain streams, routed with it), since the earpiece — or the loudspeaker over a headset — is reachable no other way; a picked headset that disconnects is stood in for by Automatic and stays picked (the UI's device poll no longer clears it on a phone). Settings → Audio → *Play sound on* lists those routes (Automatic, Speaker, Phone earpiece, headsets by name; built-in names translated) instead of sound cards; a route also sets the output kind for echo cancellation (speaker/earpiece → the call path, a headset → plain streams under Automatic). Reviewed by a second reader against the Android SDK before the first device run | the platform's own policy; one decision function both the picker and the call use; nothing changes on the desktop |
| D30 | **`BaucordActivity`**, a 200-line Java subclass of `NativeActivity` (2026-10-06, `android/app/src/main/java/dev/baucord/`): NativeActivity never gives native code an activity result, so the system file picker needs it. `pickFile(mime, max)` runs `ACTION_GET_CONTENT` (images only for the avatar, anything for attachments and the identity import) and copies the pick into `cache/picked/` under its display name (an extension added from the MIME type when missing; one pick kept; 64 MB cap, `too-large` past it), one pick at a time (a second Attach while a copy runs gets "still reading", so one pick's file can never reach another's callback), then calls `nativeFilePicked(path, name, error)` — **registered from Rust** (`RegisterNatives` on the Activity's class, `jni::native_method!`), because NativeActivity loads the library itself and the class loader would not find a `Java_…` export. `saveDownload` writes received files, saved pictures and the identity export into the public Downloads through MediaStore (API 29+, no permission; the type comes from the name's extension — MediaProvider renames a file whose type disagrees, `main.rs` → `main.rs.txt`), `openUri`/`uriExists` back a file card's Open, `openUrl` opens links. Rust side: `crates/android/src/{java,host}.rs` implement `frontend::host::Hooks` — the first slice of the §5 `Platform` trait — so the picker's path goes to the same `attach_path` as the desktop's dialog. R8 stays off (JNI-only callers) | the smallest Java that does the job; everything decided stays in Rust |
| D29 | **Back on Android 16** (2026-10-06): an app targeting API 36 on Android 16 is never sent `KEYCODE_BACK` — predictive back answers the gesture with back-to-home — so none of the UI's Back handlers (D26–D28, the viewers, the dialogs) ever ran. The manifest's activity sets `android:enableOnBackInvokedCallback="false"` (Android's documented opt-out); the handlers themselves were verified headless against the real `MainWindow` (arrow taps and `Key.Back` on the chat page, the composer focused or not, Settings section → list → close). Moving to `OnBackInvokedCallback` (in `BaucordActivity`, enabled while a page is open) is the long-term route if the opt-out goes away | the documented opt-out restores the key everywhere at once |
| D28 | **Settings in the narrow layout** (2026-10-06): at a phone's width the Settings page drew the desktop's 188 px nav beside a ~220 px content column — every section cut off on the right, Save & close half off-screen. `SettingsPanel` now has a `narrow` mode (bound to `MainWindow`'s width rule, D26): a top bar (← · Settings), then either the section list (finger-sized rows with a chevron, no highlight) or the open section's content full-width (16 px padding), with the Reset / Save & close footer on both. ← and Android's Back step back: a section → the list → out, saved (`SettingsPanel.back()`). `State.settings-section-open` holds the page: `SettingsSurface::show` clears it (Settings opens on the list), `set_tab` sets it (the connect view's pencil lands in Servers). The nav, content and footer became `SettingsNav` / `SettingsContent` / `SettingsFooter`, shared by both modes; the desktop window renders as before | the same two-page pattern as the chat, so the phone has one way of going deeper and back; every section was checked at 412 px and fits |
| D27 | **Touch scrolling** (2026-10-06): `crates/ui` compiles with `slint-build`'s default widget style, which is **fluent on every target, Android included** (the compiler falls back to fluent when no style is given; only an explicit `native` consults the per-target table that would say material) — and fluent's `ScrollView` builds its `Flickable` with `interactive: false`, so a drag never pans: on a phone nothing scrolled except through the 14 px scrollbar. Every `ListView`/`ScrollView` the phone reaches (chat, roster, Messages list, the Settings page) now binds `mouse-drag-pan-enabled: State.touch-host`. Verified headless with Slint's testing backend against 1.17.1 and the same style: without it a 200 px swipe moves the list 0 px; with it the list follows the finger and flings, and a press held past 500 ms before moving does not scroll (it selects text, as before). The desktop keeps panning off, so a mouse drag still selects | material would also pan, but restyles every std widget on the phone (pickers, fields, switches) for one property; this changes nothing else |
| D26 | **The narrow layout: one page at a time** (2026-10-06): under 600 px of window width (a phone held upright) `MainWindow` shows the sidebar as a full-width list and the chat as a second page; tapping General or a DM (or opening one from a roster menu or `/dm` — `open_dm`) sets `State.narrow-chat-open` and the chat slides in from the right (220 ms push, the list leaves to the left), the chat header's ← (with the unread count of the other conversations on it) or Android's Back slides the list back. Both pages are placed by hand inside one `nav-keys` FocusScope, so `ChatPane` stays a persistent instance; Back is taken in its capture phase, and the scope is focused when the chat page opens or closes — **Back reaches only the focus chain**, so whatever closes a page or overlay must hand focus back (`focus-composer()`, now also called when the Settings and stream pages close) or the next Back finishes the activity. `State.touch-host` (set on a single-window host) makes `focus-composer()` keep the soft keyboard down — it focuses `nav-keys` unless the composer already has focus — and keeps the DM ✕ always visible (no hover on touch). Back also closes the image viewer, the context menu and every dialog, and saves-and-closes the Settings page. A session starts on the list | the parity table's separate Channels/Members/DM pages wait for the mobile root (D14); this gives a phone a usable chat with the same `.slint` and host code, and the width rule means a phone turned sideways, a tablet or a narrow desktop window simply get the layout their width suits |
| D25 | **Echo cancellation on Android** (2026-10-06, `AEC-SPEC.md` §12 E1–E7): the voice-call streams of §6.1–6.2 exist now — `audio/src/native/aaudio.rs` (usage + preset `VoiceCommunication`, LowLatency, 48 kHz mono f32) and `native/android.rs` (JNI: `MODE_IN_COMMUNICATION` + speakerphone, output device types, `AcousticEchoCanceler.isAvailable`) — but only when the session wants them (`AecMode::Auto` = loudspeaker only; headphones and Off stay on cpal). Settings → Audio has the picker and a live status line. Type-checked on the host against the real `ndk` (its `test` feature compiles the bindings off-device) and `jni` crates; first device run pending. Not yet: the route control of §6.3 (the output is speaker unless a headset is connected), focus (§6.5), the foreground service | the phone on speaker was the worst echo case (§6.2); the media path is kept for headphones so Bluetooth stays A2DP |
| D24 | **The playback cushion follows the device's callback size** (2026-10-05): `audio::output` keeps max(20 ms, largest callback + 10 ms) queued and tells the jitter buffer the pull burst (floor = callback − 10 ms); the audio workers run at nice −16 on Android. Details in the voice-breakup paragraph after this table | a fixed 20 ms against AAudio's 20–40 ms callbacks was half silence; the formula is the desktop's old value whenever callbacks are ≤ 10 ms |
| D23 | **Stream viewing on Android** (2026-10-06), two halves. *Decoder:* `video::mediacodec::MediaCodecDecoder` (`crates/video/src/mediacodec.rs`, `cfg(target_os = "android")`, `ndk` features `media` + `api-level-29`): one `video/avc` `AMediaCodec` in synchronous mode with **byte-buffer output** (no Surface — the picture stays on the existing `FrameReady → present-prep → Image` path), configured lazily on the first keyframe from the `EncodedFrame` size, fed whole Annex-B access units (in-band SPS/PPS before every IDR is what every encoder emits), drained newest-wins with a 4 ms wait; **`flush()` is a no-op** (the client flushes before every IDR, which already resets reference state — an `AMediaCodec_flush` there would drop the pictures in the pipeline every 2 s); codec errors and a keyframe with a new size rebuild the codec at the next keyframe. The output layout comes from the buffer format's `image-data` (`MediaImage2`, Codec2 and OMX both set it), else `color-format` 19/21 + `stride`/`slice-height`, cropped by `crop`; **`video::yuv`** (platform-free, unit-tested on the host) walks any 4:2:0 layout to RGBA with fixed-point BT.709/601/2020 limited/full picked from `color-standard`/`color-range` (default BT.709 limited — what the sender signals). `client::screen::create_decoder()` is `cfg`-split: FFmpeg on the desktop, MediaCodec on Android. *Viewer:* `viewer.slint`'s body became `StreamViewerPanel` (a `FocusScope`; `StreamViewerWindow` wraps it on the desktop); `MainWindow` shows one under `if State.watch-open`, below the settings page; `State.watch-*` carry name/frame/stats/mute/volume and `Logic.watch-*` the controls (wired once by `viewers::wire_page` from `app::run`); `viewers::Viewer` holds a private `Surface::{Window, Page}` and `handle_watch_peer` unwatches any other peer first on a single-window host (one page, one stream). Back stops watching (`Key.Back` in the panel), a tap wakes the chrome, the fullscreen control hides on the page | hardware decode without FFmpeg, ~6 ms of CPU per 1080p frame for the colour conversion (measured scalar on a 2.1 GHz Xeon; a NEON `yuv` crate is the upgrade if a phone can't keep 60 fps — 2026-10-08, MOB-18: a Galaxy A07 took 24–30 ms on the decode thread, so the planes now go to the present thread as they are and only the frame that will be shown is converted, over up to four cores), and the same `.slint` + host code on both hosts, as D22. A Surface-backed path (`AImageReader` or a native window under the Slint view) would skip the conversion but needs compositing Slint over video — not for v1 |

**The first builds (2026-10-05).** Four attempts failed before Gradle
and cargo-ndk agreed, each now a line of §3 or the session guide: a
JDK 25 `JAVA_HOME` (`What went wrong: 25.0.3`), `ANDROID_HOME`
pointing at Studio's runtime, cargo-ndk's `ANDROID_PLATFORM=29`
sending Slint's build script after a `platforms/android-29` nobody
installs (hence `ANDROID_JAR`), and Kotlin-DSL name collisions in
`app/build.gradle.kts` (SESSION-GUIDE §4). The fifth built `hello` for
x86_64 in 1m25s on the Skia prebuilt. Then the whole engine compiled
first time and the client connected from a Pixel 10 AVD with the
desktop `main.slint` as it was. The first signed arm64 build on a
phone connected and chatted (in Portuguese: `sys-locale` works) but
had no audio either way: the client dropped the whole engine when the
microphone failed to open. It now keeps playback and retries the input
every 2 s (SESSION-GUIDE §4).

**Voice breaking up (2026-10-05, D24).** The phone played voices but
broke up constantly. The playback worker kept a fixed 20 ms queued
ahead of the device — two periods of a 10 ms desktop device — while
AAudio outside low-latency mode asks for 20–40 ms per callback: a
40 ms callback against 20 ms queued is half silence every time (the
emulator's steady ~25.6 underruns/s with no loss was one per ~39 ms
callback). Now the worker keeps max(20 ms, largest callback + 10 ms)
queued (`output::Cushion`, unchanged for ≤ 10 ms callbacks, so the
desktop keeps its latency) and logs it once (`Output callbacks of N
frames (X ms): keeping Y ms queued, jitter floor Z ms`); the jitter
buffer learns the pull burst (`set_pull_burst`: floor = callback − one
hop) so a low target cannot prime, play one hop and re-prime inside
every burst; and the audio workers take `THREAD_PRIORITY_AUDIO` (nice
−16). On the emulator underruns fell from ~25.6/s to ~4/s, the rest
fitting back-to-back callbacks from its host-audio bridge.
