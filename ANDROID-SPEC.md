# baucord — Android (v1) technical spec

> **Status (2026-10-07):** shipping as 1.1.8, sideloaded (and on the website, D55). §0–§13 are the
> first draft (decisions D1…D9 in §1); §14 holds every decision since
> (D10…D55, the newest at the top of its second table) — **where they disagree, §14 is right**
> (stream viewing, "not in v1" in §0, arrived with D23). What is left:
> `PRODUCTION-PLAN.md` (the MOB items). Start from `START-HERE.md`.

Status when written: draft 1, 2026-09-15; M0 written 2026-10-05 (§14). Decisions in
§1 are taken; the rest is the plan they imply. Read `SESSION-GUIDE.md`
§1 for how this codebase is worked on; the runtime is in
`ARCHITECTURE.md`. Every decision made after this draft goes in §14,
the way `PERSISTENCE-SPEC.md` §12 works.

## 0. Purpose and scope

An Android client for the existing baucord server: **voice and chat**,
distributed as a **sideloaded APK**, with a **Slint** UI that keeps
parity with the desktop client (same widgets, same theme, same strings,
same behaviour), running the **same Rust engine** the desktop runs. The
desktop crate is the reference for behaviour; it is not the base of the
Android app.

In scope for v1: connecting with an invite or a saved server, the
roster and channels, joining voice with hardware echo cancellation,
mute/deafen/speaker route, text chat in channels and DMs (send,
receive, history paging, unread counts, images inline), the settings
that make sense on a phone, staying in voice with the screen off,
reconnect on network change, the file log, telemetry.

Not in v1: screen sharing (capture *or* viewing), file transfer beyond
inline images, the admin panel, keybinds/push-to-talk, the updater,
push notifications, tablets/landscape layouts, 32-bit ARM. Each is a
later milestone (§12) or a non-goal, noted where it comes up.

## 1. Decisions

