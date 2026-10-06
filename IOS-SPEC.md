# baucord — iOS (v1) technical spec

Status: draft 1, 2026-10-06; M0 written the same day, awaiting its
first build on a Mac (§14). Decisions in §1 are taken; the rest is the
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

To come: `host.rs` (the `Hooks` of §10: pickers, saving, opening,
links), the audio route, the background task.

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
| `UIApplication openURL:options:completionHandler:` | `Hooks::open_url` | M2 |
| `PHPickerViewController` (photos), `UIDocumentPickerViewController` (any file, `asCopy`) presented from the key window's root view controller; their delegates declared in Rust (`define_class!`) | `Hooks::pick_file` → the same `attach_path` as the desktop dialog | M2 |
| `Documents/Downloads` + `UIActivityViewController` (Save to Files, Photos, AirDrop) | `Hooks::save_download`, a file card's Save | M2 |
| `QLPreviewController` (Quick Look) | `Hooks::open_target` | M2 |
| `UIPasteboard` | Slint's iOS clipboard (built in) | done |
| `VTDecompressionSession`, `CMVideoFormatDescriptionCreateFromH264ParameterSets` (`objc2-video-toolbox`, `objc2-core-media`, `objc2-core-video`): Annex-B → length-prefixed samples, output as NV12 through `video::yuv` or as BGRA | stream viewing (D9) | M4 |
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
| M0 ✍ 2026-10-06 | toolchain + hello + the client in the simulator: `crates/ios`, `ios/`, `run.sh`, Slint 1.18.1 | `run.sh --hello` shows the page in the simulator and on the phone; `run.sh` connects to a server from the simulator — the narrow layout, chat, voice received (headphones) |
| M1 | the client on the iPhone: signing, the narrow layout at the phone's size, safe areas, rotation, typing in the composer (accents, emoji), every page's way back, `sys-locale` | a DM and a channel conversation from the iPhone |
| M2 | files and links (the `Hooks` of §10), localized permission strings, the local-network hint, the iOS hint strings | attach a photo and a file; open a received file; a link opens Safari |
| M3 | voice: the session lifecycle, VPIO, routes, interruptions, the record permission at join | 30 min in voice on the loudspeaker with the screen locked, no echo reported by the others; a phone call pauses and resumes it |
| M4 | viewing (VideoToolbox); resilience: background task, network change, NAT64, `SO_NET_SERVICE_TYPE`; the local notification for a DM in voice | Wi-Fi → cellular recovers in under 10 s; a 1080p share plays |
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
| Skia has no prebuilt for the iOS targets | M0 | retired on paper 2026-10-06: the `aarch64-apple-ios` and `-sim` binaries (Metal) downloaded in the sandbox; the Mac's first build confirms |
| The engine does not compile for iOS | M0 | type-checked and clippy-clean for `aarch64-apple-ios` in the sandbox (§14 D15), which found and fixed two errors (§5.2); the real compile of libopus and ring, and the link, are the Mac's |
| libopus's cmake build for iOS | M0 | cmake-rs sets `CMAKE_SYSTEM_NAME=iOS` and the SDK; if it fails, a `CMAKE_TOOLCHAIN_FILE` or building opus with cc — the first build says |
| Xcode's environment leaking into cargo's host builds (`SDKROOT`, `PATH`) | M0 | Slint's script lineage, the PATH fix, a separate target directory; first build says |
| Background/foreground notifications under the scene life cycle | M0 | `lifecycle.rs` observes the scene's; the first run's log must show `lifecycle: background` when the app is swiped away |
| Text input limited to `UIKeyInput` | M1 | type Spanish and Portuguese on the phone; autocorrect is the gap — upstream `UITextInput`, or a native text view over the composer |
| VPIO side effects: quieter loudspeaker, ducking, 48 kHz | M3 | test on the user's iPhone; the ducking configuration; measure |
| Chat-only sessions miss DMs while suspended | v1, by design | reconnect on return; push in v2 |
| Free provisioning expires every 7 days | ongoing | the paid program before anyone else tests |
| IPv6-only networks | M4 | `getaddrinfo` for literals |
| App Store Connect's privacy-manifest and icon checks | M5 | §11 |
| Skia falls back to the CPU on iOS without telling the app (Slint 1.18: wgpu first, then software; the reason goes to stderr only) | M0 | the first run checks that it draws on the GPU (Xcode's Debug navigator shows GPU work; stderr has no surface error); if it ever falls back, `BackendSelector::require_metal()` selects Skia's own Metal surface and fails loudly instead |
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

What the first Mac build will tell, in order: whether XcodeGen accepts
`project.yml`; whether libopus's cmake build and ring compile for iOS;
whether the hello page renders in the simulator (on the GPU, with the
scene delegate and the safe area); then the client in the simulator;
then the phone. Record each here and in SESSION-GUIDE.md §5.
