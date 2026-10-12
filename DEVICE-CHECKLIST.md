# baucord — what to check on real machines (1.2.1)

> **Status (2026-10-12):** written after the review work of 1.2.1
> (`PRODUCTION-PLAN.md` §2a, §7). All of that work was done in a
> sandbox that is Linux on ARM: the Windows- and Android-only Rust was
> type-checked there, the iOS code was not even compiled, and nothing
> platform-specific was *run*. This is the list of what has to be seen
> on the real thing before 1.2.1 ships. Tick a line when it holds; a
> line that fails goes into `PRODUCTION-PLAN.md` with what §4 asks for.

Each line is *do this → expect that*. `log:` is a line to find in
`baucord.log` (desktop: Settings → Debug shows it and copies it;
phones: `adb logcat -s baucord`, `ios/run.sh`'s stream). The tag in
brackets is the review item or decision the line is there for.

Three kinds of line:

- **builds** — code changed without a compiler for that platform;
- **new** — behaviour that is meant to be different from 1.2.0;
- **same** — a refactor: must behave exactly as before.

The "changed blind" lists are `git diff v1.2.0..HEAD` for the files
only that platform compiles. macOS and the Linux desktop are not
covered here.

---

## 1. Windows

Windows 11 and, for §1.2, one Windows 10 machine. A laptop with
built-in speakers is the machine that matters most.

### 1.0 Build

- [ ] **builds** — a release build of the desktop compiles with MSVC.
  The engine crates were checked for `x86_64-pc-windows-gnu` only;
  `frontend`'s Windows code not at all. If it stops, the Windows
  halves changed blind are in `frontend/src/hotkeys.rs` (the keybind
  watcher, rewritten for P1-3), `host.rs` (`open_sound_settings`, the
  `DesktopHooks` dialogs), `clipboard.rs`, `instance.rs` and
  `updater.rs`.
- [ ] The first release run on GitHub: the build restores a cache
  warmed from `main`, installs the toolchain `rust-toolchain.toml`
  names, and has no `sccache` step [P2-50, P3-48].

### 1.1 Voice and devices [P2-55, P1-18, P2-21, P3-27]

- [ ] **same** — laptop speakers, Echo cancellation *Automatic*, join a
  voice room. Nobody hears themselves back. log: `Echo cancellation:
  Auto on speakers: voice-call path`, both `Native WASAPI` lines with
  `category=Communications`, `aec: platform (speakers)`.
- [ ] **same** — pick a headset as output in Settings → Audio. log:
  `Reopening voice streams (output device changed)`, both `Native
  WASAPI` lines with `category=Other`, `aec: none (headphones)`.
- [ ] **same** — pick a second pair of speakers (a monitor's). log: the
  same reopening line, then `AEC reference endpoint set`.
- [ ] **same** — with "Default" picked, change Windows' default
  playback device. Sound moves by itself within a couple of seconds.
- [ ] **same** — unplug the picked output while in a room. The
  microphone keeps working; plug it back and sound returns within a
  few seconds without touching the app. log: `Output restart failed
  (stream failed: …); will retry`, later `Output started late (…)`.
- [ ] **same** — unplug a USB microphone that is the default. Capture
  moves to the next default; the speaking light is not stuck on.
- [ ] **new** — switch a Bluetooth headset off while it is in use.
  Both sides recover within seconds, not never [P2-21].
- [ ] **new** — pick a microphone, unplug it, pick it again (it fails).
  The line under Echo cancellation no longer says the canceller is
  running [P2-55].
- [ ] **same** — Echo cancellation *Off*, then *Automatic*. log:
  `Reopening voice streams (echo cancellation setting changed)` each
  time, and nothing when going Automatic → Always on speakers.
- [ ] **same** — in the lobby (no room) nobody hears you and the
  speaking light stays off; joining a room is heard at once [P1-6].
- [ ] **same** — mute, deafen, push-to-talk; two hours in a room with
  no growing delay. log every 10 s: `audio stats:` — note what
  `Drift a/b` says after an hour [P3-22].

### 1.2 Ducking — other apps getting quieter [CLI-11, AEC-SPEC E14, E15]

Play music in another app first, at a steady volume.

- [ ] **Windows 11, speakers, Automatic** — connect. The music keeps its
  volume. log: the `Native WASAPI render:` line ends `do-not-duck=
  requested, own-volume=kept, Windows during calls: …`. Write down
  what the `capture:` line says for `do-not-duck=` — that answer is
  not known yet.
- [ ] **new** — **headphones, output "Default", Automatic** — the music
  keeps its volume. log: `category=Other`. (1.2.1 as first written
  would have ducked here.)
- [ ] **new** — in voice, watch a stream whose sound plays on another
  output than the voice. The stream is not lowered.
- [ ] **Windows 10, speakers, Automatic** — the music *is* lowered
  (Windows 10 cannot be asked not to). Then:
  - [ ] **new** — a toast says so, once; it does not come back at the
    next launch.
  - [ ] **new** — Settings → Audio shows an amber notice under the
    echo-cancellation line, with a button.
  - [ ] **builds** — the button opens Windows' Sound dialog on its
    *Communications* tab.
  - [ ] **new** — choose "Do nothing" there: the notice goes away
    within a second.
  - [ ] **Find out:** does the music come back up at once, or only
    after reconnecting? The notice assumes at once.

### 1.3 Sharing a screen [P2-22, P2-26, P2-41, P2-44, P3-28]

- [ ] **new** — share a screen where nothing moves; a second user
  starts watching a minute later. They get the picture at once. The
  sharer's stats line shows almost no bitrate while still [P2-41].
- [ ] **new** — share and watch another share at the same time, for a
  few minutes; stop each in turn; quit the app with both running. No
  stutter in one when the other is busy, and the app exits [P2-44].
- [ ] **new** — on a laptop with two graphics adapters, share the
  monitor driven by the *other* one [P2-26].
- [ ] **new** — share with system sound, then change the default output
  mid-share. The share's sound is back within about ten seconds, and
  the viewer's 5 s summary shows no absurd "skipped" count [P2-22].
- [ ] **new** — a 144 Hz monitor shared at 30 fps looks smooth [P3-28].
- [ ] **Known gap** — viewers do not see the sharer's mouse pointer
  [P2-25, not done]. Say whether that is so on your machine.

### 1.4 Doing nothing cheaply [P2-35, P2-37, P2-39]

- [ ] **new** — connected in the lobby, nothing shared or watched:
  `powercfg /energy` (or Sysinternals `clockres`) does not list
  baucord as holding the 1 ms timer. In a room it does [P2-35].
- [ ] **new** — minimized or in the tray: GIFs do not animate (CPU near
  zero in Task Manager) and resume when the window is back [P2-39].
- [ ] **new** — the log at its default level has no flood of `wasapi`
  lines during a call [P2-37].

### 1.5 The window, the chat, the dialogs

- [ ] **new** — "Start minimized" with auto-connect: after connecting,
  the window is still in the tray [P2-17].
- [ ] **new** — in a full-screen game, pull the network for ten
  seconds. The reconnect does not take the focus [P2-17].
- [ ] **new** — get kicked while the window is in the tray: it stays
  there. *Decide whether a kick or a ban should still bring it up.*
- [ ] **new** — in the composer, select text, right-click: Cut and Copy
  work; with nothing selected they are greyed [P2-16].
- [ ] **new** — PrintScreen, then Ctrl+V in the composer: the send
  preview opens. Copy cells in a spreadsheet: they paste as text
  [P2-18].
- [ ] **new** — "Save image" on a JPEG offers a `.jpg` name [P3-31].
- [ ] **new** — Settings → Audio open; plug in a USB headset; pick an
  entry. The device applied is the one clicked [P3-35].
- [ ] **new** — GIFs in chat play about three loops and freeze (they
  used to run 30 s); hovering replays; the gallery loops; the send
  preview animates [P2-59].
- [ ] **same** — each dialog (image preview, share, channel password,
  invite, channel, confirm, moderation, report): Escape closes, the
  buttons work, and in the three with a text field typing goes into
  it at once and Enter confirms [P2-58].
  - [ ] **Find out:** in those three, does the *first* click outside
    the card close the dialog? A harness says it takes two.
- [ ] **same** — right-click a text, code, picture and file row: each
    opens its menu; a click on a picture opens the gallery [P2-58].
- [ ] **same** — attach a file, save a received one, open it, open a
  link [P3-33].
- [ ] **same** — connect, disconnect and connect again twice, once in
  the middle of an upload: roster, conversations, mute and transfers
  start clean each time [P2-51].
- [ ] **same** — the admin panel: kick with a reason (the person sees
  it), lift a restriction, revoke an invite, delete a channel [P3-34].
- [ ] **same** — toggle each switch in Settings, close, reopen: kept
  [P3-35].
- [ ] **new** — "Install and restart" comes back up after an update
  (it used to close and stay closed).
- [ ] **new** — keybinds (push-to-talk, mute, deafen) work with another
  window focused, hold-to-talk releases when the key does, and none
  of them fires while disconnected. Rebind one in Settings and try
  again [P1-3].
- [ ] **same** — starting baucord a second time brings the first one's
  window up instead of opening another; a second `--profile` still
  opens its own.

---

## 2. Android

One phone on Android 14 or later and one older (10 or 11); a
Bluetooth headset. `android/README.md` has the build.

### 2.0 Build

- [ ] **builds** — the Gradle build compiles `crates/android`. Changed
  blind: `crates/android/src/host.rs` (the `notifies` hook is gone;
  three methods of `frontend::host::Hooks` have defaults now) [P3-47,
  P3-33], `crates/android/src/java.rs` (the `ndk-context` and
  `requestPermissions` fallbacks removed), `android/app/
  build.gradle.kts` and `gradle.properties` (the version read from
  the workspace, two flags gone) and the manifest
  (`ACCESS_NETWORK_STATE` gone). `aaudio.rs` and `mediacodec.rs` did
  meet the compiler.
- [ ] **same** — a first run on a clean install still asks for the
  microphone and the notification permission, each once.

### 2.1 Voice: the lobby, a room, mute, routes [P2-55, ANDROID-SPEC D53]

The engine's decisions are pinned by tests on a stand-in phone
(`audio/src/supervision_tests.rs`); these lines check that a real one
agrees with it.

- [ ] **same** — connect and stay in the lobby. No microphone
  indicator, media volume on the buttons. log: one `Echo
  cancellation: … plain streams`, and nothing repeating every 2 s.
- [ ] **same** — join a room unmuted on the loudspeaker. log:
  `Reopening voice streams (in a voice room)`, `voice-call path`. The
  buttons now set call volume, and others do not hear themselves.
- [ ] **same** — mute: the microphone indicator goes within a second
  (log: `Microphone closed (muted)`); unmute: `Microphone open`.
- [ ] **same** — connect a Bluetooth headset in a room: sound moves to
  it within a few seconds. log: `Reopening voice streams (output
  changed)`, `plain streams`.
- [ ] **same** — with the headset connected, pick *Speaker* in
  Settings → Audio: the call moves to the loudspeaker. Pick *Phone
  earpiece*: to the earpiece.
- [ ] **same** — leave the room: other apps' sound is back to normal.
- [ ] **same** — refuse the microphone permission, join, then grant
  it: the microphone opens without leaving the room.
- [ ] **same** — a phone call during a room; ten minutes with the
  screen off; Wi-Fi to cellular and back [TST-4].

### 2.2 What the app asks of the phone [P3-33, P3-47]

- [ ] **same** — attach a picture and a file; save a received file (it
  lands in Downloads, the toast names it); open it; open a link.
- [ ] **same** — the foreground-service notification says lobby or
  room as it should; a DM while the app is in the background
  notifies, and tapping it opens that conversation.
- [ ] **same** — a DM while the app is on screen chimes, and does not
  on silent or vibrate.

### 2.3 The screen, on a 360 dp phone

- [ ] **new** — the connect and startup cards have a margin on both
  sides, and "By connecting you agree to the / terms and conditions"
  is two lines, both tappable [P3-40].
- [ ] **same** — every dialog: Back closes it; the channel-password
  field has the keyboard, what is typed arrives, the keyboard's Enter
  joins [P2-58].
- [ ] **same** — hold a finger on a text, picture and file row: its
  menu opens, and lifting the finger from a picture does not also
  open the gallery [P2-58].
- [ ] **new** — a picture's menu has no "Copy image"; "Save image"
  writes Downloads with the right extension [P3-31].
- [ ] **new** — tapping a roster row opens its menu with no profile
  card flashing underneath [P3-37].
- [ ] **new** — the emoji palette opens at once and flicks [P3-38].
- [ ] **new** — GIFs stop while the app is in the background and carry
  on when it is back [P2-39].
- [ ] **same** — watch a stream: picture and sound, rotate, leave the
  app and come back (the picture returns at once) [P1-7, P2-44].

### 2.4 Not done yet on Android

P2-27 (saving a 50 MiB file copies it through the Java heap), P2-28
(the wake lock without a timeout), P3-44 and P3-45 (the log tee and its
rolling), and of P3-46 the Gradle ABI default (a local release build
still ships x86_64).

---

## 3. iOS

An older and a current iPhone; AirPods or another headset.
`ios/README.md` has the build.

### 3.0 Build — nothing here has met a compiler

- [ ] **builds** — `ios/run.sh --hello`, then the client. The files
  changed blind, in the order they are likely to complain:
  - `crates/audio/src/native/vpio.rs` — the wake latch's types
    (`latch::Ring` / `latch::Wait` in place of crossbeam's) [P2-33];
  - `crates/ios/src/host.rs`, `crates/ios/src/notices.rs` — two hooks
    removed, the chime's WAV now borrowed [P3-47];
  - `crates/audio/src/lib.rs` — the iOS-only blocks were moved, text
    for text, into `engage_call_path` [P2-55];
  - `crates/video/src/videotoolbox.rs` — two trait methods nothing
    called were removed;
  - `crates/ios/Cargo.toml` and the `ios/` scripts — the version now
    comes from the workspace [P2-61]: check the build number Xcode
    shows.

### 3.1 Voice [P2-55, IOS-SPEC D47, D51]

- [ ] **same** — connect, stay in the lobby, send the app to the
  background: it stays connected (a plain output keeps it alive), and
  there is no microphone indicator.
- [ ] **same** — join a room: log `Reopening voice streams (in a voice
  room)`, `voice-call path`. Others do not hear themselves from the
  loudspeaker.
- [ ] **same** — mute: the orange indicator goes at once and unmuting
  is instant (the unit is muted, nothing is reopened — no gap in what
  you hear).
- [ ] **same** — a phone call during a room: after it ends both
  directions come back by themselves.
- [ ] **same** — connect AirPods in a room: log `plain streams`; take
  them off: back to `voice-call path`.
- [ ] **same** — leave the room: the app still survives the background.

### 3.2 Notifications and the chime [P3-47, IOS-SPEC D50]

- [ ] **same** — a DM in the background notifies with baucord's own
  sound; tapping opens the conversation.
- [ ] **same** — a DM with the app on screen chimes, and the
  ring/silent switch silences it.

### 3.3 The screen, at 375 pt

- [ ] Everything under §2.3 but Back: dialogs close on Cancel and on a
  tap outside.
- [ ] **same** — watch a stream: picture, sound, pinch, rotate.

### 3.4 Not done yet on iOS

P2-29 (the lobby's silent output is a known App Review 2.5.4
rejection: it belongs in the App Store decision with PRD-1), and the
VideoToolbox half of P2-43.

---

## 4. When a line fails

Send back: the platform and its version (Windows build number,
Android version and phone model, iOS version), the line, and from the
log —

- for anything in voice: the `Echo cancellation:` line, both `Native
  WASAPI` lines (Windows), every `Reopening voice streams` line and the
  `audio stats:` line nearest the moment;
- for a share or a watched stream: the 5 s summary lines around it;
- for a build: the first error, with its file and line.

A **same** line that fails is a regression of this release: it worked
in 1.2.0. A **new** line that fails is the change not doing what it
was written to do. Both are worth the log.