| # | decision | why |
|---|---|---|
| D1 | UI in Slint via `backend-android-activity-06` | one UI codebase; `frontend` glue shared with desktop; the team's toolkit |
| D2 | `NativeActivity` (Slint's `native-activity` feature), not `GameActivity` | no Kotlin Activity at all; the platform supplies the Activity class |
| D3 | v1 = voice + chat | as agreed |
| D4 | Sideloaded APK, signed with our own key | no Play review; Play's API-36 / 16 KB rules still followed so a store build is a config change later |
| D5 | Kotlin kept to the irreducible minimum: one foreground `Service`, the manifest, Gradle | everything else is Rust calling the platform over JNI |
| D6 | `minSdk 29` (Android 10), `targetSdk 36`, `compileSdk 36` | AAudio's floor is 26; 29 is where permissions and audio behave consistently; 36 is current |
| D7 | Only `arm64-v8a` on devices, `x86_64` for the emulator | 64-bit phones since Android 10; halves build time |
| D8 | The Rust engine lives once per process, not per Activity | Android recreates Activities; the connection must not follow them |
| D9 | The desktop's Slint glue is extracted into a shared `frontend` crate before any Android code depends on it | the only way to get parity without a second 8k-line host |

Open (decide before M2): the name of the shared glue crate (`frontend`
below); whether RNNoise stays on by default on Android (§6.4).

## 2. Where things live

```
baucord/
  Cargo.toml                  workspace (members = ["crates/*"])  — see §2.1
  crates/
    protocol transport client audio video app-core i18n ui server
    desktop/                  the desktop host; shrinks in M1 (§5)
    frontend/                 NEW  Slint-facing controller shared by both hosts
    android/                  cdylib (M0): lib.rs android_main, java.rs JNI,
                              logging.rs logcat+file tee, hello.rs smoke test
  android/                    the Gradle project (no Rust in here)
    settings.gradle.kts
    build.gradle.kts          AGP 8.13.0
    gradle.properties         cargoTargets / cargoFeatures knobs
    gradle/ gradlew gradlew.bat                      Gradle 8.14.3 wrapper
    app/
      build.gradle.kts        runs cargo-ndk before pre<Variant>Build
      src/main/AndroidManifest.xml
      src/main/kotlin/dev/baucord/VoiceService.kt     M3 — the one Kotlin file (not yet)
      src/main/res/                                  theme, strings, placeholder icon
      src/main/jniLibs/                              written by cargo-ndk, git-ignored
  .github/workflows/android-release.yml   signed arm64 APK on a published Release (no PR builds)
```

The Rust side of Android is a normal workspace member (`crates/android`)
so `cargo ndk … build -p android` and every rule in the session guide
apply to it unchanged. The Gradle project sits beside `crates/` because
it is not Rust and Cargo must not see it; it consumes what cargo-ndk
drops into `jniLibs`.

### 2.1 Workspace Cargo.toml

Verified in M0 against Slint 1.17.1's manifests and, on 2026-10-05,
against the real root `Cargo.toml`:

- `members = ["crates/*"]` picks the new crate up; `resolver = "2"` is
  set (a virtual workspace without it falls back to resolver 1, which
  activates features for dependencies of every target at once).
- The `[workspace.dependencies] slint` entry (`default-features =
  false`, `std`, `backend-winit`, `renderer-skia`, `renderer-software`,
  `compat-1-2`) **stays as it is.** In
  Slint 1.17 `i-slint-backend-winit` and `i-slint-renderer-femtovg` are
  `cfg(not(target_os = "android"))` dependencies of the selector and of
  `slint` itself, so `backend-winit` / `renderer-femtovg` in the
  workspace entry resolve to nothing on the Android target
  (`cargo tree -p android --target aarch64-linux-android` shows no
  winit, no femtovg, no softbuffer). `renderer-software` and
  `system-tray` are pure Rust and harmless. The Android backend brings
  the Skia renderer itself; `android` only adds
  `backend-android-activity-06`.
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
Gradle need; nothing else. Versions are what is current as of this
draft — take the newest stable of each.

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
| `cargo-ndk` | `cargo install cargo-ndk` (<https://github.com/bbqsrc/cargo-ndk>) | 4.x (4.1.2 as of M0); finds the newest NDK under `$ANDROID_HOME/ndk` on its own, sets `CC_*`/`AR_*`/the linker per target, exports the cmake toolchain path |
| a JDK **21** | Temurin (`winget install EclipseAdoptium.Temurin.21.JDK`), not Android Studio's `jbr` | Gradle 8.14 runs on JDK 17–24 — the JBR shipped with 2026 Studio builds is 25 and fails the Kotlin DSL compile with just the version (`What went wrong: 25.0.3`) as the message (first build, 2026-10-05). Slint's backend `build.rs` needs `javac` and build-tools' `d8`; both fine on 21 with build-tools ≥ 35 |
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
                                           task sets it from compileSdk; see §14, 2026-10-05)
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

First-build notes: Slint's Android backend builds on Skia. `skia-bindings`
downloads a prebuilt Skia for `aarch64-linux-android` on first build
(network needed; minutes, not seconds); if no prebuilt matches it
builds Skia from source, which needs the NDK's clang and Python and
takes long — M0 exists to find out which of the two we get.

## 4. Runtime architecture on Android

### 4.1 Process, activity, service

- The APK loads `libbaucord_android.so`; `android_main` (in
  `crates/android/src/lib.rs`) runs on the Activity's native thread and
  is the Slint event loop. It is called every time Android creates the
  Activity.
- The engine (`frontend::Host` — the network, voice and config
  workers, §5) is created once per process in a `OnceLock` and
  *attached* to whichever Activity is current. A rotated, backgrounded
  or recreated Activity re-attaches; it never reconnects (D8). The
  manifest still sets `configChanges="orientation|screenSize|keyboardHidden|screenLayout|uiMode"`
  so rotation does not recreate the Activity at all.
- Voice needs the process alive with the screen off: while the user
  is in a voice channel, Rust starts `VoiceService` (a foreground
  service of type `microphone`) through JNI, and stops it on leave or
  disconnect. Android ≥ 12 only allows that while the app is visible,
  which it is at the moment of joining. The service shows the
  persistent "In voice — <channel>" notification (with a Leave
  action, M3) and holds a partial wake lock.
- Chat-only sessions do not start the service; the connection lives
  while the Activity does and the app reconnects on resume (the
  desktop's reconnect path, with its backoff).

### 4.2 Threads

Same as desktop (`ARCHITECTURE.md`): Slint on the main thread, the
network thread, the voice worker, plus AAudio's own callback threads
for capture and render. No screen thread on Android (the `video` crate
stubs already compile there).

One phone-specific change: the network loop's wake floor. If it wakes
every ≤ 1 ms while idle (verify in `client/src/network.rs`), it must
instead sleep until the transport's next timer or the next channel
message — a 1 kHz idle loop is a battery bug on a phone. Voice packets
wake it every 20 ms anyway when it matters.

### 4.3 Lifecycle events

`slint::android::init_with_event_listener` receives the
`android-activity` poll events. The host handles:

- `Resume` — re-check permissions (§10.2), rebind the socket if the
  network changed while paused, resume the UI timers.
- `Pause`/`Stop` — stop the UI timers; keep everything else.
- `Destroy` — detach the Activity from the engine; if no voice
  service is running the OS may kill the process, which is fine.
- `LowMemory` — drop image caches (app-core already bounds them).

### 4.4 Permissions

Runtime: `RECORD_AUDIO` (before the first voice join),
`POST_NOTIFICATIONS` (before the first foreground service). Requested
from Rust with `Activity.requestPermissions(String[], int)` over JNI
(API 23+; no Kotlin) and re-checked with `checkSelfPermission` on every
`Resume` — NativeActivity does not forward the result callback to
native code, and re-checking on resume is the same information one
frame later. Install-time (manifest only): `INTERNET`,
`FOREGROUND_SERVICE`, `FOREGROUND_SERVICE_MICROPHONE`, `WAKE_LOCK`,
`MODIFY_AUDIO_SETTINGS`, `ACCESS_NETWORK_STATE`.

## 5. The crate split

`desktop/src/app.rs` is 8.3k lines: `AppState`, `wire_logic`, the
timers, and a 6k-line `impl AppState` that turns client events into
Slint model updates and Slint callbacks into client commands. The
audit shows its desktop-only touch points are few and shallow: the
tray (36 refs), media/share sources (13), video (10), sysinfo (8),
settings/admin windows (14), viewers/present (15), updater (3),
clipboard (3), `open::that` (2). Everything else is `client::` (207
refs) and `slint::` (70) — portable.

### 5.1 `crates/frontend` (new)

Moves there, unchanged in behaviour: `AppState` and its `impl`, the
event pump (`wire_logic`, `start_timers`, the wake latch), `fill_tr`,
the helper structs and formatters at the bottom of `app.rs`, and
`spawn_workers`. It depends on `client`, `audio`, `app-core`, `i18n`,
`ui` and `slint` — never on anything desktop.

What it cannot do itself goes behind one trait it is given at startup:

```rust
pub trait Platform {
    fn open_url(&self, url: &str);
    fn clipboard_text(&self) -> Option<String>;
    fn set_clipboard_text(&self, text: &str);
    fn config_dir(&self) -> PathBuf;
    fn on_voice_state(&self, state: VoiceState);   // tray tooltip / foreground service
    fn notify(&self, kind: Notice, text: &str);   // tray balloon / Android notification
    fn share_sources(&self) -> Vec<ShareSource> { Vec::new() }   // desktop only
    fn pick_files(&self) -> Vec<PathBuf> { Vec::new() }          // v2 on Android
    fn system_stats(&self) -> Option<SystemStats> { None }
}
```

Desktop implements all of it with what `app.rs` calls today; Android
implements the first six with JNI (§10) and takes the defaults for the
rest. `frontend` gets two cargo features: `desktop-windows` (the
Settings/Admin/Viewer auxiliary windows and the code that drives them)
and `share` (share sources, viewers, present). Android enables
neither.

### 5.2 `crates/desktop` after the split

`main.rs` (env_logger, single instance, config dir, `frontend::run`
with `DesktopPlatform`), `tray.rs`, `hotkeys.rs`, `clipboard.rs`,
`instance.rs`, `updater.rs`, `media.rs`, `present.rs`, `viewers.rs`,
`settingsui.rs`, `adminui.rs`, `resources.rs`. Nothing about chat or
voice remains here. This is M1's deliverable and it ships to desktop
users as a no-behaviour-change release before Android depends on it.

### 5.3 `crates/ui`: one root per platform

`ui/build.rs` compiles one root. A cargo feature `mobile` switches it
from `slint/main.slint` to `slint/mobile/main.slint`. Both roots export
the **same globals** (`State`, `Logic`, `Tr`) and the **same struct
and enum names**, so `frontend` compiles against either without
`cfg`s. The mobile root does not export the auxiliary windows; the
`frontend` code that uses them is behind `desktop-windows`.

### 5.3a Where the split stands (2026-09-18)

Rounds 1–3 are done: `crates/frontend` holds the UI logic behind a
`desktop` feature that gates eleven host-only crates; the host-only
modules (`clipboard`, `hotkeys`, `updater`) have stub twins under
`src/stubs/`; `host` is the seam for native dialogs and the system
launcher; `app.rs` is split into ten child modules by concern
(`events`, `commands`, `presence`, `session`, `menus`, `devices`,
`conversations`, `chat`, `profiles`, `stats`). `cargo check -p
frontend --no-default-features` passes on the host. Round 4 is the
`Platform` trait of §5.1, replacing the stubs where Android needs a
real implementation, and the `ui` crate's `mobile` feature (§5.3).
Then M0.

### 5.4 `crates/android` (new)

As written in M0: `lib.rs` — `android_main`: the logging tee
(`logging.rs`: `android_logger` to logcat *and* `baucord.log` in
`internal_data_path()`, rolled at 5 MB — the client log has been the
most useful artefact in every investigation; keep it), the desktop's
panic hook, `slint::android::init_with_event_listener` (the listener
narrates the lifecycle into the log and re-checks permissions on
`Resume`), `client::config::set_config_dir`, the `RECORD_AUDIO`
request, then `frontend::app::run()` — and `std::process::exit(0)`
when it returns (§14, D11). `java.rs` — the calls in §10 (named `java`,
not `jni`, so the module never shadows the crate); permissions only so
far. `hello.rs` (feature `hello`) — the M0 smoke test. Still to come:
the `OnceLock` engine of D8 (with the service, M3) and
`audio_route.rs`. No `.slint` files here; they live in `ui`.

## 6. Audio on Android

### 6.1 Backend

M0 finding: **cpal 0.18.2's Android host is AAudio already** (the
`ndk` crate's `audio` feature plus JNI for the device list; oboe is
gone), so the existing cpal path compiles and runs on the phone with no
new C++ — that is what M0–M2 use. It opens streams without a usage or
input preset, so the platform's AEC does not engage; M3 adds the
native backend below for that, on the same `ndk::audio` API cpal uses.

`audio/src/native/aaudio.rs`, a third implementation of the
`native/mod.rs` contract beside `wasapi.rs`: capture pushes device-rate
mono `f32` into the `rtrb` ring and pings the wake latch after every
packet; render pops from its ring and fans out; both count glitches
into `AudioStats`, set the `*_stream_failed` flags and stop within
~100 ms. cpal stays out of the hot path on Android exactly as it does
on Windows; the `ndk` crate (0.9, features `audio`, `api-level-26`)
gives `AudioStreamBuilder` with everything below.

```
capture:  direction Input,  usage VoiceCommunication,
          input_preset VoiceCommunication, performance_mode None,
          session_id allocated (→ AcousticEchoCanceler on it, D46),
          sharing Shared, format F32, channels 1, sample_rate unset,
          data_callback → ring + latch, error_callback → restart (§6.3)
render:   direction Output, usage VoiceCommunication, performance_mode
          None, no session; same otherwise
```

Performance mode **None**, not LowLatency (D46): a low-latency stream
without a session is what AAudio puts on its MMAP path, which skips
the platform's effects, the echo canceller among them.

`sample_rate` is left unset and read back from the opened stream; the
workers already resample (rubato) to 48 kHz when the device rate
differs. Buffer sizes: ask for `frames_per_data_callback` of 10 ms and
accept what the device gives; the ring absorbs the difference.

### 6.2 Echo cancellation and the communication mode

baucord has no AEC. On desktop that is fine (headsets); on a phone on
speaker it is unusable. Android provides AEC, noise suppression and
AGC in the platform when *both* streams are opened with
`Usage::VoiceCommunication` and the input preset is
`VoiceCommunication`, and `AudioManager.setMode(MODE_IN_COMMUNICATION)`
is set (JNI, on join; back to `MODE_NORMAL` on leave). That is the
whole AEC story for v1; it is OEM-dependent in quality and the risk
list (§13) says how it is verified.

### 6.3 Routes and device changes

*Done (D31): the route control is Settings → Audio → Play sound on;
`audio::route` decides, `native::android` applies.*

There is no device list on Android; routing is the OS's. The settings
pickers become a **route** control in the voice bar: Speaker /
Earpiece / Headset (wired or Bluetooth, chosen automatically when
present). API ≥ 31: `AudioManager.setCommunicationDevice`; below:
`setSpeakerphoneOn`. Bluetooth headset *microphones* (SCO) are
selected by the same call on 31+; on 29–30 they are not in v1.

AAudio's error callback fires when the route changes or the stream is
stolen (call, another app). The backend flags `*_stream_failed`; the
worker's existing device-loss path reopens both streams (on the
worker thread, never in the callback) and the UI shows nothing unless
the reopen fails twice.

