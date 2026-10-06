# baucord — Android (v1) technical spec

Status: draft 1, 2026-09-15; M0 written 2026-10-05 (§14). Decisions in
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
          input_preset VoiceCommunication, performance_mode LowLatency,
          sharing Shared, format F32, channels 1, sample_rate unset,
          data_callback → ring + latch, error_callback → restart (§6.3)
render:   direction Output, usage VoiceCommunication, same otherwise
```

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
| sidebar (channels, users, DMs) | Channels + Members pages, DMs in Channels |
| chat, history paging, unread, images inline, context menu | same (long-press opens the menu) |
| files, GIF picker, drag/paste | images only; the rest greyed with a tooltip string |
| voice: join, mute, deafen, per-user volume, device pickers | same, route control instead of pickers |
| settings window | Settings page — the desktop panel as a full-window page (D22, done); the phone subset and the route control come with the mobile root |
| admin panel, slash commands | slash commands only |
| share, viewers | **viewing** works: the desktop viewer panel as a full-window page, decoded by MediaCodec (D23); sharing from the phone is absent (no encoder, not stubbed in the UI) |
| tray, keybinds, updater, single instance | absent; the foreground notification is the tray |

## 10. The JNI surface and the Kotlin that remains

### 10.1 Kotlin (all of it)

`VoiceService.kt` (~60 lines): `onStartCommand` builds the
notification (channel `voice`, low importance, ongoing) and calls
`startForeground(id, notification, FOREGROUND_SERVICE_TYPE_MICROPHONE)`,
acquires a partial wake lock; `onDestroy` releases it. It has no logic;
the Leave action sends an intent the Rust side polls (M3).

`AndroidManifest.xml`: the permissions in §4.4; the activity is
`android.app.NativeActivity` with `android.app.lib_name =
baucord_android`, `launchMode="singleTask"`,
`windowSoftInputMode="adjustResize"`, the `configChanges` list from
§4.1, `exported="true"` with the launcher intent filter; the service
`dev.baucord.VoiceService` with `foregroundServiceType="microphone"`,
`exported="false"`; `allowBackup="false"`.

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
| `Intent(ACTION_VIEW, uri)` + `startActivity` | `Platform::open_url` |
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
`versionName` follows the workspace version.

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
running at nice 0 beside the UI and the video threads. The AAudio
callback thread itself is left alone (already audio-class). Next
step if a device still breaks up: low-latency performance mode +
`VoiceCommunication` usage, which needs our own AAudio builder (cpal
exposes neither without its `realtime` feature) — M3's audio work.
