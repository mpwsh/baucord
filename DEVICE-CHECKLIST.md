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

> **The client asks of 2026-10-14:** pictures decoded only while on
> screen, tooltips drawn above everything, long press on the phones,
> the tray's icon and menu, a sound for mute. Lines marked
> **[10-14c]**; the `.slint` behaviour was driven headless on Linux,
> the Rust type-checked there (desktop with the tray, and the phones'
> `frontend`), nothing run on a device.

> **2026-10-16:** chat pictures no longer go through the avatar path
> (nor into its cache on disk); received pictures decode within
> limits a sender can reach; Escape and Back leave Settings keeping
> nothing, as its Close; the tab lists scroll on a short screen. Lines
> marked **[10-16]**; `app-core`'s tests ran, the `.slint` behaviour
> was driven headless at an iPhone SE's size and a phone on its side,
> the Rust type-checked on Linux; nothing run on a device.
> Later the same day, the rest of the 2026-10 review: Windows' call
> path only in a voice room and opened by its id (AEC-SPEC E16), the
> server's 20 s ping, a phone that stops watching in the background,
> the question before quitting with unsaved Settings, portrait monitors
> and the half-size DXGI path, X11 shares that read the screen only
> when it changed, the key watcher's probation once per process, the
> desktop's log in its config folder. Lines marked **[10-16b]**; the
> tests and the Windows (MinGW), Android and iOS type-checks ran on
> Linux, the quit question was driven headless; nothing run on
> Windows, macOS or a phone.
>
> **2026-10-15:** Settings and the admin panel are pages over the
> main window on every host (no more windows of their own), and the
> admin panel works on the phones. Lines marked **[10-15]**; the real
> main window was driven headless on Linux (Escape, Back, recording a
> key, the confirmation, taps) at desktop, small-window and phone
> sizes in the three languages; the Rust type-checked (desktop with
> the tray, the phones' `frontend`); nothing run on a device or a
> desktop OS.

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

- [ ] **new [10-16b]** — **Windows 10 and Windows 11, speakers,
  Automatic: connect and stay in the lobby.** The music keeps its
  volume on both. Settings → Audio's echo line reads "Takes effect
  when you join voice", and the log's `Native WASAPI` lines say
  `category=Other`: the streams are plain until a room [AEC-SPEC E16,
  P1-5].
- [ ] **new [10-16b]** — the same machines, **join a room**, then
  **watch someone's stream** there. Write down, for each Windows: is
  the music lowered on joining? while watching? The call path's
  streams now open the Communications device by its id, without the
  role (E16); if neither Windows lowers the music, the amber notice
  below (E15) can go. Leave the room: the music is back at once.
- [ ] **Windows 11, speakers, Automatic** — join a room. The music keeps its
  volume. log: the `Native WASAPI render:` line ends `do-not-duck=
  requested, own-volume=kept, Windows during calls: …`. Write down
  what the `capture:` line says for `do-not-duck=` — that answer is
  not known yet.
- [ ] **new** — **headphones, output "Default", Automatic** — the music
  keeps its volume. log: `category=Other`. (1.2.1 as first written
  would have ducked here.)
- [ ] **new** — in voice, watch a stream whose sound plays on another
  output than the voice. The stream is not lowered.
- [ ] **Windows 10, speakers, Automatic, in a room** — the music *is*
  lowered (Windows 10 cannot be asked not to; unless E16's open by id
  is enough — the line above). Then:
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

- [ ] **new** — share a busy 1080p60 screen (a code editor full of text,
  then a game) from an AMD machine and from a Mac for five minutes.
  log: the session line says `10000 kbit/s`; after any `encoded frame
  too large to send`, the floor named there is 24 or one step above,
  not a jump to 28–36, and later session lines step it down (`QP ≥ 26`
  → `QP ≥ 24` → none) rather than climbing back each 30 s. The viewer
  sees text as sharp as from an NVIDIA sharer, and no hitch every half
  minute.
- [ ] **new** — with someone else in your channel: start a share, stop
  it. Both of you hear the rising pair at the start and the falling
  pair (the same two notes, high to low) a moment after the stop. Share
  again and quit baucord mid-share, or move to the lobby: the other
  side hears the leave chime only, not cut into by the stop. Untick
  Settings → Sounds → "starts or stops sharing": neither plays.
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
- [ ] **new [10-16b]** — share a **portrait (rotated) monitor**: the
  viewer sees it upright, the right way round. log: `the output is
  rotated (…): screenshots instead` — the DXGI path does not turn
  pictures, so a rotated one goes to screenshots [P2-10].
- [ ] **new [10-16b]** — share, then **lock the screen** (Win+L) for
  half a minute and unlock: the share comes back by itself; the log
  may show the screenshot fallback, and streaming tried again every
  10 s until it holds [P2-12].
- [ ] **new [10-16b]** — **the half-size path**: share a 4K monitor at
  1080p (or a 1440p one at 720p — any quality at most half the
  monitor's size): the picture is sharp and the colours right, the
  pointer is where it is on the screen, its size right, and moves
  smoothly while nothing else moves. The GPU halves the picture now
  (`Halver`, mipmaps) instead of the CPU scaling the whole of it
  [P2-46, P3-35].
- [ ] **new [10-16b]** — share with no one watching for a minute,
  then someone starts: they get a picture at once [P1-1].
- [ ] **new [10-16b]** — **Linux on X11**: share a still desktop. log:
  `X11 (GetImage on DAMAGE)`; CPU near zero while nothing moves, a
  moving window shows smoothly, a second monitor's changes do not
  wake the first's share [P2-43].

### 1.4 Doing nothing cheaply [P2-35, P2-37, P2-39]

- [ ] **new** — connected in the lobby, nothing shared or watched:
  `powercfg /energy` (or Sysinternals `clockres`) does not list
  baucord as holding the 1 ms timer. In a room it does [P2-35].
- [ ] **new** — minimized or in the tray: GIFs do not animate (CPU near
  zero in Task Manager) and resume when the window is back [P2-39].
- [ ] **new** — the log at its default level has no flood of `wasapi`
  lines during a call [P2-37].
- [ ] **new [10-16b]** — **the release build's log** is `baucord.log`
  in the config folder, beside `settings.json` (Settings → General →
  Open config folder), not beside the exe; past 5 MB it rolls over to
  `baucord.log.1` while the app runs [P2-47].

### 1.5 The window, the chat, the dialogs

- [ ] **new** — on the connect screen, maximise the window and drag it
  down to its smallest: the card stays centred, and at the smallest
  height it scrolls with the wheel instead of cutting off its top and
  bottom.
- [ ] **new** — maximise the window, open Settings, then the admin
  panel: the line above the footer runs to the right edge and Save and
  close (Close in the panel) sits in the bottom-right corner; Reset to
  defaults' question keeps the bar full width. Settings' scroll bar,
  and the admin panel's Users and Audit lists' with enough rows to
  scroll, are at the window's edge, clear of the role pickers.
- [ ] **new** — admin panel → Roles: scroll down, press Edit on a
  custom role. The list is gone and the editor starts at the top; the
  arrow, Cancel and Escape each bring the list back, and so does Save
  role. Save a name already taken: the error shows under "Edit role".
  Close the panel mid-edit and open it again: it shows the list.
- [ ] **new** — ban yourself from a test server (no reason, then
  "spam" for 1 h) and connect: a red toast at the top reads "…from
  this server indefinitely: (no reason given)" / "…for 1 h: spam",
  spaced, and stays until the next Connect. Unplug the saved mic: a
  yellow toast says so under it, a click opens Settings, and opening
  Settings hides both. A dropped connection's toast goes away after
  5 s or on a click.

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

- [ ] **new [10-14c]** — post or receive ten pictures, then "Load
  earlier" a few times in a busy General: Task Manager's memory rises
  by a few MB a screen of pictures, not ~2 MB a picture, and settles;
  `/mem` says "N pictures: K decoded" with K a dozen or so at most.
  Scroll back up over them: each picture fades in within a moment at
  its own size, and nothing jumps.
- [ ] **new [10-14c]** — a GIF still plays on arrival and on hover,
  and goes back to its first frame after.
- [ ] **new [10-14c]** — hover the emoji and attach buttons beside the
  composer, the sidebar's bottom bar, the Invite / New channel chips
  and the chat header's buttons: each tooltip is drawn over the
  sidebar and the channel list, never under them, and stays inside
  the window at its edges. Moving along the bottom bar, the next one
  comes at once.
- [ ] **new [10-14c]** — the tray icon is the flat dog's head, legible
  at 100 % and 150 % scaling; mute (button, keybind or tray) turns it
  red, unmute back. Deafened, red too.
- [ ] **new [10-14c]** — right-click the tray icon while in a voice
  room: Mute / Deafen (they read Unmute / Undeafen when on, and act),
  then, after a separator and in the menu itself (no submenu), one
  item per other person in our room, "Mute Ana" / "Unmute Ana" —
  nobody from another room; picking one mutes them for us only (the
  roster shows the local-mute tint) and the item turns to "Unmute
  Ana". Someone joining or leaving our room appears or goes from the
  menu. In the lobby or the AFK room, no people; disconnected, only
  Show and Quit. **[10-15]**: the people were in a submenu named
  after the room.
- [ ] **new [10-14c]** — mute and unmute: one soft low note, then a
  slightly higher one; deafen the same. Settings → Sounds → "Mute and
  deafen" off silences only those.
- [ ] **new [10-15]** — open Settings (the gear, or the connect
  page's pencil): it covers the window, its sections where the
  sidebar was; no second window opens. Escape, or Save and close,
  brings the chat back as it was (its scroll, a half-typed message).
  Change the language or the text size there: the page follows at
  once.
- [ ] **new [10-15]** — Settings → Keyboard: Record, press a combo:
  it is taken; Record again and press Escape: only the recording is
  cancelled; Escape again closes Settings and keeps nothing: the old
  bind works, not the new one. Record again and press Save and close:
  the new bind works. **[10-16]**: Escape kept the new bind.
- [ ] **new [10-15]** — Settings on Audio, then close the window to
  the tray: the microphone closes (Windows' microphone icon goes);
  Show from the tray: Settings is still open and the meter moves.
- [ ] **new [10-15]** — the Admin chip: the panel covers the window,
  its tabs on the left; every tab fills. Users: each person has
  "Actions ▾" beside the role picker; it opens a menu under it
  (Disconnect…, greyed while they are offline; Kick…, Ban…; Reset
  profile…, Purge…). Ban someone from it: the confirmation asks;
  Escape closes only it, a second Escape the panel. Escape with the
  menu open closes only the menu. Invites → Create invite: the invite dialog shows over the
  panel; close it and press Escape: the panel closes (the keyboard
  came back to it).
- [ ] **new [10-15]** — Settings: change the language, the text size,
  the microphone and a keybind, then press Close (not Save and
  close): each goes back to what it was, at once, and after a restart
  too. Change one again and press Save and close: it stays. Escape
  does what Close does **[10-16]**. Pick a new picture, then Close: the old one is
  back, and nobody else saw the new one.
- [ ] **new [10-15]** — maximize the window with Settings, then the
  panel open: the content keeps to a column beside the tabs, its
  buttons right under it (Save and close under the form, a person's
  Actions and role beside their name, the panel's Close under the
  list).
- [ ] **new [10-15]** — every dropdown (Settings → General's Language,
  the admin panel's role picker and Actions) shows a chevron at its
  right, not a square; the sidebar's status reads "● Connected" with a
  round dot, and the line under it "… | ↑0.1 ↓0.3 Mb" with arrows. On
  the phone too (§2.3).
- [ ] **new [10-15]** — make the window narrower with the panel open:
  under about 830 px the rows stack (a person's actions on two lines,
  the audit as two-line entries); under 600 px Settings and the panel
  become a list and a page with an arrow back, as on the phone.
- [ ] **new [10-15]** — the main window's title bar and a stream
  viewer's (which had none) show the tray's icon, the dog on a disc,
  as do their taskbar buttons and Alt-Tab while the app runs; the exe
  in Explorer, the Start menu and a pinned shortcut keep the full
  painting. At 100 %
  and 150 % scaling. (Linux on X11: the same icon in the title bar
  and the task list.)
- [ ] **new [10-16]** — on a server that keeps history, with pictures
  in General and in a DM, restart the app: the folder `avatars/`
  beside `settings.json` is gone (`log: removed the old picture
  cache`, once), and `avatar-cache/` holds only people's profile
  pictures — a file per person, none of the chat's pictures, however
  many are scrolled through. A history page of pictures comes in
  without a stutter.
- [ ] **new [10-16]** — from another client, send as a file (not as a
  picture) a photo wider than 4096 px, under 10 MB: it stays a file
  card with Save; a 12 MP phone photo (4032 px) shows as a picture.
  An animated GIF a sender made plays as before.

- [ ] **new [10-16b]** — **quitting with unsaved Settings.** Open
  Settings, change the text size, then quit from the tray's Quit: the
  window comes up with the question — Save and quit, Quit without
  saving, Cancel. Cancel (or Escape): Settings is still open, the
  change still there. Quit again → Quit without saving: next start,
  the old size. Again → Save and quit (or Enter): next start, the new
  size. The same from `/quit`, from the connect page's Quit, and from
  installing an update with Settings changed. A window narrower than
  600 px stacks the three buttons. A build without the tray: closing
  the window asks the same and stays up until answered. Sign out of
  Windows (or shut down) with a change on the page: next start, the
  change is not there — nothing asked, nothing kept.
- [ ] **new [10-16b]** — someone whose profile picture is over 512 px
  a side (a hand-made one; the app scales its own to well under):
  their initial shows in its place, the window does not stall, and
  the log says `avatars: a picture refused: an avatar of …` [T2].

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
  in 1.2.0). **[10-16b]**: until three presses have been announced by
  Windows, it looks on a heartbeat that starts at 4 a second and
  slows to one every 4 s while nothing happens; after them, never.
- [ ] **new [10-16b]** — press push-to-talk three times, then
  disconnect and connect again (or rebind a key): log `Key watcher: …
  woken by key events, proven` — the new watcher trusts the events
  from the start — and PTT works at once. Type in another window and
  alt-tab while holding PTT: the log still has no `a key was pressed
  and no event said so` [P2-34].
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
- [ ] **new [10-16b]** — **against a 1.3.0 server or newer**: unplugged,
  connected in the lobby, screen off for 5 minutes; then someone sends
  a DM. The notification comes within seconds, and the others did not
  see you leave and come back. (The server's PING every 20 s wakes the
  phone; against a 1.2.1 server the session ends after 30 s and the
  phone reconnects when it wakes.) [ANDROID-SPEC D74, P1-6]
- [ ] **new [10-16b]** — watch a stream on its page, then press Home
  for a minute: no sound of it and, in Settings → Apps → baucord →
  Mobile data, almost nothing used meanwhile; back in the app the
  picture returns by itself within a couple of seconds [D74, P1-2].
- [ ] **new [10-16b]** — on screen, out of a room: a mention plays the
  mention chime, a DM the message chime (two different sounds); five
  DMs from one person within a few seconds sound once [P3-12, P3-15].

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
- [ ] **new** — with a saved mic missing, the connect screen's warning
  is a toast under the status bar, not on it, and the card sits below
  it. Tap Nickname: the keyboard comes up, the card can be dragged to
  reach Connect, and nothing is hidden behind the keys.
- [ ] **new** — admin panel → Roles → Edit on a custom role: the editor
  fills the page with one arrow, the top bar's; it (and Back) brings
  the roles back, a second press leaves the tab.
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

- [ ] **new [10-14c]** — hold a finger on a person in the roster:
  their card shows; lift it: the card goes and no menu opens. A quick
  tap still opens their menu.
- [ ] **new [10-14c]** — hold a finger on a voice channel: its menu
  opens and lifting does not join it. A tap joins.
- [ ] **new [10-14c]** — tap a picture: the gallery. Hold: its menu,
  and lifting does not open the gallery. Hold an avatar in chat: the
  author's card, no menu.
- [ ] **new [10-14c]** — no tooltip ever shows on the phone.
- [ ] **new [10-15]** — as an admin, tap the Admin chip: a list of
  the panel's nine tabs. Tap one: its page, filled; Back: the list;
  Back: the chat. Every tab reads at 360 dp without anything cut
  that matters (a long name, a ban's reason, a reported message in
  full), and the row buttons are easy to hit.
- [ ] **new [10-15]** — Users: each person's name across the width,
  "Actions ▾" and the role picker under it. Tap Actions: the menu
  opens under it; Ban…: the confirmation's reason field stays above
  the keyboard; Back closes only the confirmation, the next Back the
  tab. Change someone's role with the picker.
- [ ] **new [10-15]** — Settings: the footer is Close and Save and
  close; Reset to defaults is the list's last row, in red, and turns
  the footer into the question. Change something, then Close: it goes
  back.
- [ ] **new [10-16]** — Settings: change the text size, then Back
  from the list: it goes back, as with Close; Back on a section still
  goes to the list first. Tap Reset to defaults, then Back: only the
  question goes. With the display size at its largest (or a short
  phone), the list scrolls with a finger and Close and Save and close
  stay on screen; on its side, the tab column scrolls too. The same in
  the admin panel's list.
- [ ] **new [10-15]** — no squares where a symbol should be: the
  dropdowns' chevrons, the dot before "Connected", the arrows by the
  transfer rates; `/users` says "now" for whoever is online and
  `/reports` "handled" after a dealt-with report.
- [ ] **new [10-15]** — Roles → New role: the badge's glyphs and
  colours wrap into rows; pick one of each, Save; the role shows with
  its badge.

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
- [ ] **new [10-16b]** — play music, then connect: the music keeps
  playing in the lobby, in front and in the background. Join a room:
  the music stops (a call); leave it: the music app may resume, and
  the lobby keeps the app alive again [IOS-SPEC D64, P1-7].
- [ ] **new [10-16b]** — in the lobby in the background for ten
  minutes: the battery graph shows the app barely there (the lobby's
  output runs long buffers now) [P1-7].
- [ ] **new [10-16b]** — watch a stream, go to the home screen for a
  minute: its sound stops; back in the app the picture returns by
  itself [D64, P1-2].

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
- [ ] **new [10-16b]** — a mention plays its own chime, not the DM's;
  a burst of DMs from one person sounds once [P3-12, P3-15].

### 3.3 The screen, at 375 pt

- [ ] Everything under §2.3 but Back: dialogs close on Cancel and on a
  tap outside.
- [ ] **same** — watch a stream: picture, sound, pinch, rotate.
- [ ] **new [10-15]** — the admin panel as in §2.3, the top bar's
  arrow standing in for Back.
- [ ] **new [10-16]** — on an iPhone SE (667 pt tall): the Settings
  list scrolls with a finger, Close and Save and close stay on screen,
  and Reset to defaults is at the end of the scroll. The top bar's
  arrow from the list keeps nothing, as Close.

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

### 3.5 TestFlight **[10-23]** [IOS-SPEC D67]

Written in the Linux sandbox: the workflow was linted and its shell run
against stand-ins, the Swift helper never met a compiler.

- [ ] **the icon** — `ios/run.sh` (any build): the build log shows
  `app icon: … (1024x1024, opaque)`; on the home screen, the dog on its
  disc with corners in the app's dark background, not black. If the
  step stops in `flatten_icon.swift`, paste the log.
- [ ] **the manifest in the bundle** — Xcode → the target → Build
  Phases → Copy Bundle Resources lists `PrivacyInfo.xcprivacy`; the
  built `baucord.app` has it at its root.
- [ ] **CI as before** — ios-release by hand, *testflight* unticked:
  green; "Check the privacy manifest against the binary" lists
  FileTimestamp and SystemBootTime as used, with no error or warning.
  An error there names the API to declare.
- [ ] **the first upload** — the secrets of ios/README §3c, then a run
  with *testflight* ticked: the step ends with *uploaded 1.3.0 (N)*;
  App Store Connect → TestFlight shows the build processing, then
  *Missing Compliance*, answered once (2026-10-24, run #11: uploaded,
  processed); it installs from the TestFlight app on a phone that is
  not in the Ad Hoc profile.
- [ ] **no compliance question** — the next build after IOS-SPEC D68
  goes from *Processing* straight to *Ready to Submit*, without
  *Missing Compliance*.
- [ ] **no rejection email** — nothing from App Store Connect about
  ITMS-91053 (an undeclared API) or the icon in the hour after.

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

- [ ] **new [10-14c]** — the menu-bar icon reads at its size in light
  and dark menu bars, and turns red while muted or deafened; its menu
  as on Windows (§1.5).
- [ ] **new [10-14c]** — start, send ten pictures, scroll up and down
  over them, "Load earlier" twice: Activity Monitor's memory stays
  within ~20–30 MB of where it started instead of doubling; `/mem`
  says how many pictures are decoded.

### 4.3 Settings and the admin panel as pages **[10-15]**

- [ ] **new [10-15]** — the five §1.5 lines marked **[10-15]** on the
  Mac; recording a keybind there with Accessibility granted (§4.1).

---

### 4.4 The Mac app **[10-24]** [macos/README.md]

Built by a script tried in the Linux sandbox against stand-ins for
`lipo`, `codesign`, `ditto`, `hdiutil`, `notarytool` and `stapler`; the
bundle code of the updater unit-tested on Linux. Nothing ran on a Mac.

- [ ] **the disk image** — the `macos-dmg-<commit>` artifact of a push's
  *Build and Release* run: it opens to baucord and an Applications
  shortcut; dragging installs it. Without the Developer ID secrets the
  first open needs System Settings → Privacy & Security → Open Anyway.
- [ ] **no Terminal** — opening baucord.app shows only its window; the
  Dock shows the dog icon; About/⌘Q name it baucord.
- [ ] **both Macs** — the same app runs on Apple silicon and on Intel
  (`lipo -archs /Applications/baucord.app/Contents/MacOS/baucord` lists
  both).
- [ ] **permissions name baucord** — joining a voice room asks for the
  microphone *as baucord*, with the Info.plist's sentence; screen
  sharing and keybinds ask for theirs as baucord. A Developer ID build
  keeps them across an update.
- [ ] **notarized** — with the secrets, a release's disk image opens
  with no warning at all; `spctl -a -vv /Applications/baucord.app` says
  *accepted, source=Notarized Developer ID*.
- [ ] **the update swaps the bundle** — an installed 1.x.y app offered a
  newer release: it downloads, restarts as the new version from the
  same place, `.baucord-update` beside it is gone, and the permissions
  above were not asked again (Developer ID).
- [ ] **a zip install still updates** — the bare binary from the old
  `-macos-arm64.zip` updates from the same kind of zip, as before.
- [ ] **settings carry over** — the app opened after the zip install
  shows the same servers and nickname.

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

**[10-14]** — the second review's server and transport fixes, run in
the simulator only:

- [ ] a file between 5 and 50 MB posts in General on the official
  server (it was refused as "today's upload allowance"); a second big
  one within the same minute says "the server refused the upload"
  [SRV-1].
- [ ] a picture posted while 10+ people are online reaches all of
  them, and the server's memory (`docker stats`) rises by about one
  copy of it, not one per person [PT-2].
- [ ] voice in two rooms at once on a busy evening: nothing changes to
  the ear; `Voice — RX/FWD` stats lines as before [SRV-7].
- [ ] `docker pause baucord` for a minute, then `unpause`: the log
  says `wall clock moved forward … ms` within a second, and a message
  sent right after carries the right time [SRV-13].
- [ ] `server user mute <name>` from the host's shell: the person's
  app shows them muted within ~3 s; `server restriction lift <id>`
  unmutes it [SRV-12].

**[10-16b]** — the 2026-10 review's server side:

- [ ] back up first (`server backup`), then start the new server on
  the old database: log `schema 7`; the old server refuses the
  upgraded database (going back is restoring that backup).
- [ ] a client killed hard (pull its network cable) leaves the roster
  within about 50 s, not 30: the server's 20 s ping restarts its idle
  timer [D74].
- [ ] a 30 MB file, then pictures in the same minute: the pictures
  still go through (pictures and files have a bucket each) [P1-11].
- [ ] a `server.yaml` with an `authentication:` section, or a misspelt
  section: the log names it at start (`… is not a section this server
  reads; ignored`), and the server runs.

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
- [ ] **[10-16b]** `pr-check` has two new jobs, `chat-scroll-sim` ("0
  of the checks failed") and `windows` (clippy for
  `x86_64-pc-windows-gnu`), and its `fmt` uses `nightly-2026-10-10`.
  A push to `main` that only touches docs starts no "Build and
  Release"; two quick pushes cancel the first one's run.
- [ ] **[10-16b]** The next release: the image is built on the
  bookworm builder and `Start the image once` passes before the push;
  `latest` moves only if the tag is the newest stable one; the
  `version` file is uploaded only then; the APK and AAB artifacts
  expire after 7 days, and a dry run's APK is unsigned.

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
