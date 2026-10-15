# baucord — iOS technical spec

> **Status (2026-10-09):** 1.2.0, Ad Hoc builds on users' iPhones
> (from CI, D37, or `run.sh --device`). §0–§13 describe the client as
> it is now; §14 holds every decision taken since the first draft
> (D14…D56, in order), each with its reason. What is left:
> `PRODUCTION-PLAN.md` (MOB-2, 3, 5, 6, 9, 10, 19). Start from
> `START-HERE.md`.

`ANDROID-SPEC.md` is this spec's older sibling: almost everything the
Android port learned — the `frontend` split, the single-window host,
the narrow layout, the Settings and stream pages, touch scrolling, the
keyboard insets, the echo-cancellation contract — applies here
unchanged, and this document only says where iOS differs.

## 0. Purpose and scope

An iPhone client for the baucord server, running the **same Rust
engine** and the **same Slint UI** as the desktop and Android: voice
with Apple's echo cancellation, General and DMs, attachments through
the system pickers, viewing screen shares, the Settings page,
notifications while connected. There is no Swift or Objective-C in
the app at all (D2).

Not on iOS, by choice or not yet: sharing the phone's screen (a
ReplayKit broadcast extension), push notifications (APNs needs server
work, PRODUCTION-PLAN PRD-1), CallKit, iPad layouts,
keybinds and push-to-talk, an updater (TestFlight or the App Store
would update the app), iOS before 17, Intel Macs' simulator.

## 1. Decisions of the first draft