### 6.4 Processing chain

The desktop chain (RNNoise → VAD → Opus, the bitrate ladder, RED/FEC,
the jitter buffer) runs unchanged. Open question D-open-2: with the
platform already suppressing noise, RNNoise on top is double
processing and CPU; proposal is *off by default on Android*, setting
retained. The speaking indicator is driven by RNNoise's VAD today;
with RNNoise off it falls back to the energy VAD the encoder already
has for that case (verify in M2).

Thread priority: Android has no `SCHED_FIFO` for apps; the AAudio
callback threads are already real-time. `platform.rs` gets an
`android` branch that raises the voice worker with
`libc::setpriority(PRIO_PROCESS, 0, -19)` (what
`Process.setThreadPriority(THREAD_PRIORITY_URGENT_AUDIO)` does).

### 6.5 Focus

Request audio focus (`AUDIOFOCUS_GAIN_TRANSIENT`, usage
`VoiceCommunication`) on join, abandon on leave (JNI). Losing focus to
a phone call mutes capture and the UI shows "paused — call"; regaining
it resumes.

## 7. Networking on Android

The transport is the transport. `UdpIo` binds `[::]:0`/`0.0.0.0:0`,
marks EF through `socket2` on the Unix path (allowed for apps; carriers
strip it, harmless), probes GSO/GRO and falls back. Nothing in
`transport` is `cfg`'d for Android specifically and nothing needs to
be.

