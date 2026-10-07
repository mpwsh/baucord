# baucord — iOS (v1) technical spec

Status: draft 1, 2026-10-06; M0 written the same day and running in
the iOS 18.3 simulator the same evening — the hello page (§14 D28),
then the client, connected (§14, after D28); stream viewing and the
file pickers pulled forward the same night (D30, D31), awaiting their
first run. Decisions in §1 are taken; the rest is the
plan they imply. Read `SESSION-GUIDE.md` §1 for how this codebase is
worked on. `ANDROID-SPEC.md` is this spec's older sibling: almost
everything the Android port learned — the `frontend` split, the
single-window host, the narrow layout, the Settings and stream pages,
touch scrolling, the keyboard insets, the echo-cancellation contract —
applies here unchanged, and this document only says where iOS differs.
Every decision made after this draft goes in §14.

## 0. Purpose and scope

An iPhone client for the existing baucord server: **voice and chat**,
with a **Slint** UI at parity with the desktop and Android clients
(same widgets, theme, strings and behaviour), running the **same Rust
engine**. As much as possible is Rust: there is no Swift or
Objective-C in the app at all (D2). The desktop crate is the reference
for behaviour; the Android host is the template for the host.

In scope for v1: connecting with an invite or a saved server, the
roster and channels, joining voice with the platform's echo
cancellation, mute/deafen and the audio route, text chat in channels
and DMs (send, receive, history paging, unread counts, images inline,
attachments through the system pickers), viewing a screen share, the
Settings page, staying in voice with the screen locked, reconnect on
network change, the file log, telemetry.

Not in v1: sharing the phone's screen (a ReplayKit broadcast
extension), push notifications (APNs needs server work and a paid
account), CallKit, iPad layouts, the admin panel, keybinds and
push-to-talk, the updater (TestFlight updates the app), iOS before 17,
Intel Macs' simulator. Each is a later milestone (§12) or a non-goal.

## 1. Decisions

