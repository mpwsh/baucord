# baucord — what to check on real machines (1.2.1)

> **Status (2026-10-12):** written after the review work of 1.2.1
> (`PRODUCTION-PLAN.md` §2a, §7). All of that work was done in a
> sandbox that is Linux on ARM: the Windows- and Android-only Rust was
> type-checked there, the iOS code was not even compiled, and nothing
> platform-specific was *run*. This is the list of what has to be seen
> on the real thing before 1.2.1 ships. Tick a line when it holds; a
> line that fails goes into `PRODUCTION-PLAN.md` with what §6 asks for.
>
> **Since then (the review's seventh batch, same day):** the iOS build
> compiles (the user, before that batch). That batch changed platform code
> again; its lines are marked **[7]** and each platform's §x.0 says
> what was type-checked in the sandbox and what was not.
>
> **After the first iPhone look (same day):** Settings → Audio opens
> the microphone for its meter without a connection, and the meters
> are eased. Lines marked **[8]**; the app's own code for it was
> type-checked on Linux only (it is the same on every platform), the
> engine's rule for it is unit-tested.

Each line is *do this → expect that*. `log:` is a line to find in
`baucord.log` (desktop: Settings → Debug shows it and copies it;
phones: `adb logcat -s baucord`, `ios/run.sh`'s stream). The tag in
brackets is the review item or decision the line is there for.

Three kinds of line:

- **builds** — code changed without a compiler for that platform;
- **new** — behaviour that is meant to be different from 1.2.0;
- **same** — a refactor: must behave exactly as before.

The "changed blind" lists are `git diff v1.2.0..HEAD` for the files
only that platform compiles. macOS has §4 since the seventh batch
changed it; the Linux desktop is where the work was done.

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
- [ ] **builds [7]** — the same build again. New since: the `keywatch`
  crate (its Windows half was type-checked and linted for
  `x86_64-pc-windows-gnu`; `frontend/src/hotkeys.rs`, which now only
  calls it, was not), `audio` and `video` on the 2024 edition
  (type-checked for Windows; two `extern` blocks in
  `audio/src/platform.rs` became `unsafe extern`), and
  `frontend/src/resources.rs` (the `Sampler` around `sysinfo`).

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

- [ ] **new [8]** — not connected, Settings → Audio: the capture bar
  follows your voice, with push-to-talk chosen too, and the
  echo-cancellation line says what the streams got. Another tab, or
  Settings closed: log `Voice stopped`. The streams it opens are the
  ones a room opens, so on Windows 10 other apps may get quieter while
  the tab shows (§1.2).
- [ ] **new [8]** — the bars are smooth, and with a silent microphone
  Task Manager shows the client idle with the Audio tab open (a bar
  that did not move is not drawn).

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
- [ ] **new** — **viewers see your mouse pointer** [P2-25], where it
  is on your screen, and it moves smoothly while nothing else on the
  screen does. Look at: the arrow; the text caret over a text field
  (thin, and the right colour on dark and on light); the hand over a
  link; a resize arrow at a window's edge; a large or coloured pointer
  from Settings → Accessibility → Mouse pointer. None is a square
  block, none has a black or white box around it.
- [ ] **new** — stop the mouse and wait ten seconds: the pointer stays
  where it stopped (it used to be at risk of vanishing two seconds
  later). Move it off to another monitor: it leaves the share. Bring
  it back: it is back. In a game that hides the pointer there is none.
- [ ] **new** — the pointer at each edge and corner of the shared
  screen: cut at the edge, no crash, no stripe on the opposite side.
- [ ] **new** — a still screen for a minute with the pointer parked:
  the sharer's stats line shows almost no bitrate, as before [P2-41].
- [ ] **new** — `"share": { …, "pointer": false }` in `settings.json`
  and a restart: no pointer in the share, everything else the same.
- [ ] **Known** — started on a screen where nothing at all is moving,
  the pointer shows from the first change on the screen (a clock
  ticking, a hover), not before. On an HDR desktop (the screenshot
  fallback, said in the log) there is still no pointer.

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
  there, and so it does for a ban (decided 2026-10-12) [P2-17].
- [ ] **new** — in the composer, select text, right-click: Cut and Copy
  work; with nothing selected they are greyed [P2-16].
- [ ] **new** — kick, mute, move someone from the menu: no "Done." in
  the chat. Type `/mute name` (or any command that only succeeds): the
  line is in General's Activity view, `/mute name: done`, and not in
  the chat. A command that is refused still says why in the chat.
- [ ] **new** — drop two files on the chat at once (not pictures):
  both arrive. The second card waits at 0 % until the first is done;
  cancelling the first lets the second start, and cancelling the
  second while it waits sends nothing. The first used to fail with a
  refusal [P1-17].
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
- [ ] **same** — connect to a server whose identity this machine had
  already pinned: no "identity changed" warning. Send a picture and a
  file both ways: they arrive (their hashes are checked). The one
  SHA-256 in the tree is `ring`'s now [P3-50].
- [ ] **same** — drop a file from Explorer onto the window (the overlay
  shows, the file attaches); bring the window back from the tray (it
  repaints, no blank surface). Both come through Slint's winit
  feature now, not its internal crate [P3-50].
- [ ] **same** — edit and delete a message, create, edit and delete a
  channel, send a DM, block and unblock someone, report a message,
  change your name and colour. Each of these is sent through one
  `NetworkControl::Send` now [P2-53].
- [ ] **same** — the microphone sounds as before with noise
  suppression on and off, and the gate still closes on silence
  [P2-54].
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
- [ ] **new [7]** — on a server with more than about 140 accounts, the
  admin panel's Users tab lists all of them (it stopped at what one
  message holds), and so do Invites, Restrictions, Files, Reports and
  an Audit page. `/users` in the chat prints 150 and a line saying how
  many more the panel has. A 1.2.0 client on the same server still
  gets the first 140 or so, not an empty tab [P0-1].
- [ ] **new [7]** — download a large file (50 MB or more) from a file
  card: the window keeps responding while it is written, then the
  toast says where it went and the card offers Open [P2-27].

### 1.6 Keybinds without polling [P1-3 step 2] **[7]**

The watcher sleeps until Windows reports a key (Raw Input) and reads
the bound keys then. `cargo run -p keywatch --example watch` prints
the same edges without the app (F7, Ctrl+F8, hold V).

- [ ] **new** — connect. log: `Watching N keybind(s), woken by key
  events` and `Key watcher: N binding(s), woken by key events`. (If it
  says `on a clock`, or `Raw Input not available`, it fell back to
  reading every 12 ms: works as 1.2.0 did — send the log.)
- [ ] **new** — the default keys (F7, F8, F9) toggle while another
  program has the focus, a full-screen game included; push-to-talk
  opens on the press and closes on the release, also after holding it
  for a minute; a chord (Ctrl+Shift+M) fires only with its modifiers.
- [ ] **new** — the log does **not** get `a key was pressed and no
  event said so`. That line is the safety net: the watcher found a
  press its events did not announce and went back to the clock. If it
  appears, keybinds still work — send the log and say which key.
- [ ] **same** — a bound key still types in other programs (bind `N`,
  type in Notepad).
- [ ] **new** — Task Manager, connected and idle with the keyboard at
  rest: the `key-watch` thread does nothing (it woke 83 times a second
  in 1.2.0, 4 times a second for the first three presses of a session
  now, then never).
- [ ] *Find out* — with a window that runs as administrator in front
  (Task Manager, an elevated game) and baucord not elevated: do the
  keys reach baucord? In 1.2.0? (Windows hides input to elevated
  windows from other programs; this should be no different, and needs
  saying in the docs if it bites.)
- [ ] **same** — disconnect and connect again several times: keybinds
  work each time (the watcher hands the process's Raw Input
  registration back to the window toolkit when it stops).

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
- [ ] **builds [7]** — the Gradle build again. Closer to checked than
  the list above was: `java.rs`, `host.rs` and `logging.rs` were
  type-checked on Linux in a scratch crate against the real `jni`,
  `android_logger` and `app-core` (and a copy of `frontend`'s hook
  trait), and the three Java files compile with `javac` against API
  34's `android.jar`. Gradle, the Kotlin build script, `lib.rs`'s one
  changed line and the manifest of dependencies did not run. In the
  order they are likely to complain:
  - `crates/android/Cargo.toml` — `app-core` is a dependency now, in
    the hello build too;
  - `crates/android/src/logging.rs` — the tee writes through
    `app_core::logfile::FileSink` [P3-44, P3-45];
  - `crates/android/src/host.rs` — the pick slot is
    `app_core::files::PickSlot` [P3-44];
  - `crates/android/src/lib.rs` — the panic hook is
    `app_core::logfile::install_panic_hook` [P3-44];
  - `crates/android/src/java.rs` — `save_download` calls
    `beginDownload` / `endDownload` and writes to a file descriptor
    itself [P2-27];
  - `BaucordActivity.java` — those two methods in place of
    `saveDownload` [P2-27]; `VoiceService.java` — `holdWakeLock`
    [P2-28];
  - `android/app/build.gradle.kts`, `gradle.properties` — the ABI
    default [P3-46];
  - `frontend/src/resources.rs` — the phone `Sampler`
    (`app_core::usage`, which *was* type-checked for
    `aarch64-linux-android` and whose `/proc` half is tested on Linux)
    [P3-50].

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

### 2.1a Settings → Audio without a room **[8]** [ANDROID-SPEC D70]

- [ ] **new** — not connected, Settings → Audio: the green microphone
  dot comes on and the capture bar follows your voice, gliding. log:
  `settings: audio tab on screen`, then `microphone: no engine yet,
  starting one for this device alone`. Test microphone plays your
  voice back half a second late. Leaving the tab logs the summary line of
  §3.1a.
- [ ] **new** — back to the section list, close Settings, or press
  Home from the Audio tab: the dot goes after Android's few seconds.
- [ ] **new** — connected, in the lobby, Settings → Audio: the bar
  moves, nobody sees you speaking, and leaving the tab closes the
  microphone again.

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
- [ ] **new** — tap a picture in the chat: it fills the screen's width
  (a tall one the height), under a title row with Save and Close and
  no Copy. With several pictures in the conversation the arrows sit
  on the picture's left and right edges, only where there is a next
  or previous one. A tap on the black beside the picture, or Back,
  closes it. Nothing is under the status bar or the notch.
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

### 2.4 The seventh batch, written without a phone **[7]**

- [ ] **new** — **the one to do first.** In a room, screen off, phone
  unplugged, for 15 minutes: voice keeps flowing both ways the whole
  time. The app's own wake lock now lasts 10 minutes from the join
  (it had no limit); after that the audio streams' wake locks are
  what keeps the phone awake. `adb shell dumpsys power | grep -iE
  "baucord|AudioMix|AudioIn"`: `baucord:voice` for the first 10
  minutes, `AudioMix` / `AudioIn` throughout. If voice stops after 10
  minutes, `WAKE_LOCK_MS` in `VoiceService.java` is the line to raise
  and the plan gets an entry [P2-28].
- [ ] **new** — in a room for more than 10 minutes, screen off, then
  cut the network for 20 seconds: the app reconnects by itself (the
  wake lock is taken again when a retry starts) [P2-28].
- [ ] **new** — save a received file of 50 MB or more: the app stays
  responsive, the file is in Downloads under the name the toast says,
  and Open on the card opens it. A second save of the same file gets
  a numbered name. Out of storage, or the save cancelled by killing
  the app: no empty file is left in Downloads [P2-27].
- [ ] **new** — `baucord.log` passes 5 MB while the app runs (debug
  level in a room gets there in an hour or two): `baucord.log.1`
  appears and `baucord.log` starts over; Settings → Debug shows the
  end of the log across both, Clear empties both [P3-45].
- [ ] **same** — attach a file, set an avatar, import the identity
  key: each picker works, and a second Attach while a large file is
  still being read says it is busy [P3-44].
- [ ] **same** — the resource line (Settings → General, if shown on
  the phone) and `/mem` show a CPU percentage and a memory figure
  that move; memory is what `adb shell dumpsys meminfo dev.baucord`
  calls RSS, about [P3-50].
- [ ] **new** — `./gradlew assembleRelease` with no `-PcargoTargets`
  builds arm64-v8a only; `assembleDebug` builds both ABIs [P3-46].

---

## 3. iOS

An older and a current iPhone; AirPods or another headset.
`ios/README.md` has the build.

### 3.0 Build — nothing here had met a compiler

The list below compiled on 2026-10-12 (the user). The one
after it is the seventh batch's.

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
- [ ] **builds [7]** — `ios/run.sh --hello`, then the client, again:
  - `crates/audio` is on the 2024 edition. Its iOS-only code
    (`native/vpio.rs`, the iOS blocks of `lib.rs`, `input.rs`,
    `output.rs`, `platform.rs`) was read for what that edition
    rejects, not compiled; two nested `if`s in `vpio.rs`
    (`Mailbox::current`) became `if … && let …` [P3-49];
  - `crates/ios/Cargo.toml` — `app-core` is a dependency now, in the
    hello build too;
  - `crates/ios/src/logging.rs`, `main.rs`, `host.rs` — the log
    writer, the panic hook, the pick slot and the safe file names come
    from `app_core` [P3-44, P3-45] (`logging.rs` is plain Rust and was
    type-checked on Linux; the other two were not);
  - `frontend/src/resources.rs` — the phone `Sampler` [P3-50];
  - `crates/video/src/videotoolbox.rs` and `app_core::usage` did meet
    the compiler this time (type-checked for `aarch64-apple-ios`).

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

### 3.1a Settings → Audio without a room **[8]** [IOS-SPEC D60]

- [ ] **new** — **not connected**, Settings → Audio: the orange
  indicator comes on (the first time, after the system's microphone
  question) and the capture bar follows your voice. log: `settings:
  audio tab on screen`, `Voice started for client None`.
- [ ] **new** — the bars glide: up within a tenth of a second, down
  over about half a second, no steps and no jumping with each
  syllable. Back on the section list, log: `settings: audio tab on
  screen T s: meters read N times and drawn M; …` — N should be about
  20 × T. Far fewer means the timer is not reaching its pace on the
  phone, and that line is what to send back.
- [ ] **new** — back to the section list, or close Settings: the
  indicator goes. Send the app to the background from the Audio tab:
  it goes too, and the app is suspended as any unconnected app is.
- [ ] **new** — Test microphone there: say a word and it comes back
  half a second later, clean — not hollow, not about to howl; the hint
  says it is late. (A second at first; if half brings the hollow sound
  back, `audio::LOOPBACK_DELAY` is the number.) With push-to-talk chosen as well.
- [ ] **new** — the tab alone, and how long it takes: not connected,
  open Settings → Audio, **do not touch Test**, talk for five seconds,
  go back to the section list, open Debug. One line answers it:
  `settings: audio tab on screen 5.3 s: meters read 104 times and
  drawn 61; an engine behind them 0.4 s in; microphone with no test
  running first heard 0.9 s in, peak 0.31; during a test, peak 0.00`.
  - `an engine behind them never`: nothing started; the `microphone:`
    line above it says what the app decided.
  - an engine, and `first heard never, peak 0.00`: the engine ran and
    its microphone gave silence — the unit, the permission, or the
    session, not the app's bookkeeping.
  - `first heard` a second or more in: it works, late. That is the
    voice unit starting, and a bar that looks dead until then.
  - The `microphone:` lines name the engine: `no engine yet, starting
    one for this device alone`, `the engine started for this device
    alone carries on` (a test pressed with the tab open, or the tab
    left with a test running), `the session's engine has it`
    (connected), `nothing needs it, stopping the engine`.
- [ ] **new** — connected, in the lobby, Settings → Audio: indicator
  and meter as above; nobody sees you speaking; leaving the tab closes
  the microphone again. Muted, the bar stays empty, as in a room.
- [ ] **same** — in a room, Settings → Audio: nothing about the call
  changes; the others still hear you.

### 3.2 Notifications and the chime [P3-47, IOS-SPEC D50]

- [ ] **same** — a DM in the background notifies with baucord's own
  sound; tapping opens the conversation.
- [ ] **same** — a DM with the app on screen chimes, and the
  ring/silent switch silences it.

### 3.3 The screen, at 375 pt

- [ ] Everything under §2.3 but Back: dialogs close on Cancel and on a
  tap outside.
- [ ] **same** — watch a stream: picture, sound, pinch, rotate.

### 3.4 The seventh batch **[7]**

- [ ] **new** — **watch a screen share: the colours are right.** Blue
  and red swapped (skin blue, a red button blue) means the picture's
  BGRA was shown as RGBA: `frontend/src/present.rs`, `fill`. The
  decoder now copies the picture out as it is and the swap happens as
  the display's buffer is written — one pass over the pixels and no
  cleared buffer per frame, where there were two and one [P2-43].
- [ ] **new** — the same stream for a few minutes on the older phone:
  log's 5 s summary shows no more late or dropped pictures than
  before, and the phone is no warmer.
- [ ] **same** — attach a photo, attach a file from Files, pick an
  avatar; cancel each picker by swiping it down and attach again (the
  slot must be free); a file whose name has a `/` or `:` saves under
  a safe name [P3-44].
- [ ] **new** — `baucord.log` in Files passes 5 MB while the app runs:
  `baucord.log.1` appears beside it [P3-45].
- [ ] **new** — the resource line and `/mem`: memory is now the
  footprint Xcode's gauge shows, not the larger resident size [P3-50].

Not done on iOS, and not code: P2-29 — the lobby's silent output is a
known App Review 2.5.4 rejection. It is on the App Store decision list
now (`PRODUCTION-PLAN.md` §6).

---

## 4. macOS **[7]**

Not covered before; the seventh batch changed it. A Mac with Apple
silicon, and an Intel one if there is one.

### 4.0 Build

- [ ] **builds** — the desktop builds. Type-checked in the sandbox for
  `aarch64-apple-darwin`: `keywatch` (its macOS half declares Core
  Graphics, Core Foundation and Accessibility calls by hand — a wrong
  signature links and then misbehaves, so §4.1 matters) and
  `app_core::usage`. Not compiled: `frontend` (`hotkeys.rs`,
  `resources.rs`), `audio` on the 2024 edition, `video/src/codec.rs`
  for macOS.

### 4.1 Keybinds [P1-3 step 2, P2-30]

- [ ] **new** — a fresh install (or after `tccutil reset Accessibility
  dev.baucord`): launching, connecting and rebinding a key put up
  **no** system prompt. Settings → Keyboard shows a note that bound
  keys are taken from other apps until Accessibility is granted, and a
  button.
- [ ] **new** — the button puts up the system's Accessibility prompt
  (and lists baucord in System Settings → Privacy & Security →
  Accessibility). Grant it: within a couple of seconds the note is
  gone, and — while connected — log: `Watching N keybind(s), woken by
  key events`.
- [ ] **new** — with the permission: F7/F8/F9 toggle from another app,
  push-to-talk follows a held key, a bound letter still types in the
  other app. Without it: the keys work and do not type elsewhere (as
  in 1.2.0).
- [ ] **new** — the log does **not** get `a key was pressed and no
  event said so` (see §1.6), nor `no event tap`. If macOS asks for
  *Input Monitoring* as well, say so: the code checks that it may
  listen before it creates the tap, to avoid a second prompt.
- [ ] **new** — Activity Monitor, connected, keyboard at rest: the
  `key-watch` thread shows no wake-ups.

### 4.2 Sharing a screen [P2-42]

- [ ] **new** — share a screen. log: `Using FFmpeg encoder:
  h264_videotoolbox` (it was `libx264`), and baucord's CPU in Activity
  Monitor is well below what 1.2.0 used for the same share.
- [ ] **new** — a viewer sees a sharp picture; joining mid-share shows
  a picture within about a second (a keyframe with its parameter
  sets); scrolling text does not smear for longer than before. The
  sharer's 5 s summary has no `frame too large` drops.
- [ ] **same** — on a Mac where the hardware encoder will not open
  (a VM): log `Encoder h264_videotoolbox failed to open, trying next`,
  then `libx264`, and the share works.

---

## 5. A server host

Not a client platform, but changed blind all the same.

- [ ] **new** — `scripts/setup.sh` on a fresh host and again on an
  existing one: it runs through, and `/etc/sysctl.d/
  99-baucord-udp.conf` no longer sets `net.core.rmem_default` /
  `wmem_default` (the server sizes its own socket). log at start:
  `UDP socket on …`; `scripts/monitor-net.sh` shows no new `RCVBUF` or
  `SNDBUF` notes under a share with several viewers [P3-52].
- [ ] **new [7]** — the same run says `cloud-init network hotplug
  turned off for this host (…)` on a cloud image, and nothing about
  cloud-init on a host without one. `KEEP_CLOUD_INIT_HOTPLUG=1 sudo
  scripts/setup.sh` removes the drop-in an earlier run wrote and says
  so [P3-52].

- [ ] **new** — two people with the same role that may ban: one bans
  the other, who is removed at once (and cannot come back). They used
  to stay in the room until they next reconnected.
- [ ] **new** — a role with "restrict" and without "ban" can still
  mute and text-restrict from the menu, as before.

### 5.1 The workflows **[7]**

- [ ] The next push: `pr-check` is green. It installs neither
  `libopus-dev` nor `libdbus-1-dev` any more, which is the proof the
  release build's shorter package list rests on [P3-48].
- [ ] The next release: the `tag` job runs first and passes; the Linux
  build finds what it needs without `libopus-dev`, the Xinerama, Xft,
  Xrender, pango and cairo headers and `libdbus-1-dev`; the last two
  steps upload the zips and then the `version` file, in that order
  [P2-60, P3-48].
- [ ] A push to `main` that touches neither `web/` nor `Cargo.lock`:
  "Build and Release" runs (its caches) and "Deploy Website" does not
  follow it. After a published release it does.
- [ ] The Android release: `Install cargo-ndk` takes seconds
  (`cargo-ndk 4.1.2`, prebuilt) and the Gradle build finds it [P3-48].

---

## 6. When a line fails

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