What is phone-specific:

- **Network change.** Wi-Fi ↔ cellular changes the client's address.
  QUIC on the server side accepts the migrated path; the client
  notices through the existing loss/timeout path and reconnects. v1
  adds one improvement without Kotlin: on `Resume`, compare the
  socket's local address with the current default route and rebind
  if it moved. A `ConnectivityManager` callback (needs a Java
  callback object) is v2.
- **Keep-alive.** 2 s pings keep NAT bindings on cellular; the
  foreground service keeps the radio in a usable state during voice.
  Chat-only in the background is best-effort by design.
- **Port.** Servers listen on 51000 by default now
  (`SESSION-GUIDE.md` §4, the Movistar case); the connect screen's
  placeholder says so.

## 8. Config, identity, logs

- `client::config` gets a directory override (`set_dir_override`)
  called from `android_main` with `AndroidApp::internal_data_path()`.
  `settings.json` and the device identity key live there, app-private.
  `dirs::config_dir()` is not consulted on Android.
- `android:allowBackup="false"`: the device key is a credential; it
  must not travel through cloud backup to another device (the server
  would see two devices with one key).
- `baucord.log` in the same directory (same format, same rotation as
  desktop) *and* logcat via `android_logger` at the same level; `adb
  logcat -s baucord` is the live view, the file is what users send.
- Locale: `sys-locale` on Android needs verifying in M1; the fallback
  is reading `persist.sys.locale` / `Locale.getDefault()` over JNI and
  handing it to `i18n` as desktop does with the OS locale.

## 9. The mobile UI

### 9.1 Files

`crates/ui/slint/mobile/main.slint` is the root. It imports, unchanged:
`widgets.slint`, `theme.slint`, `tr.slint`, `globals.slint`, and the
components of `chat.slint` and `connect.slint` (row rendering, the
composer, markdown/emoji, the server list). New, mobile-only:
`mobile/shell.slint` (navigation), `mobile/voicebar.slint`,
`mobile/channels.slint` (the sidebar as a page), `mobile/members.slint`,
`mobile/settings.slint` (the phone subset as a page, not a window).

### 9.2 Structure

One `Window`. Its content is offset by `safe-area-insets` (a `Window`
property Slint's Android backend fills from the system bars; edge-to-
edge is enforced at targetSdk 35+). Pages: **Connect** (server list,
invite entry), **Channels**, **Chat** (the current channel or DM),
**Members**, **Settings**. Navigation: a bottom bar with Channels /
Chat / Members while connected; Settings from the top bar; the
predictive back gesture pops a page (the backend delivers back as a
key event). A persistent **voice bar** above the bottom bar while in
voice: channel name, mute, deafen, route, leave.

### 9.3 Rules

- Touch targets ≥ 48×48 dp; row heights from `theme.slint` scaled, not
  redefined. Hover states are inert on touch; pressed states are what
  users see.
- Text sizes come from `theme.slint`; nothing below 14 sp.
- The soft keyboard resizes the window (`adjustResize`); the composer
  stays above it and the chat list keeps its bottom anchor.
- Every new string goes in all three Fluent catalogs and every `Tr`
  property is filled in `fill_tr` — the existing rule, unchanged.
- Every `.slint` change is checked with `slint-viewer --check` and
  rendered at a phone viewBox (412×915 logical) before hand-over;
  `mobile/main.slint` is added to the guide's check list.

### 9.4 Parity table (v1)