| # | decision | why |
|---|---|---|
| D1 | UI in Slint through its **winit backend with the Skia renderer** — Slint's iOS support; Skia draws through wgpu 30's Metal backend (Slint 1.18's default surface order: wgpu, then the CPU) | one UI codebase; the `frontend` glue shared with desktop and Android |
| D2 | **No Swift, no Objective-C.** Platform APIs are called from Rust through the `objc2` framework crates (`objc2-ui-kit`, `objc2-avf-audio`, `objc2-foundation`, …); the Xcode project is generated (XcodeGen) and has one build phase that runs cargo | Android needed Java because `NativeActivity` cannot receive an activity result and its methods cannot be reached from native code; on iOS `objc2`'s `define_class!` declares Objective-C classes — picker delegates, observers — in Rust, so nothing forces a second language. The `objc2` 0.6 / 0.3 family is already in the tree through cpal and Slint's iOS backend |
| D3 | v1 = voice + chat + viewing shares | Android's v1 as it stands on 2026-10-06 (viewing landed there early, D23) |
| D4 | **iOS 17.0** minimum | the voice-processing ducking control (§6.3) and `AVAudioApplication` are 17+; iOS 17 runs on every iPhone from the XS; iOS 27 (released 2026-09-14) needs an A13 |
| D5 | **iPhone only, arm64 only** — devices and the simulator on Apple silicon | every iPhone on iOS 17 is arm64; iPad later (it runs the app in its iPhone frame meanwhile); no `x86_64-apple-ios` build |
| D6 | **Slint 1.18.1 for the whole workspace** (done, §14 D14) | built with the iOS 27 SDK (Xcode 27), an app that has not adopted the UIScene life cycle does not launch (TN3187); Slint 1.18 provides the scene delegate (`SlintWindowSceneDelegate`); 1.18.1 fixes iOS apps rendering in software instead of Metal, an intermittent black screen at start, and the 60 Hz cap |
| D7 | The iOS host selects **winit + Skia** itself; the frontend's own choice (Skia on the CPU) is compiled out on iOS | that choice keeps Windows Game Mode from classing baucord as a game; on a phone it would cost battery and ProMotion's 120 Hz |
| D8 | Echo cancellation = Apple's **Voice-Processing I/O unit (VPIO)**, a native backend `audio/src/native/vpio.rs` on `coreaudio-rs` (already in the tree through cpal), written for iOS and then reused on macOS | AEC-SPEC.md D1 (platform AEC first); VPIO is also that spec's planned macOS route (§9, E7) — one backend serves both |
| D9 | Stream viewing = **VideoToolbox** (`VTDecompressionSession`) as a fourth `VideoDecoder` | hardware H.264 with no FFmpeg for iOS (`video` already swaps in `codec_stub.rs` there), the way MediaCodec did it on Android (D23 there) |
| D10 | Distribution: **development installs from Xcode** while building (a free Apple ID works: 7-day profiles, 3 apps per device); **TestFlight** to give it to anyone else (the paid Developer Program); no App Store in v1 | iOS has no APK equivalent; TestFlight is the closest thing to "send people a build" |
| D11 | The app icon is the repo's **`assets/icons/icon.png`**, rendered at build time into one 1024 px asset (`sips`) | Android's D36 for iOS: one picture for every platform |
| D12 | Bundle id **`dev.baucord`** (overridable per machine in `Config/Local.xcconfig`), display name "baucord" | the Android `applicationId`; bundle ids are global across Apple accounts, so a developer whose id is taken sets their own |
| D13 | **Staying connected:** while in voice, the `audio` background mode keeps the process running (Android's foreground service, D34 there); a chat-only session is suspended with the app and reconnects when it comes back | iOS has no sanctioned way to keep a chat socket alive in the background; push notifications are the real answer (v2) |

Open (decide before M3): CallKit in v1 or later — proposal: later (§12).
Whether RNNoise stays on under VPIO, which suppresses noise itself —
the same question as Android's D-open-2; proposal: same answer.

## 2. Where things live

```
baucord/
  crates/
    ios/                      bin crate (`baucord`): main.rs, logging.rs, paths.rs,
                              session.rs, lifecycle.rs, hello.rs  (§5.1)
    audio/src/native/vpio.rs  M3: VPIO, iOS then macOS (§6.3)
    video/src/videotoolbox.rs M4: VideoToolbox decode (§10)
  ios/                        the Xcode project (no Rust, no Swift)
    project.yml               XcodeGen: target, Info.plist keys, build phases
    Config/Base.xcconfig      shared settings; includes Config/Local.xcconfig
    Config/Local.xcconfig.example   your signing team / bundle id (copy it)
    scripts/build_with_cargo.sh     the cargo build phase
    scripts/render_icon.sh          ../assets/icons/icon.png → AppIcon
    Resources/Assets.xcassets       the icon (rendered) and the launch colour
    run.sh                    generate, build, install, launch, stream the log
    README.md                 setup on the Mac, first run, the phone, logs
```

`crates/ios` is a normal workspace member, so every rule in the session
guide applies to it unchanged. The Xcode project sits beside `crates/`
like `android/` does; `xcodegen generate` writes `baucord.xcodeproj` and
`Generated/Info.plist` from `project.yml`, and neither is committed.

### 2.1 Workspace

- **Always build with `-p ios`**, as Android's `-p android`: its
  `frontend` dependency has `default-features = false`, and a
  workspace-wide build would unify the desktop's features back in.
- The workspace `slint` entry (`backend-winit`, `renderer-skia`,
  `renderer-software`, `compat-1-2`) is exactly Slint's iOS support;
  nothing is added for iOS. `cargo tree -p ios --target
  aarch64-apple-ios` shows no desktop crate — no FFmpeg, xcap, rfd,
  arboard, global-hotkey or interprocess (checked 2026-10-06). The C
  builds are libopus (cmake), ring's assembly, and Skia's prebuilt.

### 2.2 A binary, not a library

Android loads a `cdylib`; an iOS app is an executable. Slint's iOS
support runs an ordinary `fn main`: winit calls `UIApplicationMain`
from inside the event loop and never returns. cargo builds
`baucord` for `aarch64-apple-ios` (or `-sim`), and the build phase
copies it to where Xcode expects the bundle's executable (§11). No
naming trap like `libandroid.so`; the binary is `[[bin]] name =
"baucord"`.

## 3. Toolchain: what to install, from where

| what | where | notes |
|---|---|---|
| Xcode **16 or later** (27 is current) | App Store | the iOS SDK, the simulator, signing, `xcodebuild`, `xcrun devicectl` (device install and log); open it once to install the iOS platform. Nothing in the project needs a newer Xcode for the simulator, and any iPhone simulator on iOS 17 or later runs the app (D4) — the user's iOS 18.1 one included (D24). The phone may need a newer Xcode (one that knows the phone's iOS); App Store Connect takes uploads only from Xcode 26 or later (since 2026-04-28) |
| Xcode selected | `sudo xcode-select -s /Applications/Xcode.app` | otherwise `xcodebuild` is the bare Command Line Tools' |
| Xcode's own iOS platform | `xcodebuild -downloadPlatform iOS` (or Settings → Components) | Xcode 15+ builds for no iOS destination until the platform matching its SDK is installed; older simulator runtimes do not count (D26) |
| Homebrew | <https://brew.sh> | for the next two |
| XcodeGen | `brew install xcodegen` | the project from `project.yml` — Slint's documented route for Rust apps |
| CMake | `brew install cmake` | `opusic-sys` builds libopus with it; cmake-rs passes the iOS system name and SDK |
| Rust + the iOS targets **for the pinned toolchain** | rustup; then inside the repo `rustup target add aarch64-apple-ios aarch64-apple-ios-sim` | `rust-toolchain.toml` pins 1.95; running inside the repo adds the targets to that toolchain |
| An Apple ID in Xcode | Xcode → Settings → Accounts | free = "Personal Team", enough for your own phone (D10) |
| The iPhone | Developer Mode on (Settings → Privacy & Security), the Mac trusted | |

First-build notes: Skia comes as a prebuilt download, as on Android —
`skia-bindings` 0.153.3 publishes `aarch64-apple-ios` and
`aarch64-apple-ios-sim` binaries with Metal, and both downloaded in the
sandbox on 2026-10-06 (§14). The first build then compiles the whole
engine for iOS: minutes.

## 4. Runtime architecture on iOS

### 4.1 Process, scene, background

- UIKit starts `baucord`'s `main` (`crates/ios/src/main.rs`): the log,
  the panic hook, the app's folders, the audio session category, the
  lifecycle observers, the Slint backend, then `frontend::app::run()`
  — the same function the desktop and Android hosts call. Inside it
  Slint starts winit, winit calls `UIApplicationMain`, and the call
  never returns. **Nothing may wait for "after the loop"**: the
  desktop's `shutdown()` never runs; the host saves on the way into
  the background instead (§4.3).
- One process, one scene, one window: the Info.plist's scene manifest
  names `SlintWindowSceneDelegate` (the UIScene life cycle, required
  to launch when built with the iOS 27 SDK) and does not support
  multiple scenes. The frontend treats
  iOS as a **single-window touch host** exactly like Android
  (`SINGLE_WINDOW_HOST`), so the Settings page (D22 there), the
  stream page (D23), the narrow layout (D26), touch scrolling (D27)
  and the keyboard insets (D35) all apply as they are.
- **In voice** the audio session is active and the `audio` background
  mode (Info.plist `UIBackgroundModes`) keeps the process running with
  the screen locked or another app in front: the network thread and
  the voice worker carry on. This is D34's foreground service, with no
  notification to manage — iOS shows its own microphone indicator.
- **Chat only**, iOS suspends the app a few seconds after it leaves the
  screen; the connection times out at the server, and on return the
  existing reconnect path reconnects (the desktop's backoff). M4 adds a
  background task (`beginBackgroundTask`, ~30 s) so whatever is in
  flight when the user switches away still goes out. DMs that arrive
  while suspended appear when the app comes back (history), not as
  notifications — that is push (v2).

### 4.2 Threads

Same as desktop and Android: Slint on the main thread (UIKit's), the
network thread, the voice worker, plus CoreAudio's own real-time I/O
thread for the audio callbacks — which is already time-constraint
scheduled, so the callback is not promoted (`audio/src/platform.rs`
treats iOS like macOS since 2026-10-06). The network loop's idle wake
floor matters on a phone as on Android (§4.2 there).

UIKit is main-thread only. Hooks are called from Slint callbacks (the
main thread); anything a worker needs from UIKit goes through
`slint::invoke_from_event_loop`. `objc2`'s `MainThreadMarker` makes the
rule a type error rather than a crash.

### 4.3 Lifecycle events

winit receives UIKit's transitions but Slint does not pass them to the
application, so `crates/ios/src/lifecycle.rs` observes the
notifications itself, on the main queue:

- **Scene did enter background** — `frontend::app::persist_for_background()`
  (new, 2026-10-06: what the tray's Quit does — settings now) and a
  log flush. Background/foreground are the *scene's* notifications:
  TN3187 moves those events to the scene delegate under the scene life
  cycle; the application's notifications are still documented, but
  the scene's are the ones that life cycle is about.
- **Scene will enter foreground** — logged; M4: rebind the socket if
  the network changed, re-check the microphone permission.
- **Application memory warning** — logged; M4: drop the image caches
  (app-core bounds them already).
- **Application will terminate** — persist, flush (only delivered to a
  running app, not a suspended one).
- Audio session interruptions and route changes are `session.rs`'s
  (§6.5).

### 4.4 Permissions

Every permission is an Info.plist string the system shows in its own
prompt; v1 localizes them with `InfoPlist.strings` in en/es/pt-BR
(M2).

- **Microphone** (`NSMicrophoneUsageDescription`, in M0): the system
  asks the first time a stream opens the microphone, and the engine
  retries a pending input every 2 s meanwhile (Android's fix). M3 asks
  explicitly at the first voice join (`AVAudioApplication
  requestRecordPermission`), so the prompt is not racing the stream;
  a refusal gets a toast with a link to the app's page in Settings
  (`UIApplicationOpenSettingsURLString`).
- **Local network** (`NSLocalNetworkUsageDescription`, in M0): iOS asks
  the first time the app sends to an address on the local network (a
  server on 192.168.x.x). Refused, those servers are silently
  unreachable — the connect error must say so (M2).
- **Photos**: none for picking (`PHPickerViewController` runs out of
  process); saving a picture into Photos would need
  `NSPhotoLibraryAddUsageDescription` (M2, only if offered).
- **Notifications**: only with M4's local notification.
- No camera, location, contacts or tracking.

## 5. The crates

No new split: `frontend` without its `desktop` feature (the `stubs/`
modules, the `host::Hooks` seam), `ui` with the same roots (the mobile
root, when it comes, serves Android and iOS alike — D14 there),
`client`, `audio`, `video`, `transport` unchanged in shape.

### 5.1 `crates/ios` (M0)

- `main.rs` — the entry above; off iOS it prints where it belongs and
  exits, so a host `cargo check -p ios` validates the manifest.
- `paths.rs` — `NSFileManager`'s Application Support (`baucord/`:
  settings, identity) and Documents (the log); the config folder is
  excluded from backups (§8).
- `logging.rs` — Android's tee with stderr in place of logcat: stderr
  is what Xcode's console, `simctl launch --console-pty` and
  `devicectl … --console` show; `baucord.log` in Documents, rolled at
  5 MB.
- `session.rs` — M0's audio session: `PlayAndRecord` with the
  loudspeaker and Bluetooth (§6.1).
- `lifecycle.rs` — the observers of §4.3.
- `hello.rs` (feature `hello`) — the M0 smoke test: a page in the
  theme's colours, inside the safe area, with a tap counter.

- `host.rs` (2026-10-06, D31) — the `Hooks` of §10: the Photos and
  Files pickers behind an action sheet, saving into
  `Documents/Downloads`, Quick Look, links.

To come: the audio route, the background task.

### 5.2 What M0 changed outside `crates/ios`

- `frontend`: `SINGLE_WINDOW_HOST` includes iOS; `select_renderer()`
  and the saved window size are compiled out on iOS (D7);
  `persist_for_background()` added.
- `audio/src/platform.rs`: the CoreAudio callback thread is left alone
  on iOS, as on macOS — on iOS the promotion guard has no `Drop`, and
  the `mem::forget` was a clippy error.
- `transport/src/udp.rs`: socket2 declares `set_tclass_v6` for macOS
  but not iOS, so the IPv6 DSCP mark is skipped there (a build error
  otherwise; §7 has the iOS way).
- The workspace moved to Slint 1.18.1 (D6, §14 D14).

## 6. Audio on iOS

### 6.1 M0: cpal's RemoteIO, in a recording category

cpal 0.18's iOS host opens a RemoteIO unit through `coreaudio-rs`
and never touches the audio session's category. The default category
(`SoloAmbient`) has no microphone, so the host sets **`PlayAndRecord`**
at startup with `DefaultToSpeaker` (otherwise `PlayAndRecord` plays to
the earpiece and the app seems silent), `AllowBluetoothHFP` and
`AllowBluetoothA2DP`. Nothing is activated at startup; the session
becomes active when the first stream starts — which is when other
apps' music stops — and that is also when the microphone prompt
appears. RemoteIO has **no echo cancellation**: M0–M2 voice tests use
headphones, as Android's did before its call path.

### 6.2 The session (M3)

- Category `PlayAndRecord`, mode **`voiceChat`**, options
  `DefaultToSpeaker | AllowBluetoothHFP | AllowBluetoothA2DP`.
- **Activated on voice join**, deactivated on leave with
  `notifyOthersOnDeactivation`, so a podcast paused by joining resumes
  after leaving. While not in voice the session stays inactive.
- Preferred sample rate 48 kHz and IO buffer duration 10 ms; the
  hardware decides, the backend reports what it got (the workers
  resample, and `output::Cushion` sizes the queue from the callback,
  D24 there).
- The volume buttons control the call volume in this mode (the AEC
  hint string says so, as Android's does).

### 6.3 Echo cancellation: the VPIO backend

`audio/src/native/vpio.rs`, the fourth implementation of
`native/mod.rs`'s contract beside `wasapi.rs` and `aaudio.rs`:

- **One unit, both directions**: `kAudioUnitSubType_VoiceProcessingIO`
  (`coreaudio_rs::audio_unit::IOType::VoiceProcessingIO`), input
  enabled on bus 1, output on bus 0, client format 48 kHz mono `f32`
  on both (the unit converts to and from the hardware). The input
  callback pulls the processed microphone with `AudioUnitRender` into
  a preallocated buffer → the `rtrb` ring → the wake latch; the render
  callback pops from its ring and counts glitches into `AudioStats` —
  the same contract as `aaudio.rs`. Since one unit owns both
  directions, capture and render open and close together, which
  `VoiceChat::plan_aec` already does on the call path (AEC-SPEC E3).
- `kAUVoiceIOProperty_BypassVoiceProcessing` = 0 when `wants_aec`;
  AGC left on; **`kAUVoiceIOProperty_OtherAudioDuckingConfiguration`**
  (iOS 17+) with advanced ducking at the minimum level, so other apps'
  audio is not crushed while in voice; `MutedSpeechActivityEventListener`
  later, for a "you are muted" hint.
- Not `wants_aec` (headphones, or Off): cpal's RemoteIO exactly as in
  M0 — Android's split between the call path and the media path (E4
  there).
- The `AecReport`: tier Platform when the unit initialised as VPIO with
  processing on; the output kind from the route (§6.4); the same
  `/audio` words as every other OS.
- Every constant is in `objc2-audio-toolbox` 0.3.2, already locked
  (checked 2026-10-06).
- **macOS reuses the file** with the device chosen through
  `kAudioOutputUnitProperty_CurrentDevice` — AEC-SPEC.md §9's macOS
  item, and the canceller Mac users on built-in speakers do not have
  today. It is listed under Later (§12) so it can be pulled forward on
  its own.

### 6.4 Routes

There is no device list; the route is the session's. `audio::route`
(platform-free, Android's D31) decides with Android's device kinds, so
iOS maps its ports onto them and reuses the decision function and the
Settings picker unchanged:

| `AVAudioSessionPort` | `route::kinds` |
|---|---|
| BuiltInSpeaker | `BUILTIN_SPEAKER` |
| BuiltInReceiver | `BUILTIN_EARPIECE` |
| Headphones (+ HeadsetMic input) | `WIRED_HEADPHONES` (`WIRED_HEADSET`) |
| BluetoothHFP | `BLUETOOTH_SCO` |
| BluetoothA2DP | `BLUETOOTH_A2DP` |
| BluetoothLE | `BLE_HEADSET` |
| USBAudio | `USB_HEADSET` |
| CarAudio, AirPlay, HDMI | treated as speakers |

Applying a choice: the loudspeaker → `overrideOutputAudioPort(.speaker)`;
the earpiece → no override, built-in microphone preferred; a headset →
`setPreferredInput` with its input port (iOS moves the output with it
for wired and HFP headsets). An A2DP-only output cannot be chosen by an
app — the system's route picker (`AVRoutePickerView`) is the iOS way,
a button for later. `AVAudioSessionRouteChangeNotification` re-plans,
as Android's route callback does.

### 6.5 Interruptions and resets

`AVAudioSessionInterruptionNotification` *began* (a phone call, Siri,
an alarm): the unit has stopped; the voice bar says "paused — call"
(Android §6.5's string). *Ended* with `shouldResume`: reactivate and
reopen both streams through the worker's device-loss path.
`AVAudioSessionMediaServicesWereResetNotification`: rebuild the unit
and the session from scratch.

### 6.6 Processing chain

Unchanged (RNNoise → VAD → Opus, the ladder, RED/FEC, the jitter
buffer). RNNoise under VPIO: the open question of §1.

## 7. Networking on iOS

- The transport is the transport: quinn over std UDP sockets; GSO is
  not offered on iOS and `quinn-udp`'s probe handles that.
- **DSCP**: IPv4 is marked as everywhere; IPv6 is unmarked since M0
  (§5.2). The iOS way is one `setsockopt(SOL_SOCKET,
  SO_NET_SERVICE_TYPE, NET_SERVICE_TYPE_VO)`, which also puts the
  traffic in Wi-Fi's voice access category — M4, for both families.
  Both constants are public in xnu's `sys/socket.h` (0x1116 and 4) but
  absent from the `libc` crate, so `transport` declares them.
- **Network change** (Wi-Fi ↔ cellular): the existing loss/timeout
  reconnect, plus Android v1's check on return to the foreground
  (rebind if the local address moved), M4. `NWPathMonitor` (a C API;
  no objc2 crate covers it) is v2.
- **IPv6-only networks (NAT64)**: App Review tests on them. An invite
  with an IPv4 literal is parsed by std without `getaddrinfo`, so it
  never gets the synthesized IPv6 address and fails there. M4 resolves
  literals through `getaddrinfo` on iOS.
- **App Transport Security** governs `URLSession` only. The QUIC
  transport and `ureq` (rustls) — the link-image previews in
  `frontend/src/media.rs` — open their own sockets and are not
  subject to it.
- Port 51000 by default, as everywhere.

## 8. Config, identity, logs

- **Application Support/baucord** (`paths.rs`): `settings.json` and the
  device identity key, set with `client::config::set_config_dir`.
  Excluded from iCloud and computer backups
  (`NSURLIsExcludedFromBackupKey`): a restore onto a second iPhone
  would give the server two devices with one key — Android's
  `allowBackup="false"`. The Keychain with a `ThisDeviceOnly`
  accessibility is the more iOS-native home for the key; v2, with a
  migration.
- **Documents**: `baucord.log` (+ `.1`), and the files users save
  (§10). `UIFileSharingEnabled` + `LSSupportsOpeningDocumentsInPlace`
  make it the app's folder in the Files app ("On My iPhone ›
  baucord"), so a user can send the log without a Mac.
- stderr: Xcode's console, `ios/run.sh`'s stream.
- Locale: `sys-locale` reads the preferred languages through
  CoreFoundation on iOS, as it read the right one on Android; verify
  in M1. `CFBundleLocalizations` lists en, es and pt-BR with the
  permission strings (M2).

## 9. The UI on iOS

- **The same `.slint` as Android**, with everything §4.1 lists. Slint's
  iOS backend fills `safe-area-insets` (the notch, the home indicator)
  and `virtual-keyboard-position/size` (from the keyboard's frame,
  unified with Android's inset model), so D35's page rectangle works
  as is; the keyboard's slide is animated by the backend. Slint 1.18
  also makes touch panning always possible, independent of
  `mouse-drag-pan-enabled`.
- **No Back key.** Android's Back reaches the UI's handlers; iOS has
  none, so every page needs its on-screen way back — the chat header's
  ←, the Settings ←, the stream page's *Stop watching*, tap-outside
  for menus and dialogs. M1 walks every page to confirm. The
  edge-swipe back gesture is not something Slint gets from UIKit; a
  left-edge drag in `.slint` is a later nicety.
- **Text input is basic.** winit 0.30's iOS view implements
  `UIKeyInput` only: typing, emoji, accents from a long press, Return
  and backspace work; autocorrect replacements, marked text (Chinese
  and Japanese input methods), the space-bar cursor drag and the
  system's selection handles do not (Slint draws its own selection).
  Spanish and Portuguese typing works this way. If autocorrect matters
  to users, the fix is `UITextInput` upstream in winit, or a native
  text view over the composer as a fallback (§13).
- **Look**: dark system UI (`UIUserInterfaceStyle Dark`: dark keyboard
  and sheets), light status-bar text, the launch screen in the app's
  background colour (no white flash), 120 Hz on ProMotion iPhones
  (`CADisableMinimumFrameDurationOnPhone` + Slint 1.18.1). Text is in
  the system font through CoreText; emoji are Apple's, which Skia
  renders.
- Rotation: portrait and both landscapes; the width rule (D26 there)
  gives a phone on its side the wide layout.

### 9.1 Parity table (v1)

| desktop | iOS v1 |
|---|---|
| sidebar, chat, history, unread, images inline, context menu | as Android: the narrow layout's two pages, long-press for the menu |
| files, GIF picker, drag/paste | attachments through the photo and document pickers, saving into the app's Documents (visible in Files) or through the share sheet, Quick Look to open; paste of text through Slint's clipboard |
| voice: join, mute, deafen, per-user volume, device pickers | same; the output picker is the route control (§6.4); echo cancellation through VPIO (§6.3); no push-to-talk |
| settings window | the Settings page, as Android |
| admin panel, slash commands | slash commands only (Android's D16 toast covers the panel) |
| share, viewers | viewing through VideoToolbox (M4); sharing absent (the button hidden on a touch host) |
| tray, keybinds, updater, single instance | absent; TestFlight updates the app |

## 10. The platform surface (Rust → Objective-C, through objc2)

Android's JNI table, for iOS. Everything is called on the main thread.

| call | for | when |
|---|---|---|
| `AVAudioSession` category | §6.1 | M0, done |
| `NSFileManager`, `NSURL` resource values | §8 | M0, done |
| `NSNotificationCenter` (scene, application) | §4.3 | M0, done |
| `AVAudioSession` mode, activation, route, override, preferred input, notifications | §6.2–6.5 | M3 |
| `AVAudioApplication` record permission | §4.4 | M3 |
| VPIO through `coreaudio-rs` | §6.3 | M3 |
| `UIApplication openURL:options:completionHandler:` | `Hooks::open_url` | written 2026-10-06 (D31) |
| `PHPickerViewController` (photos), `UIDocumentPickerViewController` (any file, `asCopy`) behind a `UIAlertController` action sheet, presented from the key window's top view controller; their delegates declared in Rust (`define_class!`) | `Hooks::pick_file` → the same `attach_path` as the desktop dialog | written 2026-10-06 (D31) |
| `Documents/Downloads` | `Hooks::save_download`, a file card's Save | written 2026-10-06 (D31); a share sheet straight from Save (into Photos, AirDrop) is still M2 |
| Quick Look through `UIDocumentInteractionController` (share sheet for what it cannot preview) | `Hooks::open_target` | written 2026-10-06 (D31) |
| `UIPasteboard` | Slint's iOS clipboard (built in) | done |
| `VTDecompressionSession`, `CMVideoFormatDescriptionCreateFromH264ParameterSets` (`objc2-video-toolbox`, `objc2-core-media`, `objc2-core-video`): Annex-B → length-prefixed samples (`video::annexb`), output as BGRA, `yuv::bgra_to_rgba` | stream viewing (D9) | written 2026-10-06 (D30) |
| `beginBackgroundTask` / `endBackgroundTask` | `Hooks::keep_alive` on iOS: finish what is in flight | M4 |
| `UNUserNotificationCenter` | a DM while in voice (Android's M3 item) | M4 |
| `setsockopt SO_NET_SERVICE_TYPE` | §7 | M4 |

Everything else the app does is Rust.

## 11. Build, sign, install, distribute

```sh
ios/run.sh --hello        # M0 smoke test, simulator
ios/run.sh                # the client, simulator (a booted iPhone, else the newest iOS)
ios/run.sh --os 18.1      # an iPhone simulator on that iOS (D24)
ios/run.sh --device       # the client on the iPhone (needs Config/Local.xcconfig)
ios/run.sh --release | --sim "iPhone 16" (name or UDID) | --no-build | --no-log | --open
```

`run.sh` regenerates the project (XcodeGen), builds with `xcodebuild`
(on a device with `-allowProvisioningUpdates
-allowProvisioningDeviceRegistration`, so Xcode makes the development
profile itself and registers a phone the team has not seen), installs
and launches with `xcrun simctl` or `xcrun devicectl` (the first paired
*iPhone*: iPads report the platform "iOS" too), and streams stderr
until Ctrl+C. It is written
for macOS's bash 3.2 and was exercised under a bash 3.2.57 build
against stand-in Xcode tools (§14). `--open` opens Xcode instead,
where ⌘R and the debugger work.

The project (`ios/project.yml`): an application target whose only
source is the asset catalog; one pre-build phase renders the icon
(D11); one post-compile phase, `scripts/build_with_cargo.sh` (adapted
from Slint's script), maps `PLATFORM_NAME` to the Rust triple, refuses
anything but arm64, checks cargo, cmake and the installed targets with
Xcode-readable `error:` lines, runs `cargo build -p ios --bin baucord
--target …` into `target/xcode/` (apart from a terminal's builds,
whose environment differs), copies the binary into the bundle, makes a
dSYM where the configuration asks for one (Release) and signs device
builds. Both phases call their scripts through `bash`, so a checkout
that lost the executable bit (a commit from Windows) still builds.
`BAUCORD_CARGO_FEATURES=hello` (a build setting, which `run.sh` always
passes) builds the smoke test.

**The version** is the crates' (D25): `run.sh` writes
`Config/Version.xcconfig` on every run — `MARKETING_VERSION` from
`crates/ios/Cargo.toml`, `CURRENT_PROJECT_VERSION` = the commit count,
which only grows, as TestFlight requires — and `Base.xcconfig`
includes it; the cargo phase stops the build when the bundle's version
and the crate's disagree.

The Info.plist (from `project.yml`): the version and build number
from `MARKETING_VERSION` / `CURRENT_PROJECT_VERSION` (XcodeGen would
otherwise write "1.0" and "1"), the scene manifest, the launch colour,
the orientations, dark style and light status bar, 120 Hz, the `audio`
background mode, the microphone and local-network strings, the
Documents folder in Files.

**Signing.** Automatic. `Config/Base.xcconfig` includes the uncommitted
`Config/Local.xcconfig`, which sets `DEVELOPMENT_TEAM` (and
`BAUCORD_BUNDLE_ID` when `dev.baucord` is taken). A free Apple ID's
profiles last 7 days and allow 3 apps per device; the first launch
asks the user to trust the developer on the phone. The Developer
Program ($99/year) gives one-year profiles and TestFlight.

**TestFlight (M5).** `xcodebuild archive`, then `-exportArchive` with
`method = app-store-connect` and `destination = upload` and an App
Store Connect API key — Slint's own flow for its viewer. Before the
first upload: the icon without an alpha channel (App Store Connect
refuses one; `render_icon.sh` keeps transparency today), a privacy
manifest (`PrivacyInfo.xcprivacy`) declaring the "required reason"
APIs the binary uses — decided from `nm -u` on the built executable;
expected: file timestamps (`stat`), system boot time (cpal's iOS host
calls `mach_absolute_time`; std's `Instant` uses `clock_gettime`, which
is not on the list), disk space if `sysinfo` reads it — and the
`ITSAppUsesNonExemptEncryption` answer (standard TLS 1.3 through
rustls; the user's call). The build number already grows with every
commit (D25).

CI: none in v1 (Android's D21 stance: testing is local). If wanted,
Slint's model fits — CI builds an unsigned archive on a macOS runner,
signing stays on the Mac.

## 12. Milestones

| M | deliverable | done when |
|---|---|---|
| M0 ✍ 2026-10-06, hello ✓ and the client ✓ in the simulator (D28, §14) | toolchain + hello + the client in the simulator: `crates/ios`, `ios/`, `run.sh`, Slint 1.18.1 | `run.sh --hello` shows the page in the simulator and on the phone; `run.sh` connects to a server from the simulator — the narrow layout, chat, voice received (headphones) |
| M1 | the client on the iPhone: signing, the narrow layout at the phone's size, safe areas, rotation, typing in the composer (accents, emoji), every page's way back, `sys-locale` | a DM and a channel conversation from the iPhone |
| M2 | files and links (the `Hooks` of §10 — written early, D31), localized permission strings, the local-network hint, the iOS hint strings | attach a photo and a file; open a received file; a link opens Safari |
| M3 | voice: the session lifecycle, VPIO, routes, interruptions, the record permission at join | 30 min in voice on the loudspeaker with the screen locked, no echo reported by the others; a phone call pauses and resumes it |
| M4 | viewing (VideoToolbox — written early, D30); resilience: background task, network change, NAT64, `SO_NET_SERVICE_TYPE`; the local notification for a DM in voice | Wi-Fi → cellular recovers in under 10 s; a 1080p share plays |
| M5 | release: TestFlight, privacy manifest, icon without alpha, build numbers, README | testers install it from TestFlight |

Later (v2 candidates, roughly in order): VPIO on macOS (AEC-SPEC E7),
push notifications (APNs + the server), CallKit, an `AVRoutePickerView`
button, the Keychain for the identity key, an edge-swipe back,
`NWPathMonitor`, iPad layouts, sharing the phone's screen (a ReplayKit
broadcast extension: a separate process with a ~50 MB memory limit and
VideoToolbox encoding).

## 13. Risks, and how each is retired

| risk | when | how |
|---|---|---|
| Skia has no prebuilt for the iOS targets | M0 | **retired 2026-10-06**: the simulator build linked the `-sim` prebuilt and drew with it (D28); the device prebuilt was downloaded in the sandbox, `--device` links it |
| The engine does not compile for iOS | M0 | type-checked and clippy-clean for `aarch64-apple-ios` in the sandbox (§14 D15), which found and fixed two errors (§5.2); the real compile of libopus and ring, and the link, are the Mac's |
| libopus's cmake build for iOS | M0 | cmake-rs sets `CMAKE_SYSTEM_NAME=iOS` and the SDK; if it fails, a `CMAKE_TOOLCHAIN_FILE` or building opus with cc — the first build says |
| Xcode's environment leaking into cargo's host builds (`SDKROOT`, `PATH`) | M0 | Slint's script lineage, the PATH fix, a separate target directory. The hello build's build scripts (Slint's compiler, skia-bindings) ran clean under Xcode (D28); the app adds C builds for the target (libopus, ring), which the next build shows |
| Background/foreground notifications under the scene life cycle | M0 | `lifecycle.rs` observes the scene's; the first run's log must show `lifecycle: background` when the app is swiped away |
| Text input limited to `UIKeyInput` | M1 | type Spanish and Portuguese on the phone; autocorrect is the gap — upstream `UITextInput`, or a native text view over the composer |
| VPIO side effects: quieter loudspeaker, ducking, 48 kHz | M3 | test on the user's iPhone; the ducking configuration; measure |
| Chat-only sessions miss DMs while suspended | v1, by design | reconnect on return; push in v2 |
| Free provisioning expires every 7 days | ongoing | the paid program before anyone else tests |
| IPv6-only networks | M4 | `getaddrinfo` for literals |
| App Store Connect's privacy-manifest and icon checks | M5 | §11 |
| Skia falls back to the CPU on iOS without telling the app (Slint 1.18: wgpu first, then software; the reason goes to stderr only) | M0 | **retired in the simulator 2026-10-06** (D28): the log shows wgpu opening a Metal device and Skia compiling Metal shaders — the GPU path; the phone's first log confirms it there. If it ever falls back, `BackendSelector::require_metal()` selects Skia's own Metal surface and fails loudly instead |
| Skia's 1-second wait on the Metal shader compiler (Ganesh gives up on a program whose compile takes longer and skips that draw for the frame; nothing is cached, so the next frame retries) | M1 | seen once in the simulator's first launch (D28), no visible effect. Watch the phone's first launch after an install for `Compilation took longer than 1000 ms` and a missing shape in the first frame; if it is visible, it is Slint's to fix (Skia's persistent program cache, `GrContextOptions::fPersistentCache`), not ours |
| Slint 1.18 compiles wgpu into every winit + Skia build (iOS's render path; unused weight on the desktop, which draws on the CPU) | — | measured on the desktop side (+4 MB, +35 s on a minimal app, §14 D14); accepted |

## 14. Decisions taken after this draft

**2026-10-06 — M0 written (awaiting its first Mac build).** The
`crates/ios` binary, the `ios/` XcodeGen project and `run.sh` exist;
the workspace moved to Slint 1.18.1. Nothing ran on Apple hardware —
the sandbox is Linux — so the checks that ran are listed with what
each proves. Decisions:

| # | decision | why |
|---|---|---|
| D14 | **Slint 1.18.1 for every platform.** The re-lock moves skia to 0.153.3 and adds wgpu 30 to builds with winit + Skia (Slint's winit backend enables the renderer's default features, which include wgpu — nothing on our side turns it off); Android's tree has no wgpu. In the `.slint`: `Flickable`'s `viewport-*` properties are `content-*` now (19 uses in `chat.slint`, renamed); and 1.18 enforces a child's `min-width` over its column's fixed width, which pushed Settings → Servers' Port field 10 px past the form (the field's `Field` floor is 120 px in a 110 px column; the port field now sets `min-width: 80px`). Checked: `cargo check`/clippy of every desktop crate (no new findings), 15 headless renders against 1.17.1 (14 identical up to anti-aliasing, the 15th was the Port field), the touch harness (Back, Settings, the 300 px keyboard) passes unchanged, Android's dependency tree unchanged. Measured on a minimal winit + Skia app, release: 18.6 → 22.6 MB, clean build +35 s. The keybind recorder button (desktop) now grows to fit "Pressione uma combinação de teclas…" instead of clipping it | iOS 27 will not launch a 1.17 app (D6); 1.18.0 also fixes Android's `Key.Back` for targetSdk ≥ 34 and adds the display cutout to Android's safe area (ANDROID-SPEC.md D38) |
| D15 | **iOS code is type-checked in the Linux sandbox**: `RUSTC_BOOTSTRAP=1 cargo clippy -Zbuild-std=std,panic_abort --target aarch64-apple-ios`, with Rust's `library/` sources fetched from GitHub at the exact rustc commit (no rust-src component here). The `hello` build checks as is, Skia prebuilt included; for the app, ring's build script is overridden in a scratch `.cargo/config.toml` (`[target.aarch64-apple-ios.ring_core_0_17_14_]`) and `opusic-sys` patched to a copy with a no-op build script, since neither can compile C for iOS without Apple's SDK. Result: the whole app — `crates/ios`, `frontend`, `client`, `audio`, `video`'s stubs, `transport` — passes the workspace's clippy rules for iOS. It found two real errors (§5.2) | the only way to compile iOS code without a Mac; the Mac's build remains the proof for C, linking and signing |
| D16 | The CoreAudio callback thread is not promoted on iOS (as on macOS) | it is real-time already; on iOS the guard has no `Drop` and `mem::forget` on it is a clippy error |
| D17 | IPv6 DSCP is skipped on iOS | socket2 0.6.5 declares `set_tclass_v6` for macOS but not iOS; `SO_NET_SERVICE_TYPE` is the M4 replacement |
| D18 | Lifecycle from the **scene's** notifications (background, foreground) and the application's (memory, terminate) | TN3187: with the scene life cycle adopted, those events move to the scene |
| D19 | `frontend::app::persist_for_background()` | iOS never returns from the loop and ends suspended apps without notice; settings are saved on nearly every change already, this catches the rest |
| D20 | **M0 runs the real client, not only a hello** (Android's D10): `app` is the default feature, `--hello` the smoke test | the desktop root in the narrow layout already works on a phone-sized window; connect, chat and received voice can be tested before any iOS-specific UI |
| D21 | XcodeGen + one cargo build phase (Slint's documented route), the signing team in an uncommitted `Local.xcconfig` included by `Base.xcconfig` (`#include?`), Xcode's cargo output in `target/xcode/` | a committed `.xcodeproj` is unreadable in review; machine settings stay out of the repository |
| D22 | `ios/run.sh` (bash 3.2) and the two build-phase scripts were run under a bash 3.2.57 built from Apple's sources against stand-in `xcodegen`/`xcodebuild`/`xcrun`/`sips`/`codesign`: simulator choice (booted iPhone first, else the newest iOS; never an iPad or a watch), `--device` (paired iPhone, profile updates, `--console`), `--hello`/`--release`/`--no-log`/`--no-build`, the build phase's triples, features, dSYM, signing and its errors. What stand-ins cannot prove: the real JSON of `devicectl`/`simctl`, XcodeGen's reading of `project.yml`, Xcode's environment | macOS ships bash 3.2, where an empty array under `set -u` is an error; the idioms that avoid it are now tested, not assumed |
| D23 | **Reviewed by a second reader** before hand-over, against XcodeGen's ProjectSpec and sources, the xcodebuild man page, Flutter's devicectl/simctl usage, Apple's documentation and the crate sources. No blocker on the simulator path. Fixed from it: `-allowProvisioningDeviceRegistration` (without it the first `--device` build cannot get a profile for an unregistered phone); the build phases call their scripts through `bash` (a Windows commit drops the executable bit); `CFBundleShortVersionString`/`CFBundleVersion` from the build settings (XcodeGen hard-codes "1.0"/"1"); dSYMs only where the configuration asks (not on every Debug build); `grep` without `-q` under `pipefail` (rustup could die of EPIPE — reproduced 1 in 3000 under bash 3.2); only iPhones among paired devices; `UIRequiresFullScreen` dropped (iPad-only, deprecated); the wording of D1, D6, §4.3, §11 and §13 (wgpu is iOS's render path; the iOS 27 requirement applies to apps built with its SDK; boot-time APIs come from cpal; the `SO_NET_SERVICE_TYPE` constants are not in `libc`) | the scripts cannot run here; a reader who had not written them checked them against the tools' own documentation |
| D24 | **Any iPhone simulator on iOS 17 or later** (2026-10-06, the user has the iOS 18.1 one): nothing in M0 needs a newer runtime — the deployment target is 17.0, the scene manifest works from iOS 13, and the iOS 17+ APIs of later milestones are on the 18.1 runtime too. `run.sh --os 18.1` picks an iPhone simulator on that runtime (`--os 18` any 18.x, the newest first), and `--sim` takes a name or a UDID; the chosen one is printed with its iOS. Xcode 16 or later builds it; the phone may need the Xcode that knows its iOS, and TestFlight needs Xcode 26+ | the user's machine decides which simulators exist; the script should take what is there |
| D25 | **One version for every platform: the public crates', 1.1.7** (2026-10-06). The eight crates whose version users, the server and the stores see — `protocol`, `transport`, `client`, `server`, `frontend`, `desktop`, `android`, `ios` — move together (1.1.6 had moved only the apps: they said 1.1.6 and told the server 1.1.5, the protocol crate's version). Every platform reads the number from its crate: the desktop's updater and About from `frontend`, the wire's `client_version` from `protocol`, Android's default `versionName` from `crates/android/Cargo.toml` (Gradle, through `providers.fileContents`; the release workflow checks the tag against it), iOS's `MARKETING_VERSION` from `crates/ios/Cargo.toml` (`run.sh` → `Config/Version.xcconfig`, build number = commit count, the cargo phase refuses a mismatch). `crates/protocol/tests/versions.rs` fails when one of the eight is bumped alone. The internal libraries (`app-core`, `audio`, `video`, `ui`, `i18n`) keep their own numbers | one number per release, written once per crate and checked |
| D26 | **First Mac run (2026-10-06): Xcode 16.2 refused every destination** — "Unable to find a destination matching { id:<the iOS 18.1 simulator> }", with the only ineligible destination "Any iOS Device … error: iOS 18.2 is not installed. To use with Xcode, first download and install the platform". Since Xcode 15 the iOS platform matching Xcode's SDK (18.2 for 16.2) must be installed before `xcodebuild` builds for any iOS destination, simulators of other runtimes included; once it is, the 18.1 simulator is usable as D24 says. Fix on the Mac: `xcodebuild -downloadPlatform iOS` (or Xcode → Settings → Components) — for Xcode 16.2 it fetches the iOS 18.3.1 simulator, Apple's pairing with the 18.2 SDK (`-buildVersion 18.2` would take exactly 18.2; no need). `run.sh` now prints the Xcode it uses, and turns this error into that instruction (tested under bash 3.2 against the exact output); its other build failure says "xcodebuild failed" instead of blaming cargo, which had not run. Also noted: an iPhone on iOS 26/27 needs Xcode 26+ to install on, which needs a newer macOS (Xcode 27: macOS Tahoe 26.6, Apple silicon) — if Xcode is updated for the phone anyway, that replaces the 18.2 download | the project was fine; the machine needed its platform. The script should say what to do instead of the raw resolver dump |
| D27 | **`run.sh` ended silently on the second Mac run** (2026-10-06): no output at all, after the first run had printed `>> Xcode 16.2`. Cause: `XCODE="$(xcodebuild -version | head -n 1)"` under `set -euo pipefail` — `head` leaves after the first line, `xcodebuild` writes its second ("Build version …") into the closed pipe and dies of SIGPIPE (141), `pipefail` makes the assignment fail and `set -e` ends the script; stderr was sent to `/dev/null`, so nothing said why. A race: it lost this time. Reproduced under bash 3.2 with a stand-in that sleeps between the two lines (exit 141, no output). Fixed: `xcodebuild -version` is read whole and its first line taken in bash (`first_line`), and a failing `-version` now shows Xcode's message (first-launch setup or licence after an install); every `… \| head -n 1` in `run.sh`, the cargo phase and the Android release workflow became `awk … { print; exit }` reading the file itself; both scripts trap `ERR` and print where they stopped ("near line N" — bash 3.2 reports the end of the enclosing block), and `simctl list` has its own message. Re-tested: the slow stand-in passes 3 of 3, every earlier scenario unchanged, no false trap firings | the reviewer had flagged this class for `grep -q`; the fix then missed `head`. Under `pipefail` no pipeline may have a reader that leaves early |

| D28 | **M0's hello runs in the simulator** (2026-10-06, Xcode 16.2, iPhone 16 Pro on iOS 18.3, `run.sh --hello --os 18.3`): the page drew in the theme's colours and the button counted taps. That proves, in one go: XcodeGen reads `project.yml`; the cargo phase builds `aarch64-apple-ios-sim` from Xcode (Slint's compiler and skia-bindings' build scripts included) and the binary lands in the bundle; the version guard passes (`MARKETING_VERSION` 1.1.7); the icon phase runs; the app launches under the scene manifest with `SlintWindowSceneDelegate`; `paths.rs` and `logging.rs` work (the start line names `…/Library/Application Support/baucord` and the log in `…/Documents`); and Skia draws **on the GPU** through wgpu's Metal device. What the log also said, none of it ours to fix: wgpu's `Missing downlevel flags` (the simulator offers only the Apple2 GPU family, so wgpu reports it as not fully WebGPU-compliant); `MAPPABLE_PRIMARY_BUFFERS enabled on a discrete gpu` (Slint requests every feature the adapter has, and wgpu calls a Metal device without unified memory "discrete" — the simulator's device; an iPhone has unified memory, so the phone should not say it); and one Skia `Compilation took longer than 1000 ms` (`org.skia.ganesh`) for a circle-edge vertex shader — Ganesh waits one second for the Metal compiler and drops that draw for the frame if it is late, without caching the failure, so the next frame compiles it again with the system's compiler warm (§13) | the first Apple-hardware result; it retires the Skia-prebuilt and CPU-fallback risks in the simulator and leaves the C builds (libopus, ring) to the app build |

**2026-10-06 (evening) — the client runs in the simulator.** `run.sh
--os 18.3` built the whole app (libopus's cmake build and ring compiled
for `aarch64-apple-ios-sim` without a change) and it connected from
the iPhone 16 Pro simulator: roster, channels, chat, voice both ways
(`audio stats … Sent 30 Recv 157 Lost 0 … Underruns 0 … aec: none (not
on this platform yet)`), the audio session line, `lifecycle:
foreground`, the device key generated in Application Support. The
user's report: "all looks good" except three things — a stream showed
"Video decoder init failed: … no video codec backend on this platform",
Attach did nothing (`no file picker on this host`), and Quit did
nothing (`ControlFlow::Exit ignored on iOS`) — plus a log flooded with
winit's `processing non RedrawRequested event after the main event
loop: AboutToWait`. The first two were M4's and M2's; pulled forward:

| # | decision | why |
|---|---|---|
| D29 | **No Quit on iOS.** `frontend::app::HOST_CAN_QUIT` is false there: the connect page's Quit is hidden (`State.can-quit`), `/quit` answers with how iPhone apps close (the app switcher; `quit-ios` in the three catalogs) and leaves the command list. `host-single-window` (the admin panel's toast) no longer says "Android" but "phones" | winit ignores `ControlFlow::Exit` on iOS (the loop never returns, §4.1), and Apple's rule is that only the user closes an app — a button that does nothing is worse than none |
| D30 | **Stream viewing through VideoToolbox, now** (D9 pulled forward from M4): `video::videotoolbox::VideoToolboxDecoder`, chosen by `client::screen::create_decoder()` on iOS. The in-band SPS/PPS of each keyframe make a `CMVideoFormatDescription` and a `VTDecompressionSession`; each Annex-B access unit is re-framed as AVCC (`video::annexb::split`, platform-free: 6 host tests), wrapped in a `CMBlockBuffer`/`CMSampleBuffer` and decoded **synchronously** (no async or temporal flag: the output callback runs before `DecodeFrame` returns, so it writes into a `Target` on the caller's stack) to **BGRA** — VideoToolbox converts the colours on the GPU from the stream's own signalling — and `yuv::bgra_to_rgba` swaps two bytes a pixel into the client's RGBA (2 host tests). `kVTDecompressionPropertyKey_RealTime` on. New parameter sets rebuild the session; any decode error (`kVTInvalidSessionErr` after iOS took the hardware decoder back in the background, among them) rebuilds it at the next keyframe, dropping P-frames meanwhile — the senders send an IDR every 2 s. `flush()` is a no-op, as MediaCodec's. New crates (iOS only): `objc2-video-toolbox`, `objc2-core-media`. Checked: a host test runs the real encoder (libx264 here) through `annexb::split`, rebuilds Annex-B from the AVCC and decodes both — 75 frames, identical pictures; the iOS build clippy-clean (D15's method) | the simulator showed the gap first-hand; the Android sibling (MediaCodec, ANDROID-SPEC.md D23) set the shape, and the first device run says whether VideoToolbox agrees: `VideoToolbox: decoding H.264 to BGRA`, then `VideoToolbox: first picture WxH` |
| D31 | **The host's file and link hooks** (§10's M2 rows, pulled forward): `crates/ios/src/host.rs` implements `frontend::host::Hooks` in Rust. *Attach* shows an action sheet (*Photos* / *Files* / Cancel, `pick-from-photos`/`pick-from-files` in the catalogs): Photos is `PHPickerViewController` (out of process, no permission; *compatible* representation, so HEIC arrives as JPEG; one item; images only for the avatar) — declared in `host.rs` with `extern_class!` because `objc2-photos-ui` 0.3.2 binds it for macOS only — and Files is `UIDocumentPickerViewController` (any item, `asCopy`). The item is copied (Photos: inside the completion handler, before Photos deletes it) or moved (Files' own copy) into `tmp/picks/` and handed to the desktop dialog's road (`attach_path`); one pick at a time and the 64 MiB cap as on Android. *Save* writes `Documents/Downloads` ("Files › baucord › Downloads" in the toast). *Open* is Quick Look through `UIDocumentInteractionController`, with the share sheet for types it cannot preview. *Links* go to `UIApplication openURL:`. Delegates (weak in UIKit) and the open interaction controller are kept in a main-thread `thread_local`. Reviewed by a second reader against the crate sources and Apple's forums; fixed from it: a picker swiped down tells neither of its own delegate methods (Apple DTS), so both pickers' presentation controllers report `presentationControllerDidDismiss:` to the same delegate, which frees the slot — before, every Attach said "still reading" for 10 minutes; a Photos item with no usable type falls back to any image or movie, never to the item's first type (a Live Photo's is a folder); the callback runs after the slot's lock is released; a refused Files copy is deleted. Settings' *Open config folder* is hidden on touch hosts (Android too): no phone app opens another's private folder, and on iOS the share sheet would have offered the device key | users attach photos from a phone more than anything; doing it now, with the simulator at hand, beats waiting for M2. New crates (iOS only): `objc2-photos-ui`, `objc2-uniform-type-identifiers` |
| D32 | **winit's iOS log noise is dropped below `Error`** (`logging.rs`): winit 0.30.13 sends `AboutToWait` after switching itself into redraw processing and then warns about its own event on every run-loop turn (up to 120 a second while anything animates), and warns `… is ignored on iOS` for each desktop-only window call Slint makes. Filtered by target before formatting, so it costs nothing | none of it is actionable, and it buried the lines that are — and filled `baucord.log` |

**2026-10-06 (night) — the first stream in the simulator.** A share
(a 16:9 game) played through VideoToolbox (D30). It did not fill the
landscape screen, which the user asked for, on both phones:

| # | decision | why |
|---|---|---|
| D33 | **The stream page fills the screen** (shared `viewer.slint`; ANDROID-SPEC.md D41): on a phone the picture goes edge to edge — under the notch, the home indicator and the system bars — and only the chrome (name chip, control bar) keeps to the safe area (`chrome-inset-*`, not the keyboard's). In landscape a **double-tap**, or the bar's zoom button (Expand/Shrink; `viewer-fill`/`viewer-fit` in the catalogs), switches between *fit* (the whole picture, letterboxed) and *fill* (`ImageFit.cover`: the screen covered, the picture's edges cropped); the choice is `State.watch-fill`, kept for the session. Portrait offers no fill — a landscape stream filling an upright phone would show a quarter of it. The control bar's stats column now gives way (elides) when the bar does not fit: upright, *Stop watching* had been pushed off the screen, and iOS has no Back key to leave by. The desktop window is unchanged (double-click is still fullscreen). Rendered with Slint's software renderer at 852×393 (an iPhone 16 Pro on its side) and 393×852, a 4:3 and a 16:9 frame: fit fills the height, fill covers the screen, a double-tap toggles, the chrome sits inside the insets, the portrait bar keeps its Stop button | a 4:3 or 16:9 stream on a 19.5:9 phone is a small picture with bands; the screen's own edges were being given up to the safe area too |
| D34 | **iOS: Slint's safe area is refreshed after a rotation** (`crates/ios/src/insets.rs`). The screenshots showed portrait insets in landscape (the name chip under the Dynamic Island, the page 96 pt short — top 62 + bottom 34): Slint reads the safe area only when winit reports a resize, which winit does from its view's `layoutSubviews`, evidently before UIKit has the new orientation's insets; nothing asks again. A probe — an empty, non-interactive `UIView` subclass behind winit's view, sized with the window — hears `safeAreaInsetsDidChange` and calls `setNeedsLayout` on winit's view, which re-sends the resize, and Slint reads the insets afresh. Installed on the first `UIWindowDidBecomeKeyNotification`, with one relayout then for an app that starts rotated. Every page benefits, not only the stream's | the platform's layout pass, not the app, holds the stale value; a view that is told when the safe area changes is UIKit's own signal for it |

**2026-10-06 (night) — attach and watch, in the simulator.** The log
of that run: a Photos pick (`photo pick: loading public.jpeg for
IMG_0005 (has ["public.jpeg", …])` — the *compatible* mode gave JPEG
first, as D31 hoped), copied and previewed; a Files pick of a 2.6 MB
`baucord.log`, attached as a card; a 1600×900 share decoded by
VideoToolbox at a steady 30 fps (`VideoToolbox: first picture
1600x900`, then `viewer telemetry … 30.0 fps`), with a few `Slow
decode` frames of 20–50 ms — the simulator decodes on the Mac's CPU;
the phone's hardware decoder is the measure. Two findings, and the
user's screenshots of the main screen on its side (taken before D34's
fix was installed, so still with portrait insets):

| # | decision | why |
|---|---|---|
| D35 | **A compact sidebar for short screens** (shared `sidebar.slint`; ANDROID-SPEC.md D42): below 600 px of height — a phone on its side (~370 px with the right insets), a small desktop window — the sidebar keeps every control on screen by giving up room: gaps of 8 px instead of 12, the *New channel* / *Invite* chips on the server-name row (a `ChipButton` component now draws both rows' chips), Messages capped at 30 % of the height and at its own rows, the status and latency on one line (the resource line dropped), the control bar 44 px tall, and *Disconnect* as a red hang-up icon in that bar instead of its own button. The full stack needs ~600 px (Messages alone is 200); on its side, with Messages open, the status, the mic/deafen/settings bar and Disconnect had been pushed off the bottom entirely, and with it folded Disconnect was cut by the screen's corner. Rendered at 852×393 with landscape insets: everything on screen open or folded, the voice list keeping two to four rows; upright (393×852) unchanged | the controls are the part that must never be cut; a phone on its side has a third of a desktop's height |
| D36 | **One delivery per pick** (`host.rs`): the pick's callback is *claimed* by the first file delivered (`claim()`); a second delivery of the same pick is ignored, and a cancel no longer frees a slot whose file is still being copied. The simulator reported one Files pick twice within the same millisecond: the first moved the file and attached it, the second found it gone — `file pick failed: No such file or directory`, shown as an error toast over a successful attach | whatever UIKit's reason, a pick's file is handed over once |

In the simulator, Photos has Apple's sample pictures and Files starts
empty: drag a file from the Mac onto the simulator window to put it in
Files (or a picture, into Photos).

What the next Mac runs will tell, in order: the main screen on its
side with the compact sidebar and the right insets (D34, D35); the
stream page in landscape filling the screen, a double-tap zooming to fill, and the
name chip clear of the Dynamic Island after a rotation (D33, D34);
a shared screen in the simulator (`VideoToolbox: first picture WxH`,
seen 2026-10-06); Attach from Photos and
from Files, a received file saved and opened, a link; `lifecycle:
background` in the log on going to the home screen (⌘⇧H); then the
phone (`--device`), where the Xcode/iOS pairing of D26 may ask for a
newer Xcode. Record each here and in SESSION-GUIDE.md §5.
