# baucord — Android (v1) technical spec

Status: draft 1, 2026-09-15. Decisions in §1 are taken; the rest is
the plan they imply. Read `SESSION-GUIDE.md` §1 for how this codebase
is worked on; the runtime is in `ARCHITECTURE.md`. Every decision made
after this draft goes in §14, the way `PERSISTENCE-SPEC.md` §12 works.

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
    android/                  NEW  cdylib: android_main, JNI shims, Platform impl
  android/                    NEW  the Gradle project (no Rust in here)
    settings.gradle.kts
    build.gradle.kts
    gradle/ gradlew gradlew.bat
    app/
      build.gradle.kts
      src/main/AndroidManifest.xml
      src/main/kotlin/dev/baucord/VoiceService.kt     the one Kotlin file
      src/main/res/                                  icon, notification strings
      src/main/jniLibs/                              written by cargo-ndk, git-ignored
```

The Rust side of Android is a normal workspace member (`crates/android`)
so `cargo ndk … build -p android` and every rule in the session guide
apply to it unchanged. The Gradle project sits beside `crates/` because
it is not Rust and Cargo must not see it; it consumes what cargo-ndk
drops into `jniLibs`.

### 2.1 Workspace Cargo.toml

Two things to verify in M0 (the workspace file was not in the tarball):

- `members` includes the two new crates (a `crates/*` glob does).
- The `[workspace.dependencies] slint` entry must not turn on desktop
  backends for everyone. Whatever it enables today (`backend-winit`,
  `renderer-*`, `compat-*`) becomes `default-features = false` plus the
  platform-neutral features only; `desktop` adds its winit backend and
  renderers in its own `Cargo.toml`, `android` adds
  `backend-android-activity-06`. Cargo unifies features per build, so
  a desktop backend left in the workspace entry would be compiled into
  the Android library too.

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
| Android Studio | <https://developer.android.com/studio> | brings the SDK Manager, the JBR (a JDK 21), the emulator, `adb` |
| SDK Platform **36** (Android 16) | SDK Manager → SDK Platforms | `android.jar` for `compileSdk 36`; Slint's backend compiles its Java helper against it |
| Build-Tools **36.x** | SDK Manager → SDK Tools | `d8` for the same helper |
| NDK **r29** (or r28+) | SDK Manager → SDK Tools → NDK (Side by side) | r28+ aligns ELF segments to 16 KB by default |
| Platform-Tools | SDK Manager → SDK Tools | `adb` for sideloading and `logcat` |
| Emulator + a **Google APIs x86_64, API 36** system image | SDK Manager → SDK Tools / Virtual Device Manager | optional; the emulator can pass the host microphone through |
| Rust stable + targets | `rustup target add aarch64-linux-android x86_64-linux-android` | Rust ≥ 1.85 (cpal's MSRV) |
| `cargo-ndk` | `cargo install cargo-ndk` (<https://github.com/bbqsrc/cargo-ndk>) | 3.x; finds the newest NDK under `$ANDROID_HOME/ndk` on its own |
| a JDK | Android Studio's bundled JBR: `<studio>/jbr` | Gradle 8 needs 17+; Slint's backend `build.rs` needs `javac` |

Environment (persist them; Gradle, cargo-ndk and Slint's `build.rs`
all read them):

```
ANDROID_HOME      = <sdk root>            (…/Android/Sdk)
ANDROID_NDK_HOME  = $ANDROID_HOME/ndk/<r29 version>
JAVA_HOME         = <android-studio>/jbr
PATH             += $ANDROID_HOME/platform-tools
```

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

`lib.rs` — `android_main`: `android_logger` to logcat + the file
logger into `internal_data_path()/baucord.log` (the client log has
been the most useful artefact in every investigation; keep it),
`slint::android::init_with_event_listener`, the `OnceLock` engine,
`AndroidPlatform`, `frontend::run`. `jni.rs` — the calls in §10.
`audio_route.rs` — speaker/earpiece/Bluetooth through `AudioManager`.
No `.slint` files here; they live in `ui`.

## 6. Audio on Android

### 6.1 Backend

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
| settings window | Settings page: nickname, profile, voice activation threshold, noise suppression, language, log level |
| admin panel, slash commands | slash commands only |
| share, viewers | absent (not stubbed in the UI) |
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
# Rust → jniLibs (release, device ABI)
cargo ndk --target arm64-v8a --platform 29 \
  --output-dir android/app/src/main/jniLibs build --release -p android

# APK
cd android && ./gradlew assembleRelease
#   → app/build/outputs/apk/release/app-release.apk (signed with our key)

# install / logs
adb install -r app/build/outputs/apk/release/app-release.apk
adb logcat -s baucord
```

Debug loop: `--target x86_64` for the emulator, `assembleDebug`,
`adb install`. The Gradle `Exec` task makes the first two steps one.
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
| M0 | toolchain + hello: `crates/android` shows a Slint page with the theme on a phone; `ui` builds with `mobile`; the workspace `slint` features are platform-neutral | the APK runs on Bartrix's phone and the emulator; first-build time and the Skia prebuilt outcome are recorded in the guide |
| M1 | the split: `frontend` extracted, `Platform` trait, desktop rebuilt on it | desktop 1.1.5 ships with no behaviour change; `frontend` has no desktop dependency |
| M2 | connect + chat on Android: Connect, Channels, Chat, Members pages; history, unread, images | a DM and a channel conversation with desktop users work; `baucord.log` written; locale right |
| M3 | voice: AAudio backend, AEC, route control, foreground service, focus, permissions | 30 min in voice on speaker with the screen off, no echo reported by the others, telemetry shows the ladder healthy |
| M4 | resilience + settings: network change rebind, reconnect, Settings page, notifications for DMs in voice | Wi-Fi → cellular mid-call recovers under 10 s |
| M5 | release: signing, `versionCode`, README section, the guide's Android rules | the three Movistar users run it for a week |

Later (v2 candidates, in rough order): viewing shares (MediaCodec H.264
decode), file cards with Save, `ConnectivityManager` callback, Leave
action from the notification, tablet layout, in-app APK update,
Bluetooth SCO on API 29–30, push-to-talk on hardware buttons.

## 13. Risks, and how each is retired

| risk | when | how |
|---|---|---|
| Skia has no prebuilt for our NDK/target and builds from source (30+ min, disk) | M0 | try; if so, cache the build or pin `SKIA_BINARIES_URL` |
| Slint's Android IME: composition, autocorrect, Spanish accents in the composer | M2 | type real text on a real phone in the first chat build |
| AEC quality varies by OEM; some need `MODE_IN_COMMUNICATION` before the streams open | M3 | open streams after `setMode`; test on ≥ 3 makers |
| `sys-locale`, `dirs`, `interprocess` or another desktop-shaped crate behind a `frontend` dependency | M1 | `cargo tree -p android --target aarch64-linux-android` must show none of desktop's |
| the 1 ms network wake floor on battery | M2 | measure with Battery Historian / `dumpsys batterystats` |
| GSO on Android kernels | M2 | `quinn-udp` probes; if the probe misbehaves, cap `max_gso_segments` to 1 under `cfg(target_os = "android")` |
| Android kills the process in voice despite the service (OEM battery managers) | M3 | test on the OEMs at hand; document the "unrestricted battery" setting in the README |
| DSCP EF on cellular | — | expected to be stripped; no effect on correctness |

## 14. Decisions taken after this draft

(none yet)