| desktop | android v1 |
|---|---|
| sidebar (channels, users, DMs) | Channels + Members pages, DMs in Channels; until the mobile root: the sidebar and the chat as two pages of the narrow layout (D26, done) |
| chat, history paging, unread, images inline, context menu | same (long-press opens the menu) |
| files, GIF picker, drag/paste | attachments of any kind through the system picker (images preview, the rest upload as cards), received files and saved pictures into Downloads, Open and links through the system (D30); no drag/paste |
| voice: join, mute, deafen, per-user volume, device pickers | same; the output picker is the route control (D31, done); echo cancellation on the loudspeaker (D25); no push-to-talk (D32) |
| settings window | Settings page — the desktop panel as a full-window page (D22, done), a section list and section pages at a phone's width (D28, done); the phone subset and the route control come with the mobile root |
| admin panel, slash commands | slash commands only |
| share, viewers | **viewing** works: the desktop viewer panel as a full-window page, decoded by MediaCodec (D23); sharing from the phone is absent (no encoder; the button is hidden on a touch host, D32) |
| tray, keybinds, updater, single instance | absent; the foreground notification is the tray (D34, done) |

## 10. The JNI surface and the Kotlin that remains

### 10.1 Kotlin (all of it)

`VoiceService.kt` (~60 lines): `onStartCommand` builds the
notification (channel `voice`, low importance, ongoing) and calls
`startForeground(id, notification, FOREGROUND_SERVICE_TYPE_MICROPHONE)`,
acquires a partial wake lock; `onDestroy` releases it. It has no logic;
the Leave action sends an intent the Rust side polls (M3).

`AndroidManifest.xml`: the permissions in §4.4; the activity is
`dev.baucord.BaucordActivity` (a `NativeActivity` subclass, D30) with
`android.app.lib_name = baucord_android`, `launchMode="singleTask"`,
`windowSoftInputMode="adjustResize"`, the `configChanges` list from
§4.1, `exported="true"` with the launcher intent filter; the service
`dev.baucord.VoiceService` (Java, D34) with
`foregroundServiceType="microphone|mediaPlayback"`,
`exported="false"`; `allowBackup="false"`. Scripts start the app the way
the launcher does — resolve the LAUNCHER Activity, then start it by
name — never by a hard-coded class (D37).

`build.gradle.kts`: `compileSdk 36`, `minSdk 29`, `targetSdk 36`,
`ndk.abiFilters` `arm64-v8a` (+ `x86_64` in debug), the signing config
for release, and one `Exec` task that runs `cargo ndk` before
`preBuild` so `./gradlew assembleRelease` builds the Rust too.

### 10.2 Rust → Java calls (`crates/android/src/jni.rs`)

All through the `jni` crate on the main thread, from
`AndroidApp::vm_as_ptr()` / `activity_as_ptr()`:

| call | for |
|---|---|
| `Activity.checkSelfPermission` / `requestPermissions` | §4.4 |
| `Context.startForegroundService(Intent)` / `stopService` | §4.1 |
| `AudioManager.setMode`, `setCommunicationDevice` / `setSpeakerphoneOn`, `requestAudioFocus` / `abandonAudioFocusRequest` | §6 |
| `Intent(ACTION_VIEW, uri)` + `startActivity` | `Platform::open_url` (done as `BaucordActivity.openUrl`, D30) |
| `BaucordActivity.pickFile` / `saveDownload` / `openUri` / `uriExists`, and `nativeFilePicked` back | files (D30) |
| `ClipboardManager` | via Slint's backend (it already implements the clipboard) — `Platform` delegates |
| `NotificationManager.notify` | `Platform::notify` (DM received while in voice, M3) |
| `Locale.getDefault()` | §8 fallback |

Everything else the app does is Rust.

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

# M0 smoke test instead of the client (toolchain only):
./gradlew assembleDebug -PcargoTargets=x86_64 -PcargoFeatures=hello

# Only the Rust half, by hand (what the Exec task runs; cargo-ndk 4 syntax).
# ANDROID_JAR must be set (§3): cargo-ndk's ANDROID_PLATFORM=29 would
# otherwise send Slint's build.rs looking for platforms/android-29.
cargo ndk --target x86_64 --platform 29 \
  --output-dir android/app/src/main/jniLibs build -p android