| # | decision | why |
|---|---|---|
| D1 | UI in Slint through its **winit backend with the Skia renderer** — Slint's iOS support; Skia draws through wgpu 30's Metal backend (Slint 1.18's default surface order: wgpu, then the CPU) | one UI codebase; the `frontend` glue shared with desktop and Android |
| D2 | **No Swift, no Objective-C.** Platform APIs are called from Rust through the `objc2` framework crates (`objc2-ui-kit`, `objc2-avf-audio`, `objc2-foundation`, …); the Xcode project is generated (XcodeGen) and has one build phase that runs cargo | Android needed Java because `NativeActivity` cannot receive an activity result and its methods cannot be reached from native code; on iOS `objc2`'s `define_class!` declares Objective-C classes — picker delegates, observers — in Rust, so nothing forces a second language. The `objc2` 0.6 / 0.3 family is already in the tree through cpal and Slint's iOS backend |
| D3 | v1 = voice + chat + viewing shares | Android's v1 as it stood on 2026-10-06 (viewing landed there early, D23) |
| D4 | **iOS 17.0** minimum | the voice-processing ducking control (§6.3) and `AVAudioApplication` are 17+; iOS 17 runs on every iPhone from the XS; iOS 27 (released 2026-09-14) needs an A13 |
| D5 | **iPhone only, arm64 only** — devices and the simulator on Apple silicon | every iPhone on iOS 17 is arm64; iPad later (it runs the app in its iPhone frame meanwhile); no `x86_64-apple-ios` build |
| D6 | **Slint 1.18.1 for the whole workspace** (D14) | built with the iOS 27 SDK (Xcode 27), an app that has not adopted the UIScene life cycle does not launch (TN3187); Slint 1.18 provides the scene delegate (`SlintWindowSceneDelegate`); 1.18.1 fixes iOS apps rendering in software instead of Metal, an intermittent black screen at start, and the 60 Hz cap |
| D7 | The iOS host selects **winit + Skia** itself; the frontend's own choice (Skia on the CPU) is compiled out on iOS | that choice keeps Windows Game Mode from classing baucord as a game; on a phone it would cost battery and ProMotion's 120 Hz |
| D8 | Echo cancellation = Apple's **Voice-Processing I/O unit (VPIO)**, a native backend `audio/src/native/vpio.rs` on `coreaudio-rs`, written for iOS (D47) and reusable on macOS | AEC-SPEC.md D1 (platform AEC first); VPIO is also that spec's planned macOS route (§9, E8) — one backend serves both |
| D9 | Stream viewing = **VideoToolbox** (`VTDecompressionSession`) as a fourth `VideoDecoder` (D30) | hardware H.264 with no FFmpeg for iOS (`video` swaps in `codec_stub.rs` there), the way MediaCodec did it on Android (D23 there) |
| D10 | Distribution: **development installs from Xcode** (a free Apple ID works: 7-day profiles, 3 apps per device); **Ad Hoc** builds from CI for the paid program's registered phones (D37); **TestFlight** to give it to anyone else — not yet (MOB-5) | iOS has no APK equivalent; TestFlight is the closest thing to "send people a build" |
| D11 | The app icon is the repo's **`assets/icons/icon.png`**, rendered at build time into one 1024 px asset (`sips`) | Android's D36 for iOS: one picture for every platform |
| D12 | Bundle id **`dev.baucord`** (overridable per machine in `Config/Local.xcconfig`), display name "baucord" | the Android `applicationId`; bundle ids are global across Apple accounts, so a developer whose id is taken sets their own |
| D13 | **Staying connected:** the `audio` background mode keeps the process running while audio runs (Android's foreground service, D34 there). Since D51 a connected app always has some: the VPIO unit in a room, a plain output in the lobby. A disconnected app is suspended with the screen | iOS has no sanctioned way to keep a chat socket alive in the background; push notifications are the real answer (PRD-1) |

Open: CallKit, later; whether RNNoise stays on under VPIO, which
suppresses noise itself — the same question as ANDROID-SPEC §6.4, the
same answer (on).

## 2. Where things live

```
baucord/
  crates/
    ios/                      bin crate (`baucord`): main.rs, paths.rs, logging.rs,
                              session.rs, lifecycle.rs, host.rs, notices.rs,
                              insets.rs, render.rs, hello.rs  (§5.1)
    audio/src/native/         vpio.rs, avsession.rs, pump.rs: the call path (§6)
    video/src/videotoolbox.rs stream decoding (D30)
  ios/                        the Xcode project (no Rust, no Swift)
    project.yml               XcodeGen: target, Info.plist keys, build phases
    Config/Base.xcconfig      shared settings; includes Version.xcconfig and Local.xcconfig
    Config/Local.xcconfig.example   your signing team / bundle id (copy it)
    scripts/build_with_cargo.sh     the cargo build phase
    scripts/render_icon.sh          ../assets/icons/icon.png → AppIcon
    Resources/Assets.xcassets       the icon (rendered) and the launch colour
    run.sh                    generate, build, install, launch, stream the log
    README.md                 setup on the Mac, the phone, CI builds, logs
  .github/workflows/ios-release.yml   an Ad Hoc .ipa on a published release (D37)
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
  arboard, global-hotkey or interprocess. The C builds are libopus
  (cmake), ring's assembly, and Skia's prebuilt.

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
| Xcode **16 or later** (27 is current) | App Store | the iOS SDK, the simulator, signing, `xcodebuild`, `xcrun devicectl` (device install and log); open it once to install the iOS platform. Any iPhone simulator on iOS 17 or later runs the app (D4, D24). The phone may need a newer Xcode (one that knows the phone's iOS); App Store Connect takes uploads only from Xcode 26 or later (since 2026-04-28) |
| Xcode selected | `sudo xcode-select -s /Applications/Xcode.app` | otherwise `xcodebuild` is the bare Command Line Tools' |
| Xcode's own iOS platform | `xcodebuild -downloadPlatform iOS` (or Settings → Components) | Xcode 15+ builds for no iOS destination until the platform matching its SDK is installed; older simulator runtimes do not count (D26) |
| Homebrew | <https://brew.sh> | for the next two |
| XcodeGen | `brew install xcodegen` | the project from `project.yml` — Slint's documented route for Rust apps |
| CMake | `brew install cmake` | `opusic-sys` builds libopus with it; cmake-rs passes the iOS system name and SDK |
| Rust + the iOS targets **for the pinned toolchain** | rustup; then inside the repo `rustup target add aarch64-apple-ios aarch64-apple-ios-sim` | `rust-toolchain.toml` pins 1.95; running inside the repo adds the targets to that toolchain |
| An Apple ID in Xcode | Xcode → Settings → Accounts | free = "Personal Team", enough for your own phone (D10) |
| The iPhone | Developer Mode on (Settings → Privacy & Security), the Mac trusted | |

Skia comes as a prebuilt download, as on Android (`skia-bindings`
publishes `aarch64-apple-ios` and `-sim` binaries with Metal). The
first build then compiles the whole engine for iOS: minutes.

## 4. Runtime architecture on iOS

### 4.1 Process, scene, background

- UIKit starts `baucord`'s `main` (`crates/ios/src/main.rs`): the log,
  the panic hook, the app's folders, the audio session category, the
  lifecycle observers, the host's hooks, the renderer, then
  `frontend::app::run()` — the same function the desktop and Android
  hosts call. Inside it Slint starts winit, winit calls
  `UIApplicationMain`, and the call never returns. **Nothing may wait
  for "after the loop"**: the desktop's `shutdown()` never runs; the
  host saves on the way into the background instead (§4.3), and there
  is no Quit (D29).
- One process, one scene, one window: the Info.plist's scene manifest
  names `SlintWindowSceneDelegate` (the UIScene life cycle, required
  to launch when built with the iOS 27 SDK) and does not support
  multiple scenes. The frontend treats iOS as a **single-window touch
  host** exactly like Android (`SINGLE_WINDOW_HOST`), so the Settings
  page, the stream page, the narrow layout, touch scrolling and the
  keyboard insets all apply as they are.
- **Connected**, the `audio` background mode keeps the process running
  with the screen locked or another app in front, because audio is
  always running: the VPIO unit in a room, a plain output in the lobby
  (D51). The network thread and the voice worker carry on; what only
  feeds the screen stops (D42). iOS shows its own microphone
  indicator, which should appear only in a room and unmuted (D51,
  still to confirm on a phone).
- **Disconnected**, iOS suspends the app a few seconds after it leaves
  the screen, like any app. Not done (MOB-10): a background task
  (`beginBackgroundTask`) so a reconnect in flight finishes.

### 4.2 Threads

Same as desktop and Android: Slint on the main thread (UIKit's), the
network thread, the voice worker, the screen thread while a stream is
watched, plus CoreAudio's own real-time I/O thread for the audio
callbacks — already time-constraint scheduled, so not promoted (D16).
The network thread sleeps until a datagram, a command or the
transport's next deadline (D41, D57).

UIKit is main-thread only. Hooks are called from Slint callbacks (the
main thread); anything a worker needs from UIKit goes through
`slint::invoke_from_event_loop`. `objc2`'s `MainThreadMarker` makes the
rule a type error rather than a crash.

### 4.3 Lifecycle events

winit receives UIKit's transitions but Slint does not pass them to the
application, so `crates/ios/src/lifecycle.rs` observes the
notifications itself, on the main queue (D18):

- **Scene did enter background** — `frontend::app::persist_for_background()`
  (settings, D19), `set_host_visible(false)` (D42) and a log flush.
- **Scene will enter foreground** — `set_host_visible(true)`.
- **Application memory warning** — logged.
- **Application will terminate** — persist, flush (only delivered to a
  running app, not a suspended one).
- Audio session interruptions and media-services resets are
  `avsession.rs`'s (§6.5).

Not done: on returning to the foreground, rebinding the socket if the
network changed (MOB-10) and re-checking the microphone permission;
dropping the image caches on a memory warning (PRODUCTION-PLAN
MOB-19).

### 4.4 Permissions

Every permission is an Info.plist string the system shows in its own
prompt.

- **Microphone** (`NSMicrophoneUsageDescription`): the system asks the
  first time a stream opens the microphone — the first voice join,
  since the lobby opens none (D51) — and the engine retries a pending
  input every 2 s meanwhile.
- **Local network** (`NSLocalNetworkUsageDescription`): iOS asks the
  first time the app sends to an address on the local network (a
  server on 192.168.x.x). Refused, those servers are silently
  unreachable.
- **Notifications**: asked on the first connect of a launch (D50).
- **Photos**: none for picking (`PHPickerViewController` runs out of
  process).
- No camera, location, contacts or tracking.

Not done (MOB-19): the permission strings in Spanish and Portuguese
(`InfoPlist.strings`); asking for the microphone explicitly at the
first join (`AVAudioApplication requestRecordPermission`) with a toast
that links to the app's page in Settings on a refusal; a connect error
that names a refused local-network permission.

## 5. The crates

No new split: `frontend` without its `desktop` feature (the `stubs/`
modules, the `host::Hooks` seam), `ui` with the same root as every
host, `client`, `audio`, `video`, `transport` unchanged in shape.

### 5.1 `crates/ios`

- `main.rs` — the entry above; off iOS it prints where it belongs and
  exits, so a host `cargo check -p ios` validates the manifest.
- `paths.rs` — `NSFileManager`'s Application Support (`baucord/`:
  settings, identity) and Documents (the log, saved files); the config
  folder is excluded from backups (§8).
- `logging.rs` — Android's tee with stderr in place of logcat: stderr
  is what Xcode's console, `simctl launch --console-pty` and
  `devicectl … --console` show; `baucord.log` in Documents, rolled at
  5 MB; winit's iOS noise filtered (D32).
- `session.rs` — the audio session's category at startup (§6.1).
- `lifecycle.rs` — the observers of §4.3.
- `host.rs` — the `Hooks` (D31): the Photos and Files pickers behind an
  action sheet, saving into `Documents/Downloads`, Quick Look, links,
  the screen kept on (D45), the pasteboard (D53).
- `notices.rs` — local notifications and the chime route through the
  system (D50).
- `insets.rs` — the safe-area probe: a resize for Slint after every
  change (D34), and `UIKit`'s own numbers as a floor (D39, D46).
- `render.rs` — the renderer: Skia on the GPU unless a diagnostic
  launch names another (D40).
- `hello.rs` (feature `hello`) — the smoke test: a page in the theme's
  colours, inside the safe area, with a tap counter.

### 5.2 Outside `crates/ios`

- `frontend`: `SINGLE_WINDOW_HOST` includes iOS and `HOST_CAN_QUIT`
  is false there (D29); `select_renderer()` and the saved window size
  are compiled out on iOS (D7); `persist_for_background()` (D19).
- `audio/src/platform.rs`: the CoreAudio callback thread is left alone
  on iOS, as on macOS (D16).
- `transport/src/udp.rs`: the IPv6 DSCP mark is skipped on iOS (D17;
  §7 has the iOS way).

## 6. Audio on iOS

### 6.1 The session's category

cpal's iOS host never touches the audio session, and the default
category (`SoloAmbient`) has no microphone, so `session.rs` sets
**`PlayAndRecord`** at startup with `DefaultToSpeaker` (otherwise
`PlayAndRecord` plays to the earpiece and the app seems silent),
`AllowBluetoothHFP` and `AllowBluetoothA2DP`.

### 6.2 The session in voice

`avsession.rs`: the mode is `voiceChat` on the call path and `default`
off it, set by the engine's plan with both streams closed (D47);
48 kHz and 10 ms are asked for and the hardware decides (the workers
resample, and `output::Cushion` sizes the queue from the callback).
If the mode change put the call on the earpiece, the speaker override
brings back the loudspeaker. The volume buttons control the call
volume in this mode.

Not done (MOB-19): activating the session on joining a room and
deactivating it with `notifyOthersOnDeactivation` on leaving, so a
podcast paused by joining resumes after.

### 6.3 Echo cancellation: the VPIO backend

Done (D47, AEC-SPEC E10): `audio/src/native/vpio.rs`, one
`kAudioUnitSubType_VoiceProcessingIO` unit for both directions — the
processed microphone on bus 1, the loudspeaker (the canceller's
reference) on bus 0, 48 kHz mono `f32` both ways — opened by the plan
before either side starts, so both sides attach to it or both take
plain streams. Bypass is read back after `initialize` as the verdict;
other audio's ducking is set to its minimum. Muted, the microphone is
muted on the unit itself (D51). A watched stream is mixed into the
unit's output, so it is cancelled like the voice (D52). Not wanted
(headphones under Automatic, or Off): cpal's `RemoteIO`, Android's
split between the call path and the media path. `aec::ios_output_kind`
classifies the route for Automatic.

macOS can reuse the file with the device chosen through
`kAudioOutputUnitProperty_CurrentDevice` — AEC-SPEC §9's macOS item
(PRD-5).

### 6.4 Routes

Not done (MOB-19): iOS has no route control yet — Settings → Audio
lists cpal's devices, and the system decides. The plan: map
`AVAudioSessionPort`s onto `audio::route`'s kinds (Android's D31) and
reuse its decision function and picker unchanged:

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
app — the system's route picker (`AVRoutePickerView`) is the iOS way.
`AVAudioSessionRouteChangeNotification` re-plans, as Android's route
callback does.

### 6.5 Interruptions and resets

`AVAudioSessionInterruptionNotification` *ended* and
`AVAudioSessionMediaServicesWereResetNotification` mark both sides
failed, and the engine's `service()` reopens them (D47; the lobby's
plain output has the same watch, D51). Not done (MOB-9): the "paused —
call" line while interrupted, and recovering when iOS never posts
*ended*.

### 6.6 Processing chain

Unchanged (RNNoise → VAD → Opus, the ladder, RED/FEC, the jitter
buffer). RNNoise under VPIO: the open question of §1.

## 7. Networking on iOS

- The transport is the transport: quinn over std UDP sockets; GSO is
  not offered on iOS and `quinn-udp`'s probe handles that.
- **Wakes**: the network thread sleeps until a datagram, a command or
  the transport's next deadline (D41, D57) — none in a quiet second
  beyond the socket watcher's two.
- **App Transport Security** governs `URLSession` only. The QUIC
  transport and `ureq` (rustls) — the link-image previews in
  `frontend/src/media.rs` — open their own sockets and are not
  subject to it.
- Port 51000 by default, as everywhere.

Not done:

- **DSCP** (MOB-19): IPv4 is marked as everywhere; IPv6 is unmarked
  (D17). The iOS way is one `setsockopt(SOL_SOCKET,
  SO_NET_SERVICE_TYPE, NET_SERVICE_TYPE_VO)`, which also puts the
  traffic in Wi-Fi's voice access category, for both families. Both
  constants are public in xnu's `sys/socket.h` (0x1116 and 4) but
  absent from the `libc` crate, so `transport` would declare them.
- **Network change** (MOB-10): the existing loss/timeout reconnect
  only; a check on return to the foreground (rebind if the local
  address moved), and later `NWPathMonitor` (a C API; no objc2 crate
  covers it).
- **IPv6-only networks (NAT64)** (CLI-10): App Review tests on them.
  An invite with an IPv4 literal is parsed by std without
  `getaddrinfo`, so it never gets the synthesized IPv6 address and
  fails there. The fix resolves literals through `getaddrinfo` on iOS.

## 8. Config, identity, logs

- **Application Support/baucord** (`paths.rs`): `settings.json` and the
  device identity key, set with `client::config::set_config_dir`.
  Excluded from iCloud and computer backups
  (`NSURLIsExcludedFromBackupKey`): a restore onto a second iPhone
  would give the server two devices with one key — Android's
  `allowBackup="false"`. The Keychain with a `ThisDeviceOnly`
  accessibility is the more iOS-native home for the key; later, with a
  migration (MOB-13).
- **Documents**: `baucord.log` (+ `.1`), and the files users save
  (`Downloads/`). `UIFileSharingEnabled` +
  `LSSupportsOpeningDocumentsInPlace` make it the app's folder in the
  Files app ("On My iPhone › baucord"), so a user can send the log
  without a Mac; Settings → Debug sends or copies it too (D48, D53).
- stderr: Xcode's console, `ios/run.sh`'s stream.
- Locale: `sys-locale` reads the preferred languages through
  CoreFoundation.

## 9. The UI on iOS

- **The same `.slint` as Android**, with everything §4.1 lists. Slint's
  iOS backend fills `safe-area-insets` (the notch, the home indicator)
  and `virtual-keyboard-position/size`, so the page rectangle works as
  is; `insets.rs` keeps the safe area fresh after a rotation (D34) and
  floors it with UIKit's own numbers (D39, D46).
- **No Back key.** Every page has its on-screen way back — the chat
  header's ←, the Settings ←, the stream page's *Stop watching*,
  tap-outside for menus and dialogs. The edge-swipe back gesture is
  not something Slint gets from UIKit; a left-edge drag in `.slint` is
  a later nicety.
- **Text input is basic.** winit 0.30's iOS view implements
  `UIKeyInput` only: typing, emoji, accents from a long press, Return
  and backspace work; autocorrect replacements, marked text (Chinese
  and Japanese input methods), the space-bar cursor drag and the
  system's selection handles do not (Slint draws its own selection).
  Spanish and Portuguese typing works this way. If autocorrect matters
  to users, the fix is `UITextInput` upstream in winit, or a native
  text view over the composer.
- **Look**: dark system UI (`UIUserInterfaceStyle Dark`: dark keyboard
  and sheets), light status-bar text, the launch screen in the app's
  background colour (no white flash), 120 Hz on ProMotion iPhones
  (`CADisableMinimumFrameDurationOnPhone` + Slint 1.18.1). Text is in
  the system font through CoreText; emoji are Apple's, which Skia
  renders.
- Rotation: portrait and both landscapes; the width rule gives a phone
  on its side the wide layout, with the compact sidebar (D35).

### 9.1 Parity table

| desktop | iOS |
|---|---|
| sidebar, chat, history, unread, images inline, context menu | as Android: the narrow layout's two pages, long press for a row's menu (D56) |
| files, GIF picker, drag/paste | attachments through the photo and document pickers, saving into the app's Documents (visible in Files), Quick Look to open (D31); paste of text through Slint's clipboard |
| voice: join, mute, deafen, per-user volume, device pickers | the same; echo cancellation through VPIO (D47); no route control yet (§6.4); no push-to-talk |
| Settings, the admin panel (pages on the desktop too) | the same pages, as Android (ANDROID-SPEC.md D71) |
| slash commands | the same |
| share, viewers | viewing through VideoToolbox (D30), zoom and rotate (D44, D49); sharing absent (the button hidden on a touch host) |
| tray, keybinds, updater, single instance | absent; no Quit (D29) |

## 10. The platform surface (Rust → Objective-C, through objc2)

Android's JNI table, for iOS. Everything is called on the main thread.

| call | for | state |
|---|---|---|
| `AVAudioSession` category | §6.1 | done |
| `AVAudioSession` mode, preferred rate and buffer, speaker override, interruption and reset notifications | §6.2, §6.5 | done (D47) |
| VPIO through `coreaudio-rs` | §6.3 | done (D47) |
| `NSFileManager`, `NSURL` resource values | §8 | done |
| `NSNotificationCenter` (scene, application) | §4.3 | done |
| `UIApplication openURL:options:completionHandler:` | `Hooks::open_url` | done (D31) |
| `PHPickerViewController` (photos), `UIDocumentPickerViewController` (any file, `asCopy`) behind a `UIAlertController` action sheet; their delegates declared in Rust (`define_class!`) | `Hooks::pick_file` → the same `attach_path` as the desktop dialog | done (D31, D36) |
| `Documents/Downloads` | `Hooks::save_download`, a file card's Save | done (D31) |
| Quick Look through `UIDocumentInteractionController` (share sheet for what it cannot preview) | `Hooks::open_target` | done (D31) |
| `UIPasteboard` | Slint's clipboard, `Hooks::copy_text` | done (D53) |
| `UIApplication.idleTimerDisabled` | `Hooks::keep_screen_on` | done (D45) |
| `UNUserNotificationCenter` | DMs and "joined" off screen, the chime on screen | done (D50) |
| `VTDecompressionSession`, `CMVideoFormatDescriptionCreateFromH264ParameterSets` (`objc2-video-toolbox`, `objc2-core-media`, `objc2-core-video`): Annex-B → length-prefixed samples (`video::annexb`), output as BGRA, `yuv::bgra_to_rgba` | stream viewing (D9) | done (D30) |
| `AVAudioSession` activation, route, preferred input; `AVAudioApplication` record permission | §6.2, §6.4, §4.4 | MOB-19 |
| `beginBackgroundTask` / `endBackgroundTask` | finish a reconnect in the background | MOB-10 |
| `setsockopt SO_NET_SERVICE_TYPE` | §7 | MOB-19 |

Everything else the app does is Rust.

## 11. Build, sign, install, distribute

```sh
ios/run.sh                # the client, simulator (a booted iPhone, else the newest iOS)
ios/run.sh --os 18.1      # an iPhone simulator on that iOS (D24)
ios/run.sh --device       # the client on the iPhone (D38 settles the team)
ios/run.sh --hello        # the smoke test
ios/run.sh --release | --sim "iPhone 16" (name or UDID) | --no-build | --no-log | --open
ios/run.sh --renderer skia-software | software   # a diagnostic launch (D40)
```

`run.sh` regenerates the project (XcodeGen), builds with `xcodebuild`
(on a device with `-allowProvisioningUpdates
-allowProvisioningDeviceRegistration`, so Xcode makes the development
profile itself and registers a phone the team has not seen), installs
and launches with `xcrun simctl` or `xcrun devicectl` (the first paired
*iPhone*: iPads report the platform "iOS" too), and streams stderr
until Ctrl+C. It is written for macOS's bash 3.2 (D22, D27). `--open`
opens Xcode instead, where ⌘R and the debugger work.

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
Program ($99/year) gives one-year profiles, Ad Hoc builds and
TestFlight.

**CI**: `.github/workflows/ios-release.yml` (D37) builds on a published
release, like the Android APK — a signed Ad Hoc `.ipa` when the paid
program's certificate and profile are in the repository's secrets, an
unsigned build (an artifact, not installable) otherwise. Development
builds stay local (`run.sh`).

**TestFlight and the App Store** (not done; MOB-2, MOB-3, MOB-5,
MOB-6): `xcodebuild archive`, then `-exportArchive` with `method =
app-store-connect` and `destination = upload` and an App Store Connect
API key — Slint's own flow for its viewer. Before the first upload:
the icon without an alpha channel (App Store Connect refuses one;
`render_icon.sh` keeps transparency today), a privacy manifest
(`PrivacyInfo.xcprivacy`) declaring the "required reason" APIs the
binary uses — decided from `nm -u` on the built executable; expected:
file timestamps (`stat`), system boot time (cpal's iOS host calls
`mach_absolute_time`; std's `Instant` uses `clock_gettime`, which is
not on the list), disk space if `sysinfo` reads it — and the
`ITSAppUsesNonExemptEncryption` answer (standard TLS 1.3 through
rustls). The build number already grows with every commit (D25).

## 12. Milestones

All but the last of the first draft's are behind us; what they left
open is in PRODUCTION-PLAN.md.

| M | was | became |
|---|---|---|
| M0 | toolchain, hello and the client in the simulator | done 2026-10-06 (D28, D29); `run.sh --hello` (`hello.rs`) is still the smoke test |
| M1 | the client on the iPhone | done 2026-10-07 on an iPhone SE (D38–D46) |
| M2 | files, links, localized permission strings | files and links done early (D31); the strings are MOB-19 |
| M3 | voice: the session, VPIO, routes, interruptions, the record permission | VPIO and interruptions done (D47, D51); activation on join, routes and the record permission are MOB-19, "paused — call" MOB-9 |
| M4 | viewing; resilience; the DM notification | viewing (D30) and notifications (D50) done; the background task, network change, NAT64 and `SO_NET_SERVICE_TYPE` are MOB-10, CLI-10 and MOB-19 |
| M5 | release: TestFlight, privacy manifest, icon without alpha | open: MOB-2, MOB-3, MOB-5, MOB-6; Ad Hoc from CI meanwhile (D37) |

Later (roughly in order): VPIO on macOS (AEC-SPEC E8), push
notifications (APNs + the server), CallKit, an `AVRoutePickerView`
button, the Keychain for the identity key, an edge-swipe back,
`NWPathMonitor`, iPad layouts, sharing the phone's screen (a ReplayKit
broadcast extension: a separate process with a ~50 MB memory limit and
VideoToolbox encoding).

## 13. Risks

| risk | where it stands |
|---|---|
| Skia has no prebuilt for the iOS targets | retired: the simulator and device prebuilts download and draw (D28) |
| The engine, libopus and ring do not compile for iOS | retired: they build in Xcode unchanged (§14, after D28); the sandbox type-checks iOS code (D15) |
| Xcode's environment leaking into cargo's host builds (`SDKROOT`, `PATH`) | retired: Slint's script lineage, the PATH fix and a separate target directory (`target/xcode/`) |
| Background/foreground notifications under the scene life cycle | `lifecycle: foreground` seen in the simulator; `background` / `host: app hidden` still to see in a log (SESSION-GUIDE §5) |
| Text input limited to `UIKeyInput` | open: Spanish and Portuguese type fine; autocorrect is the gap — upstream `UITextInput`, or a native text view over the composer |
| VPIO side effects: quieter loudspeaker, ducking, 48 kHz | open until a phone log confirms D47 (SESSION-GUIDE §5) |
| Free provisioning expires every 7 days | the paid program's Ad Hoc builds (D37) last a year |
| IPv6-only networks | CLI-10: `getaddrinfo` for literals |
| App Store Connect's privacy-manifest and icon checks | MOB-2, MOB-3 (§11) |
| Skia falls back to the CPU without telling the app | retired: the logs show Skia compiling Metal shaders; if it ever falls back, `BackendSelector::require_metal()` fails loudly instead |
| Skia's 1-second wait on the Metal shader compiler (Ganesh skips that draw for the frame) | seen once in the simulator (D28), no visible effect; Slint's to fix if it ever shows |
| Half-drawn glyphs now and then on the phone | open: `run.sh --renderer` tells where they come from (D40) |
| Slint 1.18 compiles wgpu into every winit + Skia build | accepted (+4 MB, +35 s on a minimal desktop app, D14) |

## 14. Decisions taken after the first draft

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

| # | decision | why |
|---|---|---|
| D37 | **An iOS build in GitHub Actions** (2026-10-07, `.github/workflows/ios-release.yml`), the Android APK's sibling: on a published release (or by hand), on the `macos-26` runner with Xcode 26.6 pinned, `xcodebuild archive` of the XcodeGen project for `generic/platform=iOS` (the same cargo phase as on the Mac; build number = the run number; the tag must be the crates' version). With three secrets — an *Apple Distribution* `.p12`, its password, an *Ad Hoc* profile — it signs in a throwaway keychain, exports a `release-testing` `.ipa` and attaches it to the release; it checks the identity is in the `.p12`, the profile is for the app id (`IOS_BUNDLE_ID`, default `dev.baucord`) and lists devices, and the app is arm64 with `minos` 17.0. Without the secrets it builds unsigned and leaves the bundle as an artifact only (iOS installs nothing unsigned). `CMAKE_POLICY_VERSION_MINIMUM=3.5` for the runner's CMake 4 (libopus), as the desktop release does. The dSYMs ride along as an artifact. ios/README.md §3 has both ways onto a phone: `run.sh --device` (free Apple ID, 7 days) and the CI `.ipa` over USB (Devices window or Apple Configurator; paid program, a year, the profile's devices). Checked with actionlint + shellcheck and the export options generated and parsed; not yet run on GitHub | iOS has no APK: what installs is decided by signing, so CI can only hand out something installable with a paid account's distribution identity; TestFlight (M5) is the next step from the same archive |

| # | decision | why |
|---|---|---|
| D38 | **`run.sh --device` settles the signing team itself** (2026-10-07). The first phone run stopped with "No Account for Team "ABCDE12345"" and "No profiles for 'dev.baucord' were found": `Config/Local.xcconfig` had been copied from the example with its placeholder team, and the README's "Xcode → Settings → Accounts → the team ID" does not show a Personal Team's ID anywhere. Now, for `--device`, when `Local.xcconfig` is missing, sets no team, or still has the example's value, `run.sh` reads the keychain's *Apple Development* certificates (`security find-certificate -a -p`, each subject's `OU`): one team → written into `Local.xcconfig` (made from the example if missing, the rest of the file kept) and said; none → how to make the certificate; several → the list, and which line to set. Xcode's own signing failures get plain words: "No Account for Team" (sign in to that Apple ID in Xcode), "cannot be registered to your development team" (the bundle id is taken: `BAUCORD_BUNDLE_ID`), a failed install names Developer Mode and the free account's 3-app limit. The example and README say where the team ID is (the certificate's `OU`). Tested under bash 3.2 with a stand-in `security` and real `openssl` (both subject formats, `OU=X` and `OU = X`): placeholder → written, missing file → created, no certificate, two teams, the same certificate twice, a real team left alone, the simulator untouched, both Xcode failures; shellcheck clean | a placeholder that looks like a value is a trap; the machine knows the answer |

| # | decision | why |
|---|---|---|
| D39 | **The app's own measure of the safe area, as a floor** (2026-10-07; `insets.rs`, `State.host-inset-*`, `main.slint`). On the user's iPhone SE (750×1334, a 20 pt status bar, no notch) the main page started under the status bar: whatever Slint's `safe-area-insets` held, its top was 0. Slint computes it from winit's inner and outer geometry at each resize (`winitwindowadapter.rs` `safe_area_inset`), and the D34 probe already forces a resize whenever `UIKit`'s safe area changes, so the gap is upstream of us and not reproducible here. Now the probe also reads `UIWindow.safeAreaInsets` itself on every change (and once when it is installed), logs it — `safe area: top 20 left 0 bottom 0 right 0 pt` — and hands it to `frontend::app::set_host_insets` (any thread; stashed until the window exists). `main.slint` keeps every page clear of `max(safe-area-insets, State.host-inset-*)` per edge (`edge-*`; the bottom also of the keyboard), the stream page's chrome too. Rendered headless at 375×667: with a host inset of 20 and Slint's 0, the page starts 20 px down. Android never sets it (all zero) | either value alone was wrong once (D34: stale after a rotation; this: 0 on an SE); the larger of two independent readings cannot put the UI under a system bar, and the log line says which one was right |
| D40 | **Half-drawn glyphs: a diagnosis and a switch, no workaround** (2026-10-07; `render.rs`, `run.sh --renderer`). The phone showed, now and then, glyphs with their top half missing or replaced by a block — in "Connected", in the latency line's digits, and in a member count that had not changed — staying wrong until the line was redrawn. The strings are right and an unchanged glyph cannot be laid out wrong: the glyph *image* is wrong, i.e. Skia's glyph atlas on the GPU (Slint 1.18 draws Skia on iOS through wgpu's Metal backend, sharing wgpu's command queue — there is no other Skia GPU path on Apple platforms in 1.18) or the rasteriser that fills it. Read: Slint's frame path (`wgpu_30_surface.rs`: a wgpu clear, Skia's draw and `flush`, `submit(None)`, a wgpu ordering submission, `present`), Skia m153's Metal staging-buffer and command-buffer tracking (`GrMtlGpu.mm`: staging slices owned by their command buffer until Metal reports it complete) — nothing visibly wrong, so no blind fix. A per-frame GPU wait in a `BeforeRendering` notifier was written and dropped: it needs Slint's `unstable-wgpu-30` API, which re-locks 14 packages (the wasm-bindgen family, femtovg), for a guess. Instead `ios/run.sh --renderer skia-software` (Skia on the CPU: the same glyph rasteriser, no GPU atlas) or `--renderer software` (Slint's own renderer and rasteriser) launches the app with `BAUCORD_RENDERER` set — `SIMCTL_CHILD_*` in the simulator, `devicectl --environment-variables` on the phone — and the log says `renderer: … (a diagnostic launch)`. Gone with `skia-software` → the GPU path (report to Slint with a screenshot); still there → Skia's rasteriser; gone only with `software` → Skia's glyphs, and `software` is a candidate default for the phone's mostly static UI | a cosmetic, intermittent fault in a third-party renderer is answered by telling where it lives before changing anything |
| D41 | **The worker threads sleep when there is nothing to do** (2026-10-07; `client/src/inbound_wake.rs`, `network.rs`, `voice.rs`, `audio/src/input.rs`; ANDROID-SPEC.md D43). The network thread woke every **1 ms** for as long as a session lasted — it could not hear the socket, so it polled it — a thousand wakes a second in a quiet channel or a chat-only session, enough on its own to keep a phone's CPU out of its deep idle states. Now (Unix: iOS, Android, macOS, Linux) a small thread blocks in `poll(2)` on a `dup` of the socket (`transport::ReadWaiter`) and rings a channel the loop `select!`s beside its control channel, then waits to be re-armed once the loop has drained the socket. The loop sleeps up to 100 ms (`QUIET_TICK`) when the transport has no per-tick work (`Driver::wants_tick`: lane bytes the stream would not take yet, open or unacknowledged video groups, paced uploads, a deferred close, unpolled transmits), else 1 ms as before; always capped by the transport's next timer. Measured on loopback (`client/tests/network_idle.rs`): **10 wakes in a quiet second instead of ~900**, and a datagram reaches the voice thread in **0.25 ms** (median of 8; it was up to the 1 ms tick). Windows keeps the 1 ms poll. On the way: `has_backlog` was never set (`update` ignored `recv_batch`'s `drained`), so a socket left with more than 4096 datagrams waited a tick; it is set now. The voice loop's idle cadence (meters, the speaking edge, device supervision) went from 5 ms to 20 ms — 200 → 50 wakes a second while in voice; packets and commands still wake it at once. A muted microphone's capture worker waited 10 ms between empty looks (the callback stops ringing while muted); now 50 ms — unmuting still rings at once. Debug builds optimise `video` and `audio` like dependencies (`[profile.dev.package.*]`), and the decoder's BGRA→RGBA buffer is no longer zero-filled before every frame it overwrites | the phone's battery: wakes, not work, were the waste, and the network loop's thousand a second was all of it; the readiness wake is also faster than the poll was |
| D42 | **In the background, nothing is drawn for nobody** (2026-10-07; `frontend::app::set_host_visible`, `lifecycle.rs`; Android D43). In voice the app keeps running in the background (the audio mode), and so did its screen work: the 2-second latency and resource lines, the stream stats, GIF animation (30 frames a second while one played), and every frame of a watched stream — copied, set, and drawn (Slint's Skia renderer ignores winit's `Occluded`, so a frame in the background would wait for a drawable or submit GPU work iOS refuses). Now the scene's background/foreground calls `set_host_visible(false/true)`: `main_on_screen()` is false (no stats lines), the GIF ticker stops and `host_shown` restarts it, the present thread drops decoded frames before the copy (the decoder keeps going — its pictures depend on each other — so the first frame back is current), and the 2.5 s device poll skips (the first poll back reports what changed; the engine follows device loss on its own). Voice, chat and the roster are untouched. The present thread's copy is also one write now (`SharedPixelBuffer::clone_from_slice`; `new` zero-filled 8 MB per 1080p frame first) | voice in the background is the feature; drawing there is waste, and on iOS a GPU submission from the background is refused anyway |

| # | decision | why |
|---|---|---|
| D43 | **Your own microphone and headphones on the stream page** (2026-10-07, shared `viewer.slint`; ANDROID-SPEC.md D44). Watching full screen, muting yourself meant leaving the stream. The bar now starts with the mic and headphones (the sidebar's toggles: `Logic.toggle-mute` / `toggle-deafen`, red when on), a thin rule, then the stream's own speaker and level — whose tooltips now say "Mute the stream", so two mutes are never confused. The desktop's viewer windows have them too (`StreamViewerWindow.self-*`, pushed by `set_muted` / `handle_toggle_deafen` through `Viewer::set_self_audio`), so a fullscreen stream on a PC can be left muted as well. The bar fits narrower surfaces now: under 480 px of room (a phone upright, a small window) it tightens its spacing, shortens the level slider (130 → 96, or 64 under 360 px) and Stop watching becomes an ✕ — rendered at 320, 375, 667 and 852 px wide with every control on the bar | the controls belong where the person is looking |
| D44 | **Zoom into the stream** (2026-10-07, `viewer.slint`; ANDROID-SPEC.md D44). To read small text on a shared screen: two fingers pinch (Slint 1.18's `ScaleRotateGestureHandler`, also a Mac trackpad's pinch), the mouse wheel zooms where the pointer is (one notch ≈ ×1.16), `+` `-` `0` from the keyboard; up to ×8 of the fitted picture. Zoomed — or filling the screen with an edge cropped — one finger or the mouse drags it, held so the picture always covers the surface where it is larger than it. A chip top-right says how far ("2.4×") and a tap on it goes back to the whole picture; so does a double-tap, which otherwise keeps its job (fullscreen on the desktop, fit ⇄ fill on a phone on its side) and on a phone upright zooms ×2.5 where it landed. The zoom is kept, relative to the fit, through a rotation or a change of the sharer's resolution; a new stream starts whole. Found on the way: Slint 1.18 turns two fingers into increments of the finger-distance ratio and its gesture handler multiplies them up, so on a touchscreen `scale` comes out near e^(r−1) — pinching to twice the distance zoomed ×2.7, to three times ×7; the page (`pinch-from-touch`) takes `1 + ln(scale)` back to the ratio, a trackpad's value is used as it is (rendered: a simulated pinch to ×2.5 shows "2.4×", the gap being the simulation's ten large steps). Also fixed: the faded bar and chips still took taps, so a tap to bring the controls back could land on an invisible Stop watching; faded out, they are now not there (`visible: opacity > 0.01`) | reading a shared terminal or document on a phone needs it; the gestures are the ones every photo viewer has |
| D45 | **The screen stays on while a stream is watched** (2026-10-07; `frontend::host::keep_screen_on`, `host.rs`, ANDROID-SPEC.md D45). The phone dimmed and locked in the middle of a stream like any idle app. Now the stream page asks the host to keep the screen on when it opens and lets go when it closes (any way it closes: Stop, the sharer ending, a disconnect). iOS: `UIApplication.idleTimerDisabled`, which iOS applies only while the app is in front; Android: the window's `FLAG_KEEP_SCREEN_ON` (`BaucordActivity.keepScreenOn`, on the UI thread), likewise only while the window is visible. Chat and the voice lists let the phone sleep as before. The desktop is unchanged (its viewer is a window the user arranges) | what every video player does, and only for as long as there is a picture to watch |
| D46 | **The status bar as a floor for the top inset** (2026-10-07; `insets.rs`). The phone's log after D39 read `safe area: top 0 …` with the status bar on screen: `UIKit`'s window safe area on the SE left it out, so D39's floor was 0 too and the main page still started under the clock. The top is now the larger of the safe area and the status bar's height while the scene shows one (`windowScene.statusBarManager`, `isStatusBarHidden` / `statusBarFrame`; objc2-ui-kit feature `UIStatusBarManager`). The probe also reports from `layoutSubviews`, which a rotation reaches even when only the status bar came or went (the SE hides it in landscape). The log line names it: `safe area: top 20 … (status bar 20)` | the D39 check the log was for; notched phones are unchanged (their safe area is already larger than the bar) |
| D47 | **Echo cancellation: the voice-processing unit** (2026-10-07; `audio/src/native/vpio.rs`, `native/avsession.rs`, `native/pump.rs`; AEC-SPEC.md E10). The phone's `aec: none (not on this platform yet)` was accurate: §6.3 was M3 and had not been written. It is now, as §6.3 planned, with three things §6 did not say: (1) the plan opens the unit before either side starts and holds it (`vpio::prepare`): its playback is the canceller's reference, so both sides attach to it or — if it will not open — both take plain streams; the sides still open on their own (a process-wide `Weak`, a one-slot mailbox per side, a dropped side flags its half off, halves freed off the I/O thread), so a reopen of either side works; a failure of either reopens both (`SHARED_VOICE_UNIT`). (2) `plan_aec` sets the mode (`voiceChat` on the call path, `default` off it) with both streams closed, re-stating the host's options; if the mode change put the call on the earpiece the speaker override brings back the loudspeaker. (3) The interruption-ended and media-services-reset notifications mark both sides failed, and `service()` reopens them (cpal's own streams already recover by themselves). Automatic follows the route (`aec::ios_output_kind`: headphones, Bluetooth, USB → off; the speaker, the earpiece, AirPlay/car/HDMI → on). Not done from §6.2–6.5: activation on join / deactivation on leave with `notifyOthersOnDeactivation`, the route picker (§6.4), the "paused — call" line. Known gaps: a watched stream's sound still plays through its own `RemoteIO`, outside the canceller's reference; an interruption that never posts *ended* leaves voice stopped until the next reopen. Untested on a device: written against coreaudio-rs 0.14.2, objc2-audio-toolbox and objc2-avf-audio 0.3.2 and type-checked for `aarch64-apple-ios` | the voice path on the loudspeaker is the iPhone's main use, and VPIO is the canceller Apple's own calls use |
| D48 | **Settings → Debug: the log, readable and sendable from the app** (2026-10-07, shared; `app-core/src/logfile.rs`, `settingsui.rs`, `settings.slint`; ANDROID-SPEC.md D47). Asked how users get logs off a phone: on iOS through Files, on Android not at all without a cable — while the "send my log" upload (protocol, client, the server filing it under `logs/`) had existed for months with nothing calling it. A Debug tab now holds the log level (moved from General), **Send log to server** (the log's end with a header line naming the version and OS, up to `MAX_LOG_UPLOAD_BYTES` = 2 MB, across `baucord.log.1` and `baucord.log`; one per `LOG_UPLOAD_MIN_INTERVAL` = 10 min, the client says how long to wait instead of the network thread dropping it silently), **Clear log** (truncates in place — every host keeps the file open for appending, so the next line starts it — and deletes `.1`), and the **last 100 lines** in a small monospace box that follows the end unless scrolled up, read again every 2 s while the tab shows (the settings timer, 16th tick; only changed text is set). Each host says where its file is (`frontend::logfile::set_path`); a desktop debug build, which logs to the terminal, says so instead | the operator gets the log without the user knowing where it lives |
| D49 | **Shared UI from the Android reports** (2026-10-07; ANDROID-SPEC.md D48–D50). The sidebar's header is one row — the server's name, then Invite users, New channel and Admin as permitted, icons alone when the words do not fit. The stream page has a **Rotate** button while the screen is upright, for a phone with its rotation locked: the picture and its chrome turn a quarter on a stage inside the panel, taps turned back by Slint; a screen that turns by itself undoes it. The keyboard-composition fix (a composing word committed before Send, Enter or a button acts; no selection colours on it) is Android-only in effect: iOS's Latin keyboards do not compose, and the code checks `Platform.os` | the same app on both phones |
| D50 | **Notifications, and the chime under the ring/silent switch** (2026-10-07; `crates/ios/src/notices.rs`, `host.rs`, `main.rs`, `Cargo.toml`; ANDROID-SPEC.md D52 for the shared part in `frontend`). The DM chime played with the phone on silent: it is mixed into the voice session's audio, and a `PlayAndRecord` session ignores the switch — and no public API reads the switch. So on iOS every alert chime goes through the system: `chime_route()` is `System`, and the frontend posts a *sound-only* notice (identifier `fg:<key>`) that the `UNUserNotificationCenterDelegate` presents with `.sound` alone while the app is on screen — iOS then applies the switch and Focus to it as to any notification. Off screen (the scene in the background, the process kept by the audio session) a DM is a local notification (banner, list, sound; identifier `dm:<user hex>`, one per conversation) whose tap opens the conversation through `frontend::host::open_conversation`, and the first voice into empty rooms is "Ana joined General" (identifier `join`). The sound is the app's own DM chime, written each launch to `Library/Sounds/baucord-message.wav` (`frontend::host::alert_sound_wav`); without it, the default sound. The permission (alert + sound) is asked on the first connect of a launch (`keep_alive`), not at start. Local notifications only — no push entitlement; a suspended or closed app gets nothing (PRODUCTION-PLAN.md PRD-1). New dependency: `objc2-user-notifications` 0.3 (UNUserNotificationCenter and friends). Untested on a device | the only sound route that honours the switch is the system's; one mechanism covers the chime on screen and the notification off screen |
| D51 | **No microphone in the lobby or while muted** (2026-10-07; `audio/src/lib.rs` `set_presence`, `audio/src/native/vpio.rs` `Prepared::set_mic_muted`; ANDROID-SPEC.md D53 for the shared part). Users saw the orange recording indicator while sitting in the lobby and while muted: every connect opened the voice-processing unit with its microphone, and mute only dropped the samples. Now, out of a voice room the engine plays through a **plain output only** — no VPIO, no input, the session's default mode — because an iPhone app with no audio running is suspended in the background, and the lobby is where people wait for D50's "Ana joined General". Joining a room re-plans onto the VPIO unit as before (opened already muted when joining muted, so it never records first). The plain output has a `SessionWatch` of its own, so a call or a media-services reset in the lobby restarts it rather than leaving the app without the audio that keeps it running. **Muted or deafened on the unit**, the microphone is muted on the unit itself (`kAUVoiceIOProperty_MuteOutput`, what `AVAudioEngine`'s `voiceProcessingInputMuted` sets): fast, playback and the canceller untouched, and — per LiveKit's documentation of the same mode — the indicator turns off; iOS 17+ may play its own mute tone. On plain streams (headphones without echo cancellation) a muted microphone is closed. If a device shows the indicator staying on under the unit's mute, the fallback is LiveKit's "restart" mode: re-plan to the plain output while muted (a brief gap in what you hear at each toggle). A reconnect's rejoin from the background may not get the microphone until the app is in front (iOS will not start recording from the background); the engine retries it. Untested on a device | the recording indicator follows the microphone, and the lobby needs none |
| D52 | **A watched stream plays through the VPIO unit in a room** (2026-10-08; ANDROID-SPEC.md D56 for the mechanism, `audio/src/stream/mix.rs`). On the voice-processing unit the stream had an output of its own beside it — outside what VPIO cancels (its reference is its own output only), and exposed to the ducking VPIO applies to other audio. Now, while the voice output is the VPIO unit, the stream is mixed into it (mono, its own volume and mute, untouched by deafen): one route, cancelled like the voice. In the lobby (plain output, D51) and off the call path the stream keeps its own stereo output. Untested on a device |
| D53 | **Settings → Debug: Copy log** (2026-10-08, shared; ANDROID-SPEC.md D58). The log's end onto the general pasteboard (`host.rs` `copy_text`, `UIPasteboard`, on the main thread), under a line naming the build, the platform, the time and the file; 128 KB as on Android. The phones had no clipboard write before, so an invite code's copy now lands too | a pasted log that says where it came from |
| D54 | **A stream's stereo becomes mono without cancelling** (2026-10-08, 1.1.9; ANDROID-SPEC.md D60). The VPIO path mixes a watched stream into the voice output (D52) through the same phase-aligned downmix (`stream::downmix`): out-of-phase sound, which `(L + R) / 2` silenced, plays at its level | shared code; the iPhone has the same mono call path |
| D55 | **A stream backlog drains by playing faster, not by a skip** (2026-10-08, 1.1.9; ANDROID-SPEC.md D61). The VPIO path's stream mix (D52) shares `stream::mix` with Android: past 180 ms queued a source plays 1–4 % faster until it is back at 120 ms, instead of dropping 150 ms at once past 300 ms; only past 400 ms is anything dropped | as ANDROID-SPEC.md D61 |
| D56 | **Blocks, reports, deleting the account, and the terms** (2026-10-09, 1.2, shared; ANDROID-SPEC.md D64; PERSISTENCE-SPEC §12 S1). The same shared UI as on Android: Block/Report in a person's and a message's menu, the report dialog, Settings → Profile's blocked list and Delete my account, the terms line on the connect card, and a long press on a chat row for its menu (UIKit's own long press is not involved: the row's `Timer` reads the touch). `keep_alive` takes `in_room` now; iOS ignores it (the audio background mode is what keeps a call running, D13) | the App Store's guideline 1.2 asks the same as Play's user-content policy |
| D57 | **The 1.2.1 review: what changed under the phone** (2026-10-10, 1.2.1, shared; ANDROID-SPEC.md D67, which has the detail; amends D41, D42, D51, D55). D41's `QUIET_TICK`, `Driver::wants_tick`, "Windows keeps the 1 ms poll" and "10 wakes" are history: every loop waits for a datagram, a command or `Driver::next_deadline()`, and Windows has its socket wake too (review P1-1, P1-2) — a quiet second is 0 loop wakes plus the watcher's 2. Voice packets skip the voice thread, which wakes once a second (P1-5); the audio workers wait on their device's latch (P1-4). D42 changes: in the background a watched stream is no longer decoded — its frames are dropped before the jitter buffer, its sound plays on, and on return it waits for a keyframe and asks for one (P1-7). D51: the AFK room counts as out of a room — the microphone closes there too (P1-6). D55: the pacing lives in `stream::pace`, levels unchanged | as ANDROID-SPEC.md D67 |
| D58 | **An iPhone in the lobby is left alone** (2026-10-12, 1.2.1, shared; ANDROID-SPEC.md D68 for the whole of it; review P2-49). The client's QUIC keep-alive is 5 s, not 2 s; a server from 1.2.1 on sends no `VoiceReceiverReport` to a session that sends no voice (`QUIET_REPORTS`); the UI is not woken for link stats while the app is off screen. In a room nothing changes. Not measured on a device | the lobby is where an iPhone waits, on battery and often on cellular |
| D59 | **The review's seventh batch on iOS** (2026-10-12, 1.2.1; ANDROID-SPEC.md D69 for what is shared; review P2-43, P3-44, P3-45, P3-49, P3-50, P2-29). *The decoded picture*: `videotoolbox.rs` copies the visible rows of the BGRA pixel buffer out as they are (`yuv::pack_rows`, no cleared buffer first) and hands the frame on as `PictureFormat::Bgra`; the present thread swaps red and blue as it writes the buffer the UI draws from (`yuv::bgra_to_rgba_into`, in `frontend/src/present.rs`). One pass over the pixels and no zero-fill per frame, where the decoder cleared, swapped, and the picture was then copied again. Type-checked for `aarch64-apple-ios`; **a share with blue faces means the swap is missing on the path that drew it.** *Shared code*: the log tee, the panic hook, the pick slot and the safe file names are `app_core`'s (`logfile::FileSink` rolls the log at 5 MB as it grows; `files::PickSlot` keeps D36's claim-once rule). The memory figure in the resource line and `/mem` is the physical footprint (`task_info`, as on macOS), not `sysinfo`'s resident size, and the build no longer has `sysinfo`. *Edition*: `audio` and `video` are on 2024; `audio`'s iOS code was read for what that edition rejects, not compiled. *App Review*: the lobby's silent output is on the App Store decision list (PRODUCTION-PLAN.md §6, 2): guideline 2.5.4 rejects the `audio` background mode when nothing audible plays. |
| D60 | **Settings → Audio opens the microphone; the meters are eased** (2026-10-12, 1.2.1, shared; ANDROID-SPEC.md D70; amends D51; first iPhone look). *The microphone*: the Audio tab's capture bar was dead unless in a room — D51 closes the microphone everywhere else — so a microphone could not be set up before there was someone to talk to. While the tab is on screen (`settingsui.rs` `pace_meter_timer` → `AppState::set_audio_tab_open`; on the narrow page: the Audio section open, not the list with Audio selected behind it) the microphone is open. **Not connected**, an engine is started with no session (`VoiceControl::Start` with `our_client_id: None`): its link to the network thread is `local_only` for as long as it lives, so nothing it hears can be sent, and mute and push-to-talk do not hold it shut — there is no session to hold a key in. A session always brings a new engine. **Connected, in the lobby or the AFK room**, the session's engine is told it is in a room (the voice-processing unit, as a join would) and the link's `testing` flag keeps the packets and the speaking light here; mute rules as in a room. **In a room** nothing changes. It never outlives the screen: leaving the tab, closing Settings, or the app going to the background (`AppState::host_hidden`, from `set_host_visible(false)`) closes it at once — an open microphone would otherwise keep an unconnected app from being suspended. The first time, the tab is where iOS asks for the microphone. *The meters*: read 20 times a second while the tab shows (was 8) and held by `app_core::meter` — a rise taken at once, a fall over 300 ms — and a bar is set only when it moved 0.4% of its length, so silence draws nothing; the bar glides to each value over 80 ms (`animate width`, back in `widgets.slint`). The first cut — 30 readings, a 150 ms fall, no glide — read as jumping on the phone: it followed every syllable to the floor, and whatever rate the timer reached was the rate the bar was seen to step at. When the tab leaves the screen one line is logged — `settings: audio tab on screen T s: meters read N times and drawn M; an engine behind them …; microphone with no test running first heard …, peak …; during a test, peak …` (`app_core::meter::Run`) — which tells a slow timer, an engine that never started, a silent microphone and a late start apart from a pasted log; `microphone: …` lines say what the app did about the engine each time the tab or a test changed, and which engine (the session's from `Init`, or one for this device alone). *The test*: on a phone it plays the voice back half a second late (a second at first; `audio::LOOPBACK_DELAY`, `audio/src/loopback.rs`; the hint says so). Played at once through the loudspeaker it had the echo canceller removing the user's voice from the microphone while the user was still saying the same words, and sounded about to feed back. The desktop's test is live as before. Type-checked on Linux; the hold, the delay line and the engine's mute rule are unit-tested; **not run on a phone** | a meter is for setting the microphone up, which is done before a call, not in one |
| D61 | **Settings and the admin panel are pages on every host; the admin panel on the iPhone** (2026-10-15, 1.2.1, shared; ANDROID-SPEC.md D71 for the whole of it; amends D29's toast). The desktop's Settings and admin windows became pages over its main window, as the phones' Settings already was; the admin panel is now a page on the iPhone too — the Admin chip opens a list of its nine tabs, a tab opens as a page, the top bar's arrow steps back out (there is no Back key, §9), and every tab is laid out for a phone's width. The `host-single-window` toast is gone. Driven headless on Linux at 390×844; **not run on an iPhone** | as Android's |
| D62 | **Settings' Close puts back; a person's actions in one menu; the pages keep to a column** (2026-10-15, 1.2.1, shared; ANDROID-SPEC.md D72 for the whole of it). On the iPhone: Settings' footer is Close and Save and close, Reset to defaults the section list's last row; the admin panel's Users tab has one Actions button per person, opening the app's menu. Driven headless at 390×844; **not run on an iPhone** | as Android's |