# install / logs
adb install -r android/app/build/outputs/apk/debug/app-debug.apk
adb logcat -s baucord            # logcat; baucord.log is in the app's files dir
adb shell run-as dev.baucord cat files/baucord.log   # debug builds only
```

`android/run.ps1` (or `run.cmd`) chains all of this — build, start the
AVD if needed, wait for boot, install, launch, `logcat` — with
`-Device`, `-Release`, `-Hello`, `-Avd`, `-NoBuild` switches: the
`cargo run` of the Android host.

`cargoTargets` and `cargoFeatures` default in `android/gradle.properties`.
Until the permission request lands on the first voice join, a build
whose dialog was dismissed can be fixed with
`adb shell pm grant dev.baucord android.permission.RECORD_AUDIO`.

Emulator: a **Google APIs x86_64, API 36** image; for the desktop UI
that M0–M1 show, a tablet profile (Pixel Tablet) or a phone in
landscape — the phone layout is M2. Extended controls → Microphone →
"Virtual microphone uses host audio input" for voice. The Windows host
is `10.0.2.2` from inside the emulator, so a local server is
`10.0.2.2:51000`.
Signing: one keystore generated once (`keytool`), kept out of the repo,
referenced from `~/.gradle/gradle.properties`; a sideloaded update only
installs over the previous one if the signature matches, so the key is
permanent. Versioning: `versionCode` counts up every build,
`versionName` is the crates' version — `crates/android/Cargo.toml`'s,
the same number every platform ships (D39).

16 KB check before any release: `llvm-readelf -l libbaucord_android.so`
shows `LOAD` alignment `0x4000`; NDK r28+ does this by default and the
check is there to catch a regression.

## 12. Milestones

| M | deliverable | done when |
|---|---|---|
| M0 ✅ 2026-10-05 | toolchain + hello: `crates/android` shows a Slint page with the theme on the emulator (`ui/mobile` deferred, D14; the workspace `slint` entry needed no change, D12) | done on the x86_64 emulator: hello builds in ~1.5 min, Skia prebuilt downloads; the phone comes with the first `app` build |
| M1 | the split: `frontend` extracted, `Platform` trait, desktop rebuilt on it | desktop 1.1.5 ships with no behaviour change; `frontend` has no desktop dependency |
| M2 | connect + chat on Android: Connect, Channels, Chat, Members pages; history, unread, images. *Connecting, the roster and channels already work through the desktop root (2026-10-05); M2 is the phone layout.* | a DM and a channel conversation with desktop users work; `baucord.log` written; locale right |
| M3 | voice: AAudio backend, AEC, route control, foreground service, focus, permissions | 30 min in voice on speaker with the screen off, no echo reported by the others, telemetry shows the ladder healthy |
| M4 | resilience + settings: network change rebind, reconnect, Settings page, notifications for DMs in voice | Wi-Fi → cellular mid-call recovers under 10 s |
| M5 | release: signing, `versionCode`, README section, the guide's Android rules | the three Movistar users run it for a week |

Viewing shares (MediaCodec H.264 decode) was pulled forward at the
user's request and landed on 2026-10-06 (D23) ahead of M2–M3.

Later (v2 candidates, in rough order): file cards with Save, `ConnectivityManager` callback, Leave
action from the notification, tablet layout, in-app APK update,
Bluetooth SCO on API 29–30, push-to-talk on hardware buttons.

## 13. Risks, and how each is retired

| risk | when | how |
|---|---|---|
| Skia has no prebuilt for our NDK/target and builds from source (30+ min, disk) | M0 | mostly retired on paper: Slint 1.17.1 → skia-safe 0.99 asks for `gl,vulkan,textlayout` on Android, and rust-skia's binaries matrix publishes exactly that for `aarch64-linux-android` and `x86_64-linux-android` (built against API 26). The first `cargo ndk` run confirms; if it ever builds from source, pin `SKIA_BINARIES_URL` |
| Slint's Android IME: composition, autocorrect, Spanish accents in the composer | M2 | type real text on a real phone in the first chat build |
| AEC quality varies by OEM; some need `MODE_IN_COMMUNICATION` before the streams open | M3 | open streams after `setMode`; test on ≥ 3 makers |
| `sys-locale`, `dirs`, `interprocess` or another desktop-shaped crate behind a `frontend` dependency | M1 | `cargo tree -p android --target aarch64-linux-android` must show none of desktop's |
| the 1 ms network wake floor on battery | M2 | measure with Battery Historian / `dumpsys batterystats` |
| GSO on Android kernels | M2 | `quinn-udp` probes; if the probe misbehaves, cap `max_gso_segments` to 1 under `cfg(target_os = "android")` |
| Android kills the process in voice despite the service (OEM battery managers) | M3 | test on the OEMs at hand; document the "unrestricted battery" setting in the README |
| DSCP EF on cellular | — | expected to be stripped; no effect on correctness |

## 14. Decisions taken after this draft

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
| D14 | The `ui` crate's `mobile` feature (§5.3) is deferred to M2 with the mobile root itself | a placeholder root re-exporting the desktop one adds a feature with no behaviour |
| D15 | `RECORD_AUDIO` is requested at startup for now; M3 moves it to the first voice join (§4.4) | one fewer step to test voice on the emulator |
| D16 | The auxiliary windows (Settings, admin panel, stream viewer) are refused on Android with a toast (`host-single-window`, in the three catalogs) via `frontend::app::SINGLE_WINDOW_HOST` | the backend hands every `Window` the same adapter; opening a second one would collide with the main window. Settings became a page (D22), the stream viewer too (D23); the toast now covers only the admin panel |
| D17 | JNI lives in `crates/android/src/java.rs` (not `jni.rs`) | a module named like the crate it uses invites path ambiguity |
| D18 | Versions pinned by the Gradle project: Gradle 8.14.3 (wrapper, verified to exist), AGP 8.13.0 (from memory — Studio's upgrade assistant corrects it if wrong), compileSdk/targetSdk 36, minSdk 29 | the last 8.x pair; AGP 9 needs Gradle 9 and a Kotlin-DSL migration nobody needs yet |
| D22 | **Settings is a page inside the main window on Android** (2026-10-05): `settings.slint`'s body became `SettingsPanel`, which `SettingsWindow` wraps on desktop and `MainWindow` shows under `if State.settings-open`; `settingsui.rs` drives either through one `SettingsSurface` enum (`surface(app)`: the window when it exists, the page always), wired at startup by `app::run` on a single-window host. The D16 toast now covers only the admin panel and the stream viewer | the same `.slint` and host code on both hosts; the viewer page (D23) follows the same pattern |
| D21 | **CI builds Android only on a published GitHub Release** (`android-release.yml`: signed arm64 APK attached to the release; secrets for the keystore; `workflow_dispatch` as a dry run). No push/PR build — testing is local, and the desktop `release.yml` stays desktop-only | the user's call on 2026-10-05: the release workflow holds release things, and a PR build buys nothing while one person builds locally |
| D20 | `Cargo.lock` ships with the Android entries already resolved (android-activity 0.6.1, i-slint-backend-android-activity 1.17.1, android_logger 0.15.1, ndk-context 0.1.1; jni 0.22.4 and ndk 0.9.0 were already locked through cpal) | the first `cargo ndk` run resolves nothing new, and the versions the APIs were read against are the ones that build |
| D19 | Stream viewing on Android goes through **MediaCodec via the `ndk` crate's `media` feature** as a third `VideoDecoder` (done 2026-10-06, see D23) | hardware H.264 decode with no FFmpeg build for Android; the trait, `Quality` and the jitter buffer already compile there |
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
| D24 | **The playback cushion follows the device's callback size** (2026-10-05): `audio::output` keeps max(20 ms, largest callback + 10 ms) queued and tells the jitter buffer the pull burst (floor = callback − 10 ms); the audio workers run at nice −16 on Android. Details in the voice-breakup paragraph after the first-build log | a fixed 20 ms against AAudio's 20–40 ms callbacks was half silence; the formula is the desktop's old value whenever callbacks are ≤ 10 ms |
| D23 | **Stream viewing on Android** (2026-10-06), two halves. *Decoder:* `video::mediacodec::MediaCodecDecoder` (`crates/video/src/mediacodec.rs`, `cfg(target_os = "android")`, `ndk` features `media` + `api-level-29`): one `video/avc` `AMediaCodec` in synchronous mode with **byte-buffer output** (no Surface — the picture stays on the existing `FrameReady → present-prep → Image` path), configured lazily on the first keyframe from the `EncodedFrame` size, fed whole Annex-B access units (in-band SPS/PPS before every IDR is what every encoder emits), drained newest-wins with a 4 ms wait; **`flush()` is a no-op** (the client flushes before every IDR, which already resets reference state — an `AMediaCodec_flush` there would drop the pictures in the pipeline every 2 s); codec errors and a keyframe with a new size rebuild the codec at the next keyframe. The output layout comes from the buffer format's `image-data` (`MediaImage2`, Codec2 and OMX both set it), else `color-format` 19/21 + `stride`/`slice-height`, cropped by `crop`; **`video::yuv`** (platform-free, unit-tested on the host) walks any 4:2:0 layout to RGBA with fixed-point BT.709/601/2020 limited/full picked from `color-standard`/`color-range` (default BT.709 limited — what the sender signals). `client::screen::create_decoder()` is `cfg`-split: FFmpeg on the desktop, MediaCodec on Android. *Viewer:* `viewer.slint`'s body became `StreamViewerPanel` (a `FocusScope`; `StreamViewerWindow` wraps it on the desktop); `MainWindow` shows one under `if State.watch-open`, below the settings page; `State.watch-*` carry name/frame/stats/mute/volume and `Logic.watch-*` the controls (wired once by `viewers::wire_page` from `app::run`); `viewers::Viewer` holds a private `Surface::{Window, Page}` and `handle_watch_peer` unwatches any other peer first on a single-window host (one page, one stream). Back stops watching (`Key.Back` in the panel), a tap wakes the chrome, the fullscreen control hides on the page | hardware decode without FFmpeg, ~6 ms of CPU per 1080p frame for the colour conversion (measured scalar on a 2.1 GHz Xeon; a NEON `yuv` crate is the upgrade if a phone can't keep 60 fps), and the same `.slint` + host code on both hosts, as D22. A Surface-backed path (`AImageReader` or a native window under the Slint view) would skip the conversion but needs compositing Slint over video — not for v1 |

What the first build will tell us, in order: whether the Skia prebuilt
downloads (§13), whether cpal's AAudio host opens the emulator's mic,
and whether the desktop `main.slint` is usable on a tablet AVD. Record
each in SESSION-GUIDE.md §5 and here.

**First-build log (2026-10-05).** Attempt 1 died before Gradle read
the project: `What went wrong: 25.0.3` after "Kotlin DSL script
compilation (Settings/TopLevel/stage1)". That bare version string is
Gradle's Kotlin compiler choking on an unsupported JVM — `JAVA_HOME`
was Android Studio's `jbr`, a JDK 25, and Gradle 8.14 runs on 17–24.
Fix: a Temurin JDK 21 as `JAVA_HOME` (§3), `gradlew --stop`, retry —
confirmed working the same day. Android Studio needs the same choice
made in its *Gradle JDK* setting, since it does not read `JAVA_HOME`.
CI is unaffected (`setup-java` installs Temurin 17). Not taken:
Gradle 9.1 + AGP 9 to keep JDK 25 — an unverified pair on top of an
unbuilt project. Attempt 2 got as far as AGP: `Using Android SDK:
D:\android\jbr` — `ANDROID_HOME` had been set to Studio's runtime
directory instead of the SDK, and AGP then asked for Build-Tools
35.0.0 (its default; the installed 36 does not substitute). Fix:
`ANDROID_HOME`/`ANDROID_SDK_ROOT` to the SDK Manager's location,
`ANDROID_NDK_HOME`/`ANDROID_NDK` under it, Build-Tools 35.0.0 ticked
or `sdkmanager --licenses` so Gradle fetches it. Attempt 3 reached
the Rust build and died in `i-slint-backend-android-activity`'s
build.rs: "No Android platforms found". **cargo-ndk 4 exports
`ANDROID_PLATFORM=<--platform>` (29, the NDK API level) to cargo's
children, and the `android-build` crate reads that same name as the
SDK platform to compile the Java helper against** — so it wanted
`platforms/android-29/android.jar`, which nobody installs. It checks
`ANDROID_JAR` first and cargo-ndk does not touch it: the Gradle Exec
tasks now set `ANDROID_JAR` to `platforms/android-<compileSdk>/
android.jar` under the SDK AGP resolves (`pointAtCompileSdkJar`), and
a hand-run `cargo ndk` needs the variable set (§3). Not chosen:
installing `platforms;android-29` (compiling the helper against an
old SDK for no reason) or dropping `--platform` (cargo-ndk's default
is 21, below minSdk). Attempt 4 tripped on three Kotlin-DSL name
collisions in `app/build.gradle.kts` (`java` is the Java-plugin
extension in a script, so `java.util.Properties` needs an import;
`compileSdkVersion` inside `android { }` is AGP's deprecated String
property and shadows a script val — now `baucordCompileSdk`).
**Attempt 5: `BUILD SUCCESSFUL` for `hello` on x86_64 — 1m18s of
Rust, 1m25s total; the Skia prebuilt downloaded (no "FULL BUILD"),
so §13's first risk is retired.** AGP warned "Unable to strip the
following libraries" — it had no `ndkVersion`; the module now sets it
to the newest NDK under the SDK. **Installed and ran on a Pixel 10
AVD (API 36.1, x86_64): the page renders, taps count, logcat shows the
start line, `lifecycle: start/resume` and the `RECORD_AUDIO` check —
i.e. `java.rs`'s jni 0.22 calls work. M0 is done.** Then the `app`
build: **the whole engine compiled for x86_64 Android first time and
the client connected to the official server from the Pixel 10 AVD —
roster, channels, lobby → room, speaking indicators, voice received,
41 ms RTT in the status line — with the desktop `main.slint` as is.**
So M2's "connect" and M3's receive path are demonstrated on cpal's
AAudio host without an Android-specific line of UI. Not yet tested:
the microphone (emulator mic not enabled); observed: the chat pane
overflows a portrait phone (the mobile root is M2's job), and the
sidebar's memory figure read 1164 MB on the debug build — expected to
be the unstripped `.so` with DWARF mapped in; re-read on the release
build before treating it as a sysinfo-on-Android issue. Then the
**signed arm64 release on a real phone**: connects, chat works, the
toast is in Portuguese (`sys-locale` works), Mem 321 MB (so the debug
figure was DWARF) — but **no audio either way**, and Settings / the
stream viewer show the D16 toast as designed. Two client-side causes
for the silence found by reading: `client/src/voice.rs` dropped the
whole engine when `VoiceChat::start()` returned any `Err`, so a
capture failure (no microphone permission yet) also killed playback —
the audio crate's own comment warned of exactly this; and
`VoiceChat::service()` never retried an input that had not opened
(`input.is_some()` gate). Fixed: the client keeps the engine when
`output_running()` and toasts "Microphone unavailable"; `service()`
retries a pending input every 2 s (`input_pending`) and logs "Input
started late". Whether playback itself also failed on the phone is
what the next logcat says (`Voice output failed to start` vs `Voice
input failed to start`).

**Stream viewing on the emulator (2026-10-05, later).** Worked first
run: `c2.goldfish.h264.decoder`, output layout `1920x1080 Y@0 s1920
U@2073600 s960 V@2592000 s960` (I420 via `image-data`, BT.709
limited), 336 pictures from 361 frames in 22 s. The emulator decodes a
1080p frame in 20–70 ms (the goldfish codec pipes every frame through
the host, and the dev profile builds the workspace at `opt-level = 1`,
so the colour conversion runs 3× slower than measured), which is ~16
fps and tripped the per-frame "Slow decode" / "SLOW FRAME" warnings
on every frame — 652 of 729 log lines. Those are now once-a-second
summaries (`video::timing::SlowFrames`, in the client and the
frontend). The phone's hardware decoder is the real measurement. Same
log, the voice line read `Underruns 162` in 6 s with `Lost 0` — first
blamed on the emulator's audio clock; wrong, see the next paragraph.

**Voice breaking up on Android (2026-10-05, D24).** The phone played
voices but broke up constantly. Cause, in `audio::output`: the playback
worker kept a fixed **20 ms** queued ahead of the device — two periods
of a 10 ms desktop device — and AAudio outside low-latency mode asks
for 20–40 ms per callback (cpal leaves `BufferSize::Default` there, and
the performance mode at NONE without cpal's `realtime` feature). A
40 ms callback against 20 ms queued is half silence on every callback;
the emulator's steady ~25.6 underruns/s with zero loss was exactly one
per ~39 ms callback. A 20 ms callback has zero margin, so any late
worker wake is a gap. Fixes: the output callback records the largest
callback it has served and the worker keeps **max(20 ms, callback +
10 ms)** queued (`output::Cushion`, unit-tested; unchanged for ≤ 10 ms
callbacks, so the desktop keeps its latency) and logs it once
(`Output callbacks of N frames (X ms): keeping Y ms queued, jitter floor
Z ms`); the jitter buffer learns the pull burst (`set_pull_burst`: floor
= callback − one hop, 2 ms on desktop) so a low target cannot prime,
play one hop and re-prime inside every burst (simulation test: 40 ms
bursts against 20 ms arrivals play clean with the floor); and the audio
worker threads take Android's `THREAD_PRIORITY_AUDIO` (nice −16,
`platform::promote_current_thread`, logged per thread) instead of
the nice −10 every thread of ours inherits from the activity's
boosted main thread (the same level as the video threads). The AAudio
callback thread itself is left alone (already audio-class).

*Emulator, after the fix (same evening):* `Output callbacks of 2180
frames (45.4 ms): keeping 55.4 ms queued, jitter floor 35.4 ms`; workers
`nice -10 -> -16`; `Underruns` 21 → 178 over 48 s, i.e. ~4/s against
~25.6/s before — one callback in five instead of every one, while the
refill after a callback takes well under a millisecond. What is left
fits back-to-back callbacks from the emulator's host-audio bridge (two
45 ms requests inside one refill window need 90 ms queued); the phone's
rate decides whether the cushion should also grow on underruns
(AAudio's own tuning does: one burst per xrun). Lost 0, PLC 6 in 48 s,
the emulator microphone sending. Cost: ~55 ms queued + a 35 ms jitter
floor on a 45 ms-callback device, against ~22 ms on desktop — the price
of AAudio's default mode until low-latency mode (M3). Next
step if a device still breaks up: low-latency performance mode +
`VoiceCommunication` usage, which needs our own AAudio builder (cpal
exposes neither without its `realtime` feature) — M3's audio work.
