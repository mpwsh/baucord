# baucord — session guide

The working manual. **Read `START-HERE.md` first**: it says how a
session runs, which document covers what, and in which order to read
them. This file is the detail: how the assistant's sandbox is set up and
what it can check (§1), where things are (§2), what shipped (§3), and
the rules that were learned the hard way (§4). What is *left* is
`PRODUCTION-PLAN.md`, not here.

## 1. Setting up the assistant's sandbox

The sandbox (as of 2026-10-07) has `cargo` (stable, 1.97) and reaches
crates.io, the Ubuntu archive, GitHub and raw.githubusercontent.com — but
not static.rust-lang.org, dl.google.com or Maven Central. So the
pinned toolchain cannot be installed, nor the Android SDK/NDK, nor AGP.
Check with `which cargo` and `df -h /` first.

- **Unpack the upload** (the repo tarball START-HERE §2 describes) into
  `/home/claude/work`; if it has a top folder, move its contents up.
  Delete macOS litter: `find . -name '._*' -delete; find . -name
  .DS_Store -delete`. Right after, `touch` a marker file outside the
  tree: every file newer than it is a change to hand back.
- **The toolchain:** the repo pins Rust 1.95 (`rust-toolchain.toml`),
  which rustup cannot download here — every cargo command runs with
  `RUSTUP_TOOLCHAIN=stable` (keep the file: the user's machines need
  it), and `CARGO_INCREMENTAL=0` to keep `target/` small.
- **Disk is the scarce thing**: `target/` reaches 15+ GB and a few GB are
  free. Before a big build: `df -h /`; free space by deleting
  `target/debug/incremental`, scratch targets and old renders.
- **System libraries** for `client`, `frontend`, `desktop` and the
  media crates on the host (`apt-get` reaches the Ubuntu archive; the
  Docker repo 403s, ignore it): `apt-get update && apt-get install -y
  libasound2-dev libavcodec-dev libavformat-dev libavutil-dev
  libswscale-dev libavfilter-dev libswresample-dev libavdevice-dev clang
  libclang-dev libwayland-dev libxcb1-dev libxcb-randr0-dev
  libxcb-shm0-dev libxrandr-dev libdbus-1-dev libpipewire-0.3-dev
  libspa-0.2-dev libegl-dev libgbm-dev libxkbcommon-dev libfontconfig-dev
  libxi-dev libx11-xcb-dev libxcursor-dev libssl-dev libudev-dev`
  (Ubuntu's FFmpeg 6.1 builds `ffmpeg-next` 8). If the upload lacks
  `assets/`, put a placeholder `assets/icons/icon.png` and short
  `assets/sounds/{join,leave}.wav` in place (`ui` and `audio` embed them)
  and never hand those back.
- **Any dependency's real API** is a download away:
  `https://static.crates.io/crates/<name>/<name>-<version>.crate`
  (`crates.io/api/v1/crates/<name>` lists versions; send a
  `User-Agent`). Read the source before writing against it — done for
  `interprocess`, `quinn-proto`, `quinn-udp`, `jni`, the `objc2`
  framework crates.

### What can be checked here, per platform

- **Host (Linux):** `cargo clippy -p audio -p client -p frontend` (the
  workspace denies `clippy::all`; compare the pedantic warnings with a
  run before the change — new ones are yours), `cargo check -p frontend
  --no-default-features` (the phones' build of it), and the tests:
  `cargo test -p protocol -p transport -p server` (86, 45 and 51 + 88
  e2e on 2026-10-09, all green), plus `-p audio`, `-p client`,
  `-p app-core`, `-p i18n` as touched.
- **Android, Rust:** `RUSTC_BOOTSTRAP=1 cargo clippy -p audio --target
  aarch64-linux-android -Zbuild-std=std,panic_abort` checks the AAudio
  backend. The `android` crate itself cannot be checked (Slint's build
  script wants an SDK): type-check `crates/android/src/java.rs` and
  `host.rs` in a scratch crate that `#[path]`-includes them, with
  `jni = "=0.22.4"`, `ndk-context` and a stub `frontend` crate holding
  `notice.rs` and a copy of the `Hooks` trait (2026-10-07). The `ndk`
  crate's `test` feature also compiles its bindings off Android (§4).
- **Android, Java:** compile against a real `android.jar` — API 33's is
  at `https://raw.githubusercontent.com/Sable/android-platforms/master/android-33/android.jar`:
  `javac --release 11 -cp android-33.jar -d out R.java
  android/app/src/main/java/dev/baucord/*.java`, with a stub `R.java`
  declaring the resources the code names. That catches wrong method
  names and signatures; Gradle (AGP from Maven) cannot run here, and
  build-script Kotlin is checked in a scratch Gradle project with
  `java-base` when a Gradle install is available.
- **iOS:** type-checked with no Mac (IOS-SPEC.md §14 D15): fetch Rust's
  `library/` at the exact `rustc -vV` commit-hash (a sparse
  `git fetch --depth 1` of rust-lang/rust, plus the `library/backtrace`
  submodule from rust-lang/backtrace-rs at the commit `git ls-tree`
  names), link it as `$(rustc --print sysroot)/lib/rustlib/src/rust/library`,
  then `RUSTC_BOOTSTRAP=1 cargo clippy -p ios --target aarch64-apple-ios
  -Zbuild-std=std,panic_abort` in a scratch copy of the workspace whose
  `.cargo/config.toml` overrides ring's build script
  (`[target.aarch64-apple-ios.ring_core_0_17_14_] rustc-link-lib =
  ["static=ring_core_0_17_14_"]`) and whose root `Cargo.toml`
  `[patch]`es `opusic-sys` to a copy whose `build.rs` only prints its
  link line — neither compiles C for iOS without Apple's SDK. Mirror
  `crates/` into it before each check (a small `sync.sh`). Skia's
  prebuilt downloads. A bash 3.2 for testing macOS scripts builds from
  github.com/apple-oss-distributions/bash (`bash-3.2/`, with a
  `xlocale.h` shim, `-DEBADEXEC=ENOEXEC -DEBADARCH=ENOEXEC` and a
  pass-through `fmtcheck`).
- **Windows-only code** (`wasapi.rs`, DXGI, the socket waits, qWAVE) is
  type-checked against the real target since 2026-10-12, where the
  sandbox can install packages: `rustup target add
  x86_64-pc-windows-gnu`, `apt-get install gcc-mingw-w64-x86-64-posix
  g++-mingw-w64-x86-64-posix`, then `cargo check --target
  x86_64-pc-windows-gnu -p transport -p audio -p video -p client -p
  server` (`transport` alone needs `--features server`). `video` needs
  two variables for `ffmpeg-sys-next`'s bindgen: `FFMPEG_DIR=<dir>`
  whose `include/` holds links to only the FFmpeg header folders
  (`/usr/include/<arch>-linux-gnu/libav*`, `libsw*`, `libpostproc`;
  the folder itself drags glibc's headers in) and an empty `lib/`, and
  `BINDGEN_EXTRA_CLANG_ARGS="--sysroot=/usr/x86_64-w64-mingw32
  -I/usr/x86_64-w64-mingw32/include"`. It is the GNU target, not MSVC:
  the Rust is checked, nothing is linked or run. The frontend's
  Windows code is not covered (Skia has no prebuilt for that target).
- **Android without an NDK**, the same way: `rustup target add
  aarch64-linux-android`, and let the C dependencies build with the
  host compiler, which is enough because `cargo check` links nothing —
  `CC_aarch64_linux_android=gcc CXX_aarch64_linux_android=g++
  AR_aarch64_linux_android=ar` and `CMAKE_TOOLCHAIN_FILE` pointing at
  a file that says `set(CMAKE_SYSTEM_NAME Linux)`,
  `set(CMAKE_C_COMPILER gcc)`, `set(CMAKE_CXX_COMPILER g++)`,
  `set(CMAKE_TRY_COMPILE_TARGET_TYPE STATIC_LIBRARY)`. Then `cargo
  check --target aarch64-linux-android -p audio -p video -p client`
  (AAudio, MediaCodec and the phone paths of the client). The
  `android` crate still needs the scratch crate above.
- **Dependency graphs** without a target installed: `cargo tree -p
  android --target aarch64-linux-android -e normal,build`, the same for
  `-p ios --target aarch64-apple-ios`.
- **Crates with no C in them, for any target** (2026-10-12): `rustup
  target add aarch64-apple-darwin aarch64-apple-ios`, then `cargo
  clippy --target <t> -p keywatch -p app-core` (macOS, iOS, Windows,
  Android) and `cargo check --target aarch64-apple-ios -p video` — on
  iOS `video` is `objc2` crates only, so `videotoolbox.rs` type-checks
  with no SDK. `audio` does not (cpal's Core Audio bindings want the
  SDK's headers). A call declared by hand (`keywatch/src/macos.rs`)
  is only as right as its signature: the check proves the Rust, not
  the declaration.
- **X11 keys for real:** `apt-get install xvfb xdotool`, start `Xvfb
  :97`, run `target/debug/examples/watch` (`-p keywatch`) with
  `DISPLAY=:97` and press keys at it with `xdotool key F7`, `keydown
  v` / `keyup v`. XTEST keys do raise XInput2 raw events. Wake-ups are
  the sum of `voluntary_ctxt_switches` over `/proc/<pid>/task/*/status`.
- **The Android host's Rust, with the `app` feature:** the scratch
  crate above now `#[path]`-includes `java.rs`, `host.rs` and
  `logging.rs` (and iOS's `logging.rs`), with `jni`, `android_logger`,
  `chrono`, the real `app-core`, and a stub `frontend` whose `host`
  module holds the `Hooks` trait and its types cut out of the real
  file by a few lines of Python. Java: API 34's jar is at the same
  address as 33's (`android-34/android.jar`) and has the constants
  `VoiceService` names.
- **Workflows:** `actionlint` (a static binary from
  github.com/rhysd/actionlint's releases) reads all of
  `.github/workflows/`. An old one does not know the newest runner
  labels; anything else it says is worth reading.

### The loop

Slint's viewer and the icon library, matching the app's Slint 1.18.1.
On an ARM Linux machine take
`slint-viewer-aarch64-unknown-linux-gnu.tar.gz` from the same release
(it wants `libinput10`), and the icon library is already in
`~/.cargo/registry/src/*/lucide-slint-1.35.0/` after any build:

```sh
mkdir -p /tmp/sv && cd /tmp/sv && curl -sSL -o sv.tar.gz \
  https://github.com/slint-ui/slint/releases/download/v1.18.1/slint-viewer-linux.tar.gz \
  && tar -xzf sv.tar.gz     # → /tmp/sv/slint-viewer/slint-viewer
mkdir -p /tmp/lucide && cd /tmp && curl -sSL -o lucide-slint.crate \
  https://static.crates.io/crates/lucide-slint/lucide-slint-1.35.0.crate \
  && tar -xzf lucide-slint.crate -C /tmp/lucide   # → /tmp/lucide/lucide-slint-1.35.0/lucide.slint
# Slint's own sources, for checking runtime behaviour instead of guessing
for c in i-slint-core i-slint-compiler i-slint-backend-winit; do
  curl -sSL -o /tmp/$c.crate https://static.crates.io/crates/$c/$c-1.18.1.crate
  mkdir -p /tmp/src-$c && tar -xzf /tmp/$c.crate -C /tmp/src-$c
done
```

```sh
cd /home/claude/work/crates/ui/slint
V=/tmp/sv/slint-viewer/slint-viewer; L="-L lucide=/tmp/lucide/lucide-slint-1.35.0/lucide.slint"

$V --check $L main.slint                                   # every .slint change
$V --screenshot out.png --load-data props.json $L main.slint          # render the main window
$V --screenshot out.png --component SettingsWindow --load-data p.json $L settings.slint
$V --screenshot out.png --component AdminWindow --load-data p.json $L admin.slint
SLINT_SCALE_FACTOR=2 $V --screenshot out.png ...          # 2× for site screenshots
```

`--load-data` takes JSON: top-level keys are the window's own
properties (`tab`, `phase` lives in `State`) and globals by name
(`State`, `SettingsState`, `AdminState`, `Tr`). The globals are only
visible through `main.slint`, which exports them: render the other
windows as `--component SettingsWindow ... main.slint` too, or `Tr` is
"unknown" and every string comes out empty. A `PopupWindow` never
shows in a still frame; to look at one (the emoji palette), render a
scratch copy of the tree with it turned into a `Rectangle`. Two
renders of the same fixture, before and after a change, diffed pixel
by pixel (`PIL.ImageChops.difference(...).getbbox()`), say exactly
what a refactor moved. Rows of a model must
carry **every** field of their struct — a missing `color`/`image`
field is filled with unit and panics at render (`binding was of the
wrong type: ()`); a missing string field just warns and the model is
left empty. `Timer`s do not fire in a screenshot (the hover card, the
follow), and `image` properties cannot be set from JSON.

For phone layouts and behaviour, the better tool is a **headless
harness**: a scratch crate whose `build.rs` runs
`slint_build::compile_with_config` on `crates/ui/slint/main.slint` with
the `lucide` library path, rendering `MainWindow` through Slint's
software renderer into a buffer at any size (375×667, 412×880,
1000×640), with `State` set from Rust and taps, double-taps, drags and
wheel events dispatched before the frame is taken (§4 has the
details). Its renders cannot show `transform-rotation` (the software
renderer ignores it; the phones use Skia, which does not). Read PNGs
back cropped to the region of interest — a full frame costs tokens.
One such harness lives in the repository: `tools/chat-scroll-sim`
(its own workspace; `CARGO_TARGET_DIR` outside the tree), which drives
the real chat pane through the scrolling cases and writes screenshots
with `SHOT=<dir>`.

### Handing over

`tar -czf <name>.tar.gz` the files newer than the marker (paths from
the repo root; leave out placeholders, `target/`, renders), cumulative
since the last archive the user confirmed. **The user unpacks with
`tar -xzmf`** (Windows' built-in tar has `-m` too): `-m` stamps the
extraction time; without it cargo's mtime check skips the Slint build
script, `ui` does not rebuild, and `frontend` fails with "no method
named `set_<new property>`" (2026-10-05). Say which files to delete —
the archive cannot. Edit scripts assert their anchors (`assert old in
s`): a silent miss once shipped a function that did not exist.

**Style:** edition 2024 (`audio` and `video` are still 2021), Rust
1.95, `clippy::all = deny`,
`clippy::pedantic = warn` (the allowed pedantic lints and why are in
the root `Cargo.toml`), rustfmt at 100 columns with the repo's
`.rustfmt.toml` — `imports_granularity = "Crate"` and `group_imports =
"StdExternalCrate"` are nightly-only, so stable `rustfmt` here formats
everything but the imports: write new imports the nightly way (one
`use` per crate, std / external / crate groups) and the user's
`cargo +nightly fmt --all` settles the rest. Markdown wraps at 80.
Third-party notices: `scripts/licenses.py` (cargo-about plus
`licenses/native/`; see licenses/README.md) writes
`crates/frontend/licenses.json` and `web/licenses.html`. CI runs it
(`.github/workflows/licenses.yml`) before every release build and site
deploy, so a Cargo.lock change needs nothing by hand; only a new native
library needs an entry in its `COMPONENTS`. Slint is taken under its
royalty-free license (the AboutSlint widget in Settings → About is its
condition); the desktop builds are GPL-3.0 as a whole because of FFmpeg
and x264, whose exact sources the notices name (x264 pinned as
`X264_COMMIT` in release.yml).

Every new string goes into all three Fluent catalogs (en, es-AR, pt-BR).
A string the Slint side shows is also declared in `tr.slint` — the name
only, never the English (the .ftl has it; copies there drifted from what
the app showed, 32 of them by 1.2). `frontend/build.rs` turns the names
into `fill_tr_from_ftl` and fails the build when a catalog lacks one;
`fill_tr` adds only the per-platform variants. Renders get the English
from `en.ftl` as `--load-data` (slint-viewer shows the names empty
otherwise).

## 2. Map

- `protocol` — the wire. Positional postcard: **never** add a field
  in the middle of a shipped struct or enum, never reorder; append
  variants, and gate every appended variant on a **new** feature bit
  (`features.rs`; a client that negotiated an old bit cannot decode a
  variant added later under it). Permission bits and names in
  `permissions.rs` (`PERMISSION_NAMES`, `ROLE_ICONS`,
  `derived_rank`). Admin commands in `admin.rs`.
- `transport` — the quinn-proto driver (sans-IO), lanes, media pacing,
  identity and pinning, the simulator.
- `server` — the tick in `server.rs`; child modules `moderation`,
  `history`, `channels`, `profiles`, `dms`, `files`, `admin`, `safety`
  (block, report, erase); `relay.rs` (voice and video fan-out); the
  persistence thread in `persistence/` (`store.rs` is SQLite, the CLI
  helpers `cli_*` live there too); `main.rs` is the CLI; migrations
  in `persistence/migrations/`. The CLI is `server rotate-key`,
  `server user|role|invite|restriction|key|telemetry|backup|restore`,
  `server db check|db prune`, `server user purge` (`server role --help`
  etc.); the config keys are PERSISTENCE-SPEC §1.
- `client` — the network thread (`network.rs`), the voice and screen
  threads, `config.rs` (settings, identity key), `inbound_wake.rs`,
  `viewlog.rs` (the viewer's 5 s summary lines).
- `audio`, `video` — devices, codecs, the jitter buffers, echo
  cancellation (`aec.rs`), the platform backends (`audio/src/native/`:
  AAudio, VPIO, WASAPI; cpal elsewhere; `video`: FFmpeg on the desktop,
  `mediacodec.rs` on Android, `videotoolbox.rs` on iOS).
- `app-core` — pure view models with unit tests: `chat.rs`
  (`ChatHistory`, `ChatItem`, `FileMeta`), `roster.rs`, `menu.rs`,
  `markdown.rs`, `images.rs`, `gif.rs` (the streaming player),
  `emoji.rs`, `composer.rs`, `devices.rs`, `update.rs`, `stats.rs`,
  `fps.rs`, `logfile.rs` (also the hosts' log writer and panic hook),
  `files.rs` (safe names, the pick slot), `usage.rs` (the phones' own
  CPU and memory), `meter.rs` (how a level bar moves). What two hosts
  need and no UI does goes here: it is the one crate that type-checks
  for every target from the sandbox.
- `keywatch` — the keybind watcher: key state and key events per
  desktop OS behind one `Watcher`, no dependency on the rest. Pure
  Rust, so `cargo check --target` covers its Windows and macOS halves.
- `ui` — the `.slint` files and `convert` glue (`lib.rs`):
  `main.slint` (window, overlays, card, dialogs, the phone pages),
  `connect.slint`, `startup.slint`, `sidebar.slint`, `chat.slint`,
  `settings.slint`, `admin.slint`, `viewer.slint`, `tray.slint`,
  `widgets.slint`, `theme.slint`, `tr.slint`,
  `globals.slint` (`State`/`Logic` and every struct the host fills).
- `i18n` — Fluent bundles (`locales/*.ftl`).
- `frontend` — the client's UI logic for every host: `app.rs`
  (`AppState`, `run`, the event pump, the timers, `fill_tr`) with its
  methods filed by concern under `src/app/` (`events`, `commands`,
  `presence`, `session`, `menus`, `devices`, `conversations`, `chat`,
  `profiles`, `stats`, `alerts`, `activity`, `safety`), `settingsui`,
  `adminui`, `media`,
  `viewers`, `present`, `resources`, `notice` (what a phone's
  notification is), `host` (one module: a `Hooks` object answers —
  the desktop's own `DesktopHooks`, or what a phone's crate installs:
  files, links, the keep-alive, notifications), and the host seams
  `clipboard`, `hotkeys` and `updater` behind the `desktop` feature,
  with same-API twins under `src/stubs/` for the phones (`instance`
  is desktop-only, with no twin).
- `desktop` — `main.rs` alone: logging, the config directory, the
  single instance, the panic hook, `frontend::app::run()`.
- `android` — the cdylib host: `lib.rs` (`android_main`), `java.rs`
  (every JNI call), `host.rs` (the `Hooks`), `logging.rs`. The Gradle
  project is `android/` beside `crates/` (Java only:
  `BaucordActivity`, `VoiceService`, `Notices`). Always `-p android`
  (ANDROID-SPEC.md §2.1).
- `ios` — the iPhone host, a binary: `main.rs`, `host.rs` (the `Hooks`:
  pickers, Quick Look, links), `notices.rs` (notifications),
  `lifecycle.rs`, `session.rs`, `insets.rs`, `paths.rs`, `logging.rs`,
  `render.rs`. The Xcode project is generated by XcodeGen from
  `ios/project.yml`; `ios/run.sh` is its `cargo run`. No Swift. Always
  `-p ios` (IOS-SPEC.md §2.1).

## 3. Shipped

| version | what |
|---|---|
| 1.1.0 | persistence thread, schema, invites and device keys, four built-in roles, restrictions, history with the ring, channels from the UI, telemetry, progress, admin CLI |
| 1.1.1 | direct messages with offline delivery, unread counts, paging, the conversation list |
| 1.1.2 | profiles (name color/style, avatars, contrast check), the app's own context menus with the volume slider, collapsible Messages, invite dialog, XP bar (`UserProgress`), share sounds, one instance per profile |
| 1.1.3 | custom roles with derived ranks, badges (icon + color) and `RoleList`; `panel`, `gold` and `admin-channel` as permissions; files on the blob path in General and DMs (cards, transfer ring, cancel, Open after save); the admin panel (eight tabs, confirm dialogs, humanized audit); resizable sidebar and Messages divider; hover on chat rows |
| 1.1.4 | blob retention, purge drops files, chat xp, pictures as files wherever the server is persistent, transfer budget (`limits.max_transfer_kb_per_minute[_global]`, old names alias), `server restore` / `db prune`, the admin channel (`AuditLine` feed), keybinds watched not grabbed, command output in the conversation it was typed in, closed DMs stay closed |
| 1.1.5 | the lobby (connect invisible, `LOBBY` bit 16, implicit channel, "joined <room>" lines, reconnect-rejoin, no xp in lobby or AFK); relay port 51000 by default with the client following the official server's move; drag a user onto a channel; images get Copy/Delete and a gallery copy button; LIVE chip opens the viewer; gold badges, green admin marker; timestamps on system lines and channel-move lines; `delete-messages` permission; log-level setting; window centred instead of remembered; the DM avatar ring gone, "Admin logs" with a `#`; unchanged profiles no longer trip the rate limit |
| 1.1.6 | the `frontend` split (the UI logic shared by every host; `desktop` is `main.rs`), dropping files from the OS onto the window, the tech-debt cycles (clippy clean at `deny`), drag-to-move and badge fixes |
| 1.1.7 | the Android client (sideloaded; voice, chat, DMs, files, stream viewing through MediaCodec, the in-window Settings and stream pages), the iPhone client (Ad Hoc; the same, VideoToolbox), echo cancellation (Windows, Android, iOS VPIO), Slint 1.18.1, the battery work (no polling workers, nothing drawn in the background), the stream page (zoom, fill, rotate), Settings → Debug (log upload), the one-row sidebar header, notifications on the phones |
| 1.1.8 | Windows asks not to lower other apps while in voice (AEC-SPEC E11, the vendored `wasapi`); the phones open the microphone only in a voice room and unmuted (ANDROID-SPEC D53, IOS-SPEC D51); the APK on the website, named like the desktop zips (D55); the Android release workflow on maintained actions, with *Run workflow* able to rebuild a release (D54) |
| 1.1.9 | phones keep up with a 60 fps stream (MOB-18: only the shown picture is converted, on several cores, into reused buffers; MediaCodec's pictures paced instead of dropped; a jitter buffer that holds a keyframe interval); stream audio through the call path on phones (MOB-12); the `viewing …` / `sharing …` stream logs; Settings → Debug → *Copy log*, and a clipboard on the phones; Windows audio that recovers from a device that came back (CLI-12, CLI-9); the video ladder reading a collapsed uplink (CLI-13); a quantizer floor after AMD keyframes too large to send (CLI-15); a stream's stereo mixed to mono on the phones without cancelling out-of-phase sound (ANDROID-SPEC D60) |
| 1.2.0 | what the stores require of a chat app (bit `SAFETY` 17; PERSISTENCE-SPEC §12 S1): block (hidden messages and DMs, no voice, DMs refused both ways; Settings → Profile lists them), report a message or a person (the admin channel at once, the panel's Reports tab, `/reports`), delete my account (everything it posted, at once), the terms linked on the connect card ("by connecting you agree"), `Authors` (a lobby poster's name, and the looks of history authors and DM partners offline), the chat's stickiness by input with an "N new messages" pill (ANDROID-SPEC D66, `tools/chat-scroll-sim`), third-party notices (Settings → Licenses, `web/licenses.html`, `scripts/licenses.py`, refreshed by CI before every build and deploy; AboutSlint in About), long press for a chat row's menu on the phones; General's **Activity** view behind a header toggle, beside clear-chat (joins, leaves, moves, shares and device notices out of the conversation; memory only, this session, 200 lines — `app/activity.rs`); Android's foreground service `microphone` in a room and `specialUse` in the lobby (ANDROID-SPEC D63); files of a deleted account's DM conversations released; `web/terms.html`, `web/delete-account.html` |

Memory-mode servers (persistence off) still use the legacy
`ImageMessage` relay for pictures and refuse files.

## 4. Rules learned the hard way

- `wait_for` closures must not borrow the client being pumped: copy
  `x.id` first. And `wait_for` does not consume the message it found.
- **A control message is at most 17 KiB, both ways**
  (`protocol::MAX_CONTROL_MESSAGE_BYTES`): every shipped peer drops a
  larger one unread, and until 1.2.1 nothing said so — history pages
  and admin lists vanished (review P0-1). A message carrying a list is
  fitted with `keep_prefix_that_fits` / `keep_suffix_that_fits` (one
  message the client replaces or prepends) or `split_to_fit` (only
  where the client merges batches: `Authors`, `HistoryAttachments`).
  An oversized send now logs a warning on the server.
- **The updater's relaunch waits for its parent** (1.2.1): a process
  started with `BAUCORD_RELAUNCH_AFTER_UPDATE` waits for the exiting
  instance to stop answering the instance socket before claiming it;
  without that it was told "already running" and quit.
- **Whatever the transport does on a clock needs a deadline in
  `Driver::next_deadline()`** (review P1-2): both loops sleep until a
  datagram or that deadline, so work without one waits for the next
  datagram. Poll transmits until `None` before asking. The simulator's
  `step_to_next_event` tests catch a missing deadline.
- **Voice packets never pass through the voice thread** (review P1-5):
  it hands each engine's `VoiceLink` (the `IncomingSender` and the
  encoder's receiver) to the network thread at start, and wakes on
  commands, speaking edges (`VoiceChat::speaking_edges`) and once a
  second for `service()`. The UI reads meters through
  `SharedAudioState::levels`. A new per-packet duty belongs in the
  network thread or the engine, not in that loop.
- **Audio workers wait on their device latch** (review P1-4), with
  `LATCH_SAFETY_WAIT`; they leave when `wait_for_wake` returns false,
  and their owner rings the latch on drop (`stop_ring`) so a stop is
  immediate. A stream's own output has no worker: the decoder writes
  the ring and the callback reads it through `stream::pace`
  (`Pacing::own_output`: 60 ms cushion on the desktop).
- **`sync_voice_room` runs on every platform** (review P1-6): the lobby
  and the AFK room are out of a room; the desktop drops what the
  microphone hears there (`InputConfig::in_room`), so its Settings
  meter is flat unless the microphone test runs.
- **Windows "Default" is one answer** (`communications_default`, review
  P1-18): the Communications device unless echo cancellation is Off,
  for the decision, both streams and the watcher. Never resolve it per
  call site.
- **swscale never writes into a tight buffer unless the width is a
  multiple of 16** (review P1-11): FFmpeg 7.1's SIMD writes whole
  blocks (8 pixels on x86, 16 on aarch64), past the row's end or short
  of it. Other widths decode through the padded frame.
- **FFmpeg 7.1's AMF wrapper returns nothing from a receive without a
  new frame**, so `drain()` cannot release its last frame; the first
  still tick re-encodes it once (review P1-13).
- **The roster is rebuilt once per pass, and only on screen** (review
  P1-9): `update_user_list` queues `flush_roster`. A new path that
  brings the main screen back must call `flush_roster`; speaking and
  audio-state edges use `refresh_voice_row`.
- **Bytes from the wire decode with `images::decode_received`** (PNG,
  JPEG, GIF; review P1-20); `decode_bounded` is only for pictures the
  user picks, pastes or drops.
- **Keybinds are armed only while connected** (review P1-3):
  `HotkeyManager::set_connected` at `Connected` and in teardown;
  keybind and push-to-talk changes go through `refresh_hotkeys`.
- **Parallel worktrees sharing one `CARGO_TARGET_DIR` reuse each
  other's workspace-crate artifacts** (same fingerprint, decided by
  file times): a test can run another tree's code. Give each its own
  target directory, or `cargo clean -p` the workspace crates before
  trusting a merged build.
- **A new wire variant needs a new feature bit** (spec §12): the
  first cut gated `UserProgress` on `PROGRESS`, which a 1.1.0 client
  negotiates too — it would have choked on the variant. Now
  `PROGRESS_ALL` and `ROLE_LIST` (bits 11 and 12).
- `app-core`'s chat test helper multiplied a placeholder id near
  `u64::MAX` by 1000: fine in release, an overflow panic under
  `cargo test` (debug assertions). `saturating_mul` now.
- Tests about media connect people with `connect_member`; `connect`
  makes guests, who cannot share, upload, or DM.
- A CLI change reaches a running server within about 3 s
  (`data_version` poll every 2 s, the tick's socket wait up to 1 s) and
  is applied to live sessions (bans close, kicks close, roles refresh);
  only what changed is sent. **Nothing on a CLI read path may commit or
  restart the WAL** (a TRUNCATE checkpoint, an UPSERT of an unchanged
  row): the running server's poll counts either as a change and
  reloads (review P1-22). Listings open the database read-only.
- `/kick` forgets keys and roles (needs a new invite); `/ban` is the
  restriction; a ban never catches the actor or an equal rank, and
  skips an address someone else is on.
- `VoiceChat::start()` says what opened (`StartOutcome`; an `Err`
  for either stream until 2026-10-12, review P3-24): keep the engine
  on `NoMicrophone`, or a missing microphone silences playback too
  (the Android phone test, 2026-10-05 — and a PC with no capture
  device, the field incident the audio crate's comment describes). A
  side that is not open —
  never opened, or its reopen failed — is retried by `service()`
  (`input_pending`, `output_pending`; until 2026-10-08 a failed
  reopen was never retried, CLI-9); a failed stream by the
  `*_stream_failed` flags.
- Windows opens its voice streams on the native WASAPI backend
  without asking cpal first; cpal is only the fallback, asked when the
  native stream will not open (CLI-12). Asked first, a cpal failure
  ("Cannot change thread mode after it is set", after a device change)
  kept the native path from ever being tried.
- Chat earns 1 xp per message since 1.1.4 (at the minute flush); an
  image counts as a chat message for mutes and
  the rate limit.
- `/help` is built from `Session.permissions`; never advertise a
  command the server would refuse — the roster menu mirrors the rank
  rule per row for the same reason.
- Regenerating an identity makes a new user; guests can repeat a
  member-free name, so ghosts pile up — `server user purge` (or an id
  prefix in any CLI `<who>`) handles them.
- `EditChannel` replaces every field, and the server never sends a
  channel's password back: an edit must resend it, so the client
  keeps passwords it created or joined with in `channel_passwords`
  and `current_spec` reads them from there. A password the client
  never saw is cleared by any edit — the dialog says so.
- The server's edit/delete ring (200 most recent) holds text *and*
  blob messages (a picture on a persistent server is a blob, spec
  §7); only memory-mode live-relay images (`ChatImageChunk`) never
  enter it, so they cannot be deleted. Nothing edits an image. The
  server is *silent* when it refuses an edit or delete — the
  client's 3 s timer is the only feedback. (Until 1.1.5 the client
  menu hid Delete on every image; `app-core::menu` returned early.)
- `UserDetails` shipped with four positional fields, so the profile
  became its own variant (`UserProfile`), not a fifth field (§12).
- The server type is `ChatServer`; `Config` nests `avatars` under
  `persistence`; the client re-exports `protocol` as a module.
- `ContextMenuArea.enabled`, `if`/`for` inside `Menu`, the `Timer`
  element, `Window.full-screen`, and `absolute-position` all exist
  in Slint 1.17; `StyledText` cannot be selected (hence the plain
  view toggle); Slint maps ⌘ to `control` and physical Ctrl to
  `meta` on macOS; a layout child's `y` is ignored — wrap it in a
  plain `Rectangle` and center there; a `ListView` wants only `for`
  children (use `ScrollView` + layout for short mixed lists); ids
  inside `if` are not visible outside it.
- Slint globals are per window instance: `Theme.scale`, `Tr`, etc.
  must be set on every window (`apply_text_scale`).
- `invoke_from_event_loop` closures must be `Send`: never capture
  `self` (the windows) — read what you need from the closure's own
  `with_current(|app| …)`.
- Row ids for chat rows come from `AppState::next_row_id`, unique
  across conversations, because images and GIFs are keyed by them and
  conversations swap models.
- **Verified in Slint 1.17.1's sources (crates.io), not guessed:**
  `i-slint-backend-selector` enables `i-slint-backend-winit/muda` on
  macOS and Windows whenever `backend-winit` is on, and
  `show_native_popup_menu` always tries muda first — so
  `ContextMenuArea` is native there and there is no runtime opt-out.
  Mouse dispatch is front-to-back among siblings; the first item to
  accept wins and the parent's own `input_event` only runs when every
  child ignored. `TouchArea` returns `GrabMouse` on *any* button, so a
  row's TouchArea swallowed the right-click before the enclosing
  `ContextMenuArea` saw it — why right-clicking a channel header did
  nothing (user rows only worked because they left-clicked into
  `menu.show()`). `TextInput` ignores every button but the left one
  (a `TouchArea` declared first, under a read-only body, gets the
  right-click). `Flickable` intercepts only the left button.
  `StyledText` accepts every press and move (`shared-parley`, on by
  default), so a right-click on a formatted body opens nothing — the
  header, gutter and margin do — and the double-click-to-select
  TouchArea under it never fires; both pre-existing.
- **"Slint: long callback/instantiation chain detected"** is
  `ensure_tree_instantiated` giving up after 10 rounds of
  instantiate-then-run-change-handlers. A `changed viewport-height`
  handler on the chat ListView stacked into it while scrolling (every
  virtualization refinement is a round, and the `if`-gated delegates
  add a nested one per row). The owed follow is now a bounded 16 ms
  `Timer` (at most 90 ticks; it stops after 8 frames at the end,
  ANDROID-SPEC D66) instead. Rule: never scroll from a `changed`
  handler (D66's own `changed content-y` only records where the reader
  is; it does not move the list).
- **The horizontal scroll, found by bisecting renders:** a
  `ListView` sets its `viewport-width` to the max of its rows'
  *minimum* widths (`listview_layout` returns
  `layout_info(Horizontal).min`), and a row's minimum merges every
  unpositioned child — `visible: false` ones too. The delegate hosted
  all five row components per row, and `DayMarkerRow`'s pill was
  `width: label.preferred-width + …` with the *message body* as its
  label, so every long message demanded a pill's worth of width and
  the list grew a horizontal scrollbar (the 2 px thumb above the
  composer in every screenshot, sideways drift on a trackpad). Fixed
  twice over: the delegate instantiates only the matching kind with
  `if` (a conditional child contributes no constraint — which also
  means a bisect built on `if` variants proves nothing, learned the
  hard way), and the pill is capped at 80 % of the row with an
  elided label. A wrapping `TextInput`/`Text` reports `min: 0`, so
  bodies never did this; word-wrap also breaks a 180-character token
  at character boundaries.
- A `pure` callback in a binding is **cached per element** until a
  Slint property it read changes; reading Rust state by *index*
  (`user-drag-data(i)`) went stale as soon as the roster reshuffled,
  so a drag carried whoever sat in that slot before (1.1.5's "can't
  move them back"). Key such payloads on the row's own data
  (`user.client-id-lo/hi`), never on its index.
- **Drag and drop** (Slint 1.17): `DragArea` must be the row's
  *parent* — it watches the press through `input_event_filter_before_
  children` and only intercepts past the threshold, so a sibling in
  front would never see the moves once the row's TouchArea grabbed.
  `DropArea` ignores ordinary mouse events, so it can sit in front of
  the row. The payload is an opaque `data-transfer`: built by a
  **`pure`** host callback (a binding may only call pure callbacks —
  the compiler says "Call of impure callback") carrying our own type
  via `set_user_data`, read back with `user_data().downcast`. In a
  `ListView` the wrapper needs an explicit `height` (bind it to the
  row's). The drop highlight cannot be rendered by the viewer (it
  needs a live drag); QA it in the app.
- Nothing in the app layer knew the negotiated feature set until the
  lobby; `NetworkMessage::Connected { lobby }` is the pattern for the
  next bit a client must gate on (add a field, not a new message).
- Settings code talks to `settingsui::surface(app)` (an enum over
  the desktop `SettingsWindow` and Android's in-window page), never
  to `app.settings_win` directly — the field is the desktop window's
  storage only. The page is wired once in `app::run`; the window when
  first created. The stream viewer is the same shape: `viewers::Viewer`
  owns a private `Surface::{Window, Page}` and the rest of the app
  only calls its methods (`raise`, `update_frame`, `close`,
  `apply_text_scale`) — never a `.win`. Anything that opens its own
  `Window` on the desktop gets a `*Panel` component + `State.*-open`
  page on Android (ANDROID-SPEC.md D22/D23); that is the pattern for
  the admin panel too.
- **`VideoDecoder::flush()` is advisory.** The client calls it before
  every IDR; FFmpeg honours it (cheap), MediaCodec deliberately does
  not (it would discard the pipeline's pictures every 2 s, see
  `video/src/mediacodec.rs`). Don't make a decoder's correctness
  depend on it — an IDR resets reference state on its own.
- **A playback cushion is sized from the device's callback, never as
  a fixed number of ms.** 20 ms ahead was two periods of a 10 ms
  desktop device and half of one 40 ms AAudio callback — every Android
  callback came out half silence (D24). The tell in any log: an
  `Underruns` count that climbs at a steady rate (~one per callback)
  with `Lost 0`. `output::Cushion` does the arithmetic; the worker
  logs `Output callbacks of N frames …` once per size it sees.
- **Read a steady counter as a rate before blaming the platform.** The
  emulator's `Underruns 162` in 6 s was 27/s — one per callback — and
  was first written off as the emulator's audio clock. Divide by the
  time and compare with the callback period first.
- **Android-only code can be type-checked here** (and, since
  2026-10-12, Windows-only code too — §1). `ndk` has a `test` feature that compiles its bindings off
  Android: a scratch crate depending on `ndk` (`audio`, `api-level-28`,
  `test`), `jni` 0.22, `ndk-context` and the real `audio` crate, with
  `#[path]` modules pulling in `native/aaudio.rs` + `native/android.rs`
  and tiny shims for the `crate::` items they use, runs `cargo clippy`
  clean (2026-10-06). The `windows` 0.62 crates do not build off
  Windows at all (`windows-future` needs `windows_core::imp::IMarshal`;
  `--cfg windows` then trips on `std::os::windows`), so `wasapi.rs`
  changes are checked against the crate sources by reading and by the
  user's first Windows `cargo check`. Say so in the hand-over.
- **Check a pasted log's first line before reading it.** Settings →
  Debug → *Copy log* starts the text with `# baucord <version>
  (<os>/<arch>), copied … at <time>, from <path>`: the build, the
  machine and the file. A log without it was taken by hand and may be
  the wrong one — on 2026-10-08 a "sharer's log" was a stale copy from
  another folder, an older build, and a different session (its client
  ids matched nothing in the phone's log). Ask for *Copy log* from the
  same time as the other side's.
- **No per-frame or per-event `warn!` for a steady state.** A timing
  alarm that fires once a frame is 60 lines a second on any machine
  slower than the one it was tuned on (the emulator: 652 of 729 logcat
  lines), and once a second is still 120 lines a minute repeating one
  fact (the A07, 2026-10-08). A stream logs one summary line every 5 s
  (`client::viewlog`: `viewing …` per watched stream, `sharing …` for
  our own) — warn when something is wrong, info otherwise — and a line
  only when its state changes (falling behind live, caught up, frozen
  waiting for a keyframe, resumed). New measurements go into those
  lines, not beside them.
- **Reading a stream's lines.** `viewing <peer> 1920x1080: in 60.0/s,
  decoded 59.6/s, shown 41.2/s (92 skipped for a newer one) … |
  decoding busy 70%: codec 8.1 + output 1.3 ms (p95 …) | behind live
  60 ms (max 140), then 12 ms to screen (max 30; preparing 9.8 ms
  each) | audio 120 ms queued`. *In* below the sharer's encoded rate
  = the network (check the `sharing` line); *decoded* below *in* with
  busy near 100 % = the device cannot decode the stream — *codec* is
  the hardware decoder, *output* getting the picture out of it (the
  desktop's colour conversion; on Android a copy of the YUV planes);
  *shown* below *decoded*, with skips, = the present thread or the UI:
  *preparing* is the present thread's work per shown frame (on Android
  the YUV → RGBA conversion, spread over up to four cores), and above
  a frame period (16 ms at 60 fps) it is the limit — frames are then
  skipped by design, the picture stays live. *Behind live* is how late a frame is
  against the fastest delivery seen (the playout delay and network
  wobble are part of it: tens of ms); a value that keeps growing is the
  A/V drift, and *audio queued* is the audio's side of it (stream
  audio has no timestamps yet). `N not decoded (off screen)` is normal
  while the app or the viewer is hidden (review P1-7): those frames
  were dropped before the jitter buffer. `sharing 1080p 60fps: … encoded
  58.4/s … | offered 6.8 Mbit/s, sent 6.5 Mbit/s, abandoned 0/3
  groups, path loss up to 0%, rtt 45 ms | bitrate 100%, fps 100%, 1
  viewer`: offered far above sent with groups abandoned = the uplink;
  the bitrate ladder should be stepping down (`bitrate ladder:`
  lines). `N too large to send (up to X KB)` and `bitrate 100% (QP ≥
  28)`: the encoder made frames over the wire's 275 KB and runs with
  a quantizer floor until they fit (CLI-15, AMD mostly).
- The platform-free half of a platform feature goes in its own module
  with host tests (`video::yuv` for MediaCodec's planes → RGBA): it is
  the only part the assistant can run here, and `rustc --test` on the
  single file beats waiting for the phone. The Android-only half can
  be type-checked on the host against a signature-only stub of the
  `ndk` API (a scratch crate named `ndk` with the same method
  signatures, `unimplemented!()` bodies) — that caught nothing this
  time but costs a minute, and the real build takes ten.
- **PowerShell scripts for the user are ASCII-only and saved with a
  UTF-8 BOM (CRLF).** Windows PowerShell 5.1 reads a BOM-less file as
  the ANSI code page; a UTF-8 em dash becomes `â€”`, and that `”` is
  a legal double quote to the PowerShell lexer — the string closed
  early and the rest of the message ran as a command (`run.ps1`,
  2026-10-05: "The term 'drop' is not recognized").
- A menu action lives in **one** place since 2026-10-12 (review
  P2-57): `app-core::menu::Action`. `ui::convert::menu_entries` hands
  the overlay rows carrying an `id` and keeps the actions by that id;
  the pick comes back as the id (`AppState::menu_actions`). The Slint
  `MenuAction` enum and the two conversions in `ui/src/lib.rs` are
  gone: a new action is a variant and its arm in `menu_activated`.
- **A text input drops its selection when it loses the focus** (Slint's
  `FocusOut`, any reason but a window or popup activation). Anything
  that opens over one and then acts on its selection — the composer's
  menu was the case, review P2-16 — has to read
  `cursor-position-byte-offset` / `anchor-position-byte-offset` before
  it opens and do the edit host-side (`app_core::composer`).
- **A new dialog is a `Modal`** (`widgets.slint`): `card-width`,
  `dismissed` (Escape, Back, a click outside), `key` for Return, the
  content as children. A field that should have the keyboard is
  focused in the dialog's `opened`, never in the field's own `init`:
  that one runs *before* the component's, whose scope then takes the
  keyboard back (found the hard way — no render shows it).
- **A person's chat row inherits `RowFrame`** (`chat.slint`): it sets
  `top` and where the name line goes and puts its body inside. In a
  component that inherits another, the base's properties are not in
  scope unqualified: `root.data`, not `data`.
- **Which audio streams are open is decided in one function**,
  `VoiceChat::decide` (`audio/src/lib.rs`), and done in four
  (`plan_and_open`, `apply_sides`, `open_output`, `open_input`). A new
  rule — "on Windows open the call streams only in a room" — is a
  branch there and a scenario in `audio/src/supervision_tests.rs`,
  run under that platform's `Traits` on the stand-in machine
  (`fake.rs`): the test reads the log of plans, opens and closes.
  Platform differences in *decisions* are `Traits` fields, never
  `cfg` — a `cfg` branch cannot be tested on another host. The fake
  has to behave like the streams where the engine depends on it (a
  stream that starts clears its "failed" flag; the first version did
  not, and every tick reopened everything).
- **Sending the server a message that needs nothing else is
  `NetworkControl::Send(ClientMessage::…)`** — no new variant of
  `NetworkControl`, no arm in `handle_control`. A variant of its own
  is for a message the network thread has to do something about (an
  upload to chunk, a state to remember).
- **A hash is `transport::sha256`** (`digest`, or `Hasher` for bytes
  that arrive in pieces). A dependency several crates share is
  declared once, under `[workspace.dependencies]`.
- **A list for an admin goes out through `send_admin_result`**, never
  as an `AdminResult` built by hand: it pages what does not fit one
  17 KiB message (`AdminRows`, then the result that closes them) and
  cuts to the first page for a session without `ADMIN_PAGES`. A new
  listing gets its arm in `AdminOutcome::into_pages`, `rows` and
  `prepend` — the compiler asks for all three.
- **A test of the process's own CPU or memory asserts nothing about
  "idle" or "before and after"**: the figures are the whole test
  binary's, and the other tests run beside it (`usage.rs`'s first
  version failed in the full run and passed alone).
- **Code two hosts need goes in `app-core`, not copied into both**:
  the log writer, the panic hook, the pick slot and safe file names
  were three copies each. Code for one OS that needs no UI goes in a
  crate without C dependencies (`keywatch`), where the sandbox can
  type-check it for that OS.
- **A blind platform change gets a way back that needs no release**:
  the key watcher falls back to its old clock when its events prove
  unreliable, the Mac's hardware encoder to libx264 when it will not
  open. Where there is no such way back (the pointer in a share), it
  is not written blind.
- **An e2e test that waits for the database waits in real time**: the
  world's `settle` runs simulated time, which a busy machine gets
  through before the persistence thread has done its write. Poll for
  the thing itself (`rows_become`, or ask again in a loop).
- **What belongs to one connection goes in `Session`** (`app.rs`; or
  its `Safety`, `Transfers`, `ChatFlow`), not in `AppState`: the
  teardown replaces it whole, so nothing has to be remembered there.
  A session thing that also shows on screen is cleared where it is
  shown, beside the reset.
- **A wrapped `Text` inside a `HorizontalLayout` inside a
  `VerticalLayout` reports one line's height**, and the next element
  is drawn over the rest. Give the box an explicit `height:
  <text>.preferred-height + padding`, as the chat's standing notice and
  the settings' Windows notice do.
- **The narrow (phone) layout renders only at a phone width:**
  `MainWindow` has `preferred-width`/`min-width`, so a fixture alone
  draws the wide layout. Copy the `.slint` tree, swap
  `preferred-width: 1000px` → `width: 412px` (and the height; drop
  both `min-*` lines — the viewer rejects `height` next to
  `min-height`), then render; `State.narrow-chat-open: true` shows
  the chat page, `State.touch-host: true` the touch-only affordances
  (the DM ✕); `State.settings-open: true` the Settings page, with
  `settings-tab` and `settings-section-open` picking list or section.
  `--component SettingsWindow ... settings.slint` with `{"tab": 2}`
  checks the desktop window didn't move. The slide itself (a `Timer`-free `animate`) cannot be
  captured. Check 1000×640 after any change there: the wide layout
  must not move.
- **Drive the real `MainWindow` headless** when a fix is about
  behaviour (a tap, Back, focus): a scratch crate whose `build.rs`
  runs `slint_build::compile_with_config` on
  `crates/ui/slint/main.slint` with the `lucide` library path (as
  `ui/build.rs` does), `slint::include_modules!()`,
  `i_slint_backend_testing::init_no_event_loop()`, a 412×880 window,
  `State` set from Rust, then `dispatch_event(PointerPressed/Released
  | KeyPressed { text: Key::Back.into() })` with `mock_elapsed_time`
  + `update_timers_and_animations()` between, asserting on `State`
  and on Logic callbacks hooked to counters. Used for D29 and D32.
- **`ndk_context::android_context().context()` is the Application,
  not the Activity** (android-activity 0.6): fine for `AudioManager`,
  wrong for anything Activity-only (`requestPermissions`, our
  `BaucordActivity` methods). The Activity is `java::init`'s, from
  `AndroidApp::activity_as_ptr()` (D33).
- **The keyboard and system bars on Android** are on the `Window`:
  `safe-area-insets`, `virtual-keyboard-position/size`. The pages sit
  in `MainWindow`'s `inset-*` rectangle; a new full-window page must
  too (D35). The headless harness can set a keyboard with
  `window.set_virtual_keyboard(pos, size, i_slint_core::InternalToken)`
  (dev-dependency `i-slint-core = "=1.18.1"`).
- **Start the app the way the launcher does, not by a hard-coded
  class**: `cmd package resolve-activity --brief -a
  android.intent.action.MAIN -c android.intent.category.LAUNCHER
  dev.baucord` (last line = the component), then `am start -n` it; or
  `monkey -p dev.baucord 1`. Not an implicit `am start … dev.baucord`:
  Activity starts resolve with `MATCH_DEFAULT_ONLY`, and our launcher
  filter has no `category.DEFAULT` ("unable to resolve Intent"). `am`
  exits 0 on errors; check its words (D37). A Linux PowerShell 7 can
  be fetched from the PowerShell GitHub releases to test `run.ps1`.
- **The public crates move together.** `protocol`, `transport`,
  `client`, `server`, `frontend`, `desktop`, `android` and `ios` carry
  one version — the release's — and it is written once: `version`
  under `[workspace.package]` in the root `Cargo.toml`, which the
  eight inherit (`version.workspace = true`) and which the release
  workflows, Android's Gradle script and the iOS scripts read
  (IOS-SPEC.md D25). Bump that line; `cargo test -p protocol --test
  versions` fails if a public crate sets its own. 1.1.6 bumped only
  the apps, and clients told the server they were 1.1.5. The internal
  libraries keep their own numbers.
- **No early-leaving readers under `pipefail`.** `cmd | head -n 1` or
  `cmd | grep -q x` in a `set -euo pipefail` script ends it silently
  when `cmd` writes after the reader has left (SIGPIPE, 141) — a race
  that passes on one run and kills the next (IOS-SPEC.md D27). Read the
  output whole and cut it in the shell, let `awk` read the file and
  `exit`, or `grep … >/dev/null`; and trap `ERR` so a stop says where.
- **`target_os = "ios"` is not macOS.** A `cfg(target_os = "macos")`
  branch does not cover the iPhone, and crates declare APIs per OS:
  socket2 0.6.5 has `set_tclass_v6` on macOS but not iOS (IOS-SPEC.md
  D17), and a guard that has `Drop` on macOS/Linux had none on iOS
  (D16). Check iOS with the build-std clippy of §1 before handing
  over anything that touches `cfg`s or platform crates.
- **On iOS the event loop never returns.** `frontend::app::run()` hands
  the main thread to `UIApplicationMain`; the desktop's `shutdown()`
  never runs and a suspended app is ended without notice. Whatever
  must survive is saved when it changes or in
  `persist_for_background` (IOS-SPEC.md §4.3). Nor may the app end
  itself (`HOST_CAN_QUIT`, D29).
- **objc2 on iOS** (`crates/ios/src/host.rs`, IOS-SPEC.md D31): in a
  `define_class!`, put a protocol's methods inside that protocol's
  `unsafe impl` block — a Debug build checks at class registration
  that every *required* method was registered there and panics
  otherwise; the generated bindings sometimes declare a method for
  macOS only (the Rust trait is then empty on iOS), and the selector
  goes in the block all the same. Rust names stay snake_case and the
  selector lives in the attribute (the workspace denies
  `nonstandard_style`, which also rules out matching on a framework's
  `kSomething` constants — use a table). UIKit delegates are weak:
  keep them alive yourself. A sheet the user swipes down tells only
  its presentation controller's delegate.
- **Slint 1.18's touch pinch overshoots** (IOS-SPEC.md D44): the
  runtime feeds `ScaleRotateGestureHandler` increments of the finger
  distance's ratio and the handler multiplies them, so on a
  touchscreen `scale` ≈ e^(r−1) (twice the distance → ×2.7). The
  stream page uses `1 + ln(scale)` when `pinch-from-touch`; a
  trackpad's pinch is right as it is. Re-check on a Slint upgrade.
  And chrome that fades with `opacity` still takes taps unless it is
  also made invisible (`visible: self.opacity > 0.01`).
- **An Android keyboard's composing word is not in a TextInput's
  `text`** (ANDROID-SPEC.md D48): Slint keeps it in `preedit-text`,
  draws it with the selection colours, and commits it only on a space
  from the keyboard or when the field loses focus. Anything that acts
  on a field's text from Enter or a button commits first (the composer
  and `Field` cycle focus when `preedit-text != ""`; `Btn` takes focus
  on Android). A new editable field: same pattern.
- **A platform canceller is verified on the stream we opened, not
  assumed from the device** (AEC-SPEC D5, E9): on Android the
  low-latency mode without a session id is MMAP, which skips every
  effect — the voice-call streams use performance mode None and read
  `AcousticEchoCanceler.getEnabled()` back on their own session; on
  iOS the VPIO bypass property is read back after `initialize`.
- **No worker polls on a fixed short timer** (IOS-SPEC.md D41,
  ANDROID-SPEC.md D43): a thread blocks on what wakes it — a channel,
  the audio callback's latch, the socket through `ReadWaiter` — with
  a timeout only for timers it really has. The network loop's 1 ms
  poll was a thousand wakes a second for a whole session on a phone.
  `client/tests/network_idle.rs` counts a quiet connection's loop and
  watcher wakes (`network::loop_passes`, `watcher_wakes`) on every
  platform: keep the two under 25 (0 + 2 at 1.2.1). And what only feeds
  the screen checks `main_on_screen()` / `host_visible()`: a phone in
  the background shows nothing (D42).
- **In `build.gradle.kts`, never write `java.awt.…` / `java.io.…`
  out in full**: `java` resolves to the project's `java` extension (AGP
  applies java-base) and the script fails with "Unresolved reference:
  awt". Import the classes at the top. Gradle 8.14.3 is installed here
  (`/opt/gradle`) but Maven is blocked, so AGP itself cannot run;
  check Kotlin script code in a scratch project with `java-base`.
- **Android 16 + targetSdk 36 never delivers `KEYCODE_BACK`**
  (predictive back): the manifest's `enableOnBackInvokedCallback=
  "false"` is what makes every `Key.Back` handler run (D29).
- **JNI from Java into Rust**: NativeActivity `dlopen`s our library
  itself, so a `Java_…` export is invisible to the Activity's class
  loader — register natives from Rust (`env.register_native_methods`
  on `get_object_class(activity)`). jni 0.22's `native_method!` takes
  `error_policy = Ident` (an identifier in scope, not a path — the
  docs show a path and it fails to parse). The Java compiles against
  stubs here: `javac --release 8` with a dozen hand-written
  `android.*` stub classes (`scratchpad/jstub`) catches syntax and
  checked-exception mistakes; Gradle is the real check.
- `z` takes a number literal only (`z: cond ? 0 : 1` is a compile
  error); order siblings, or lay them out so they never overlap.
- **The widget style is fluent everywhere, Android too** (no style in
  `ui/build.rs` → the compiler's fallback is fluent, not the per-target
  "native" table). Fluent's `ScrollView`/`ListView` never pan on a drag,
  so every scroll view a phone reaches binds
  `mouse-drag-pan-enabled: State.touch-host` (D27) — a new one must too.
- Interactions (drags, flicks, long-press) can't be screenshotted, but
  they can be driven headless: a scratch crate with `slint =
  { version = "=1.18.1", default-features = false, features = ["std",
  "compat-1-2"] }` and `i-slint-backend-testing = "=1.18.1"`, a
  `slint::slint!{}` copy of the element under test,
  `i_slint_backend_testing::init_no_event_loop()`, then
  `window.dispatch_event(WindowEvent::Pointer*)` with
  `mock_elapsed_time` between moves, reading back an `out` property
  (`viewport-y`). Builds in a few minutes with `debug = 0`; Android's
  touch input reaches items as these same pointer events.
- A render fixture's `image` fields (`avatar`, `viewer-image`) must
  point at a real file — `""` makes the whole model fail to load
  ("Failed to load image from path") — a 1×1 PNG written by the
  fixture script does; `/tmp/fx/main.json` is a full-field roster
  fixture (admin, sharer, guest, custom badge) worth regenerating —
  without `outranked`, `role-editable`, `channel-kickable`, `owned`
  and `from-config`, which left `UserRow` in 1.2.1 (the menu works
  them out when it opens, review P1-9).
- External drag-and-drop reaches the app only through
  `on_winit_window_event` (`WindowEvent::DroppedFile` and friends);
  Slint's `DropArea` is for in-app drags on this backend. The hook
  may be set once per window, so new event kinds join the existing
  closure in `repaint_on_reveal`.
- **The chat keeps to the newest message by the reader's input, never
  by the scroll position** (ANDROID-SPEC D66). A `ListView` rewrites
  its own `content-y` whenever its estimate of the rows it has not
  laid out moves — up as often as down — so a position-based rule
  lets go on its own. Only the list's `scrolled` and the pane's own
  scrollbar (the stock one is off: it moved the list without
  reporting it) count as the reader. Any change to the chat's
  scrolling runs `tools/chat-scroll-sim` (48 checks against the real
  pane).

- **A phone's alert chime asks the host first** (ANDROID-SPEC.md D52,
  IOS-SPEC.md D50): the voice call's audio ignores the ringer and the
  ring/silent switch, so a DM or mention chime goes through
  `AppState::play_alert_chime`, which asks `host::chime_route()` —
  the engine where the ringer is on, nothing while the phone is
  silenced or in Do Not Disturb (Android), a sound-only notification on
  iOS. A new alert sound takes the same road; the join/leave and share
  chimes are part of the call and do not.
- **A phone opens the microphone only in a voice room, unmuted**
  (ANDROID-SPEC.md D53, IOS-SPEC.md D51): the system shows an open
  capture stream as recording, muted or not. `sync_voice_room` (from
  `update_user_list`) tells the voice thread `SetInRoom`; the engine's
  `set_presence` opens and closes what that needs. Anything new that
  needs the microphone outside a room (a test, a recorder) must make
  `sync_voice_room` count it, as the microphone test does. An Android
  engine in the lobby has no playback either: chimes there go through
  `host::play_chime`.
- **On a phone's call path a stream plays through the voice output**
  (ANDROID-SPEC.md D56, IOS-SPEC.md D52): anything that should sound
  with the call (stream audio, and any future in-call playback) goes
  into that output's mix (`stream::mix`), never onto a second output —
  a second output is *media* beside the call: the wrong volume (the
  buttons move the call's), the wrong route, outside the echo
  canceller, and on some phones silent. Deafen and the voice volume do
  not touch the stream; its own volume and mute are applied before the
  mix.
- **A Windows Communications stream ducks every other app** (by 80 %
  by default), and so does *any* stream opened on the device
  `GetDefaultAudioEndpoint(eCommunications)` returns. Every stream
  asks Windows not to (`ask_not_to_duck` in `native/wasapi.rs`,
  AEC-SPEC.md E11) and logs `do-not-duck=` on its open line; a plain
  stream opens the Communications default by its id instead
  (`stream_device`, E14). Never hand a role-fetched device to a
  stream that is not on the call path. Windows 10 has no opt-out.
- **`third_party/wasapi` is crates.io's `wasapi` 0.24.0 plus one
  accessor** (`AudioClient::as_iaudioclient`), wired in by the root
  `[patch.crates-io]` and kept out of the workspace (`exclude`): never
  `cargo fmt` or edit it beyond the patch, and re-apply the patch if
  the crate is upgraded (`third_party/wasapi/BAUCORD-PATCH.md`).
- **Whatever the root `Cargo.toml` names by path must be in the
  Dockerfile.** Cargo reads every `[patch]` while it resolves, even for
  a crate that never uses it: the server image did not copy
  `third_party/`, and no image was published from 1.1.8 to 1.2.0
  (`failed to read /app/third_party/wasapi/Cargo.toml`). A new vendored
  crate or path dependency outside `crates/` gets a `COPY` line in both
  stages.
- **Off screen, a phone notifies instead of chiming** (`app::alerts`):
  a DM is a notification whose tap opens the conversation; the first
  voice into empty rooms is "X joined Y". Notifications exist only while
  the app is connected — nothing reaches a phone whose app is closed
  until push exists (PRODUCTION-PLAN.md PRD-1).

### Memory (the "RAM tripled" pass)
What the app retains is now measurable: **`/mem`** prints (and logs)
the process figure beside every bucket the app holds — chat rows
across conversations, picture previews (texture bytes from
`Image::size()`), compressed image bytes, GIF players, decoded
avatars, roster sizes, open viewers. Ask for that first next time;
the renderer's own caches and allocator slack are whatever is left
over. Already bounded before this pass: Skia runs on its raster
backend (the GPU build cost ~150 MB), chat previews are downscaled,
GIFs stream one frame, evicted rows drop their entries, the video
decoder recycles its RGBA buffer. Trimmed in this pass: the send
preview and the gallery kept their **full-resolution** decode in a
`State` image property after closing (a 4K screenshot = 32 MB until
the next one) — both clear it now; chat preview textures follow the
window's scale factor (`preview_max_edge`: 360² on a 1× display,
720² only at 2×, a quarter of the memory per picture for most
Windows users); avatars from other clients are clamped to 128² at
decode (ours already are at pick time). Since 1.1.9 the stream
viewer reuses a ring of three `SharedPixelBuffer`s per stream and the
decoders' buffers go round `video::pool` (ANDROID-SPEC D57), so the
8 MB allocation per 1080p frame is gone too. Left to measure: the `StyledText`
layouts of visible rows and Skia's glyph cache with color emoji,
both only visible from a heap profile.

### Network diagnosis (the Movistar / port-5000 case, 2026-09-09..15)
Three users on one ISP (Movistar fibre, 600/600) heard each other at
16 kbps in evening episodes; reconnecting "fixed" it; sometimes the
join hung. Others were fine. **Cause: the relay's UDP port.** On 5000
the ISP's gateway/path dropped ~40 % of the clients' *outbound*
packets in on/off episodes; on 51000 nothing. Fix in place: the
server listens on 51000 (`port` + `public_address` in `server.yaml`)
and Docker publishes both `51000:51000/udp` and `5000:51000/udp` so
old saved addresses still work — one socket, two doors; the invite
string already carries 51000. Rules that came out of it:

- **A shipped binary may reference system libraries only** (macOS:
  frameworks and `/usr/lib`; Linux: `/lib`, `/usr/lib`). The 1.1.5
  Intel macOS build aborted at launch for anyone without Homebrew's
  `libxcb`: FFmpeg's `./configure` auto-enables every optional
  dependency it finds on the runner and links it dynamically, and
  the post-build check only grepped for `libav*`. Fixed in
  `release.yml` (2026-09-18): `--disable-autodetect` on both static
  FFmpeg builds (only what is named — `x264`, `videotoolbox` — is
  compiled in), cache keys bumped so the tainted builds are not
  restored, and both verify steps fail on any non-system path. The
  user-side stopgap until the rebuilt binary is up is
  `brew install libxcb libx11`; it is not an instruction for the
  README. The second failure behind it, "Symbol not found:
  _AVCaptureDeviceTypeContinuityCamera" on Ventura, is the same
  class from the other side: no `MACOSX_DEPLOYMENT_TARGET`, so clang
  targeted the runner's Sequoia and hard-linked a macOS-14 symbol
  (FFmpeg's avfoundation capture device). Fixes: the job sets
  `MACOSX_DEPLOYMENT_TARGET=11.0` and the verify step checks the
  binary's `minos`; and `video` no longer links libavdevice at all
  (`ffmpeg-next` without the `device` feature; both static builds
  `--disable-avdevice`) — capture is xcap's, and libavdevice is where
  every capture-device dependency lived, on all three OSes. The
  deployment floor then broke the arm64 *link*: with the target below
  the SDK, FFmpeg's `__builtin_available` checks emit calls to
  `___isPlatformVersionAtLeast` (clang's compiler-rt), which rustc's
  `-nodefaultlibs` link never adds — `video/build.rs` now asks
  `clang -print-file-name=libclang_rt.osx.a` and links it on macOS.
  A local `cargo run` never sees any of this because without the
  variable the checks fold to constants: **a Mac dev build is not
  evidence about the release build; the CI link is.**
- **Read both loss counters before anything else.** The client's
  `link:` line (transport: our unacked packets) and the relay's
  `VoiceReceiverReport` (`uplink N%` in `audio stats`) are
  independent; when they agree the loss is real and outbound. The
  downlink figure (`Recv … Lost`) tells you the direction. Flat RTT +
  loss = a policer or classifier, not congestion. Bimodal loss
  (~0 % or ~40 %, nothing between) = a switch somewhere, not a dial.
- **`server telemetry <user>` is the same data at 10 s cadence** on
  the server side; the per-minute view shows episode boundaries, and
  boundaries that do not line up with reconnects mean reconnecting is
  not the fix.
- **Split "our packets" from "the path" with an unmarked control:**
  `ping -t <relay ip>` beside the client during an episode. Clean ping
  + baucord loss = something about our packets (port, DSCP, ECN,
  802.1p tag, protocol); ping loss too = the path.
- **Split "the PC" from "beyond it" with `pktmon`** (built into
  Windows): `pktmon filter add baucord -t UDP -p <port>`,
  `pktmon start -c`, then `pktmon counters --drop-reason`. Equal Tx
  counts at TCPIP, QoS Packet Scheduler, WFP and the NIC with no drop
  rows = nothing on the machine ate them; a third-party filter driver
  (Killer, cFosSpeed, AV suites) would show up as a component.
- Things checked and cleared, so nobody re-chases them: the audio
  ladder (`adapt.rs` did exactly what its inputs said); the Realtek
  802.1p tag (disabled, no change); CS7 DSCP from the qWAVE voice flow
  (still marked on 51000, no loss); ECN (same); upstream capacity;
  Discord's QoS toggle was off on those PCs, so it was never a
  like-for-like comparison.
- The server-side `link:` lines carry the *relay's* view and the
  source `addr:port`; `voice rx/expected/lost/late` there vs the
  client's `path loss` is the first cross-check next time.

The clients keep marking CS7 on Windows and ECT(0) everywhere; both
were exonerated and neither is changed.


### Memory on macOS/Linux (2026-09-18, open — PRODUCTION-PLAN.md PRD-6)
Windows shows ~40 MB at boot, macOS and Linux 100–200 MB, and a
viewed 1080p stream adds ~150 MB that Linux never gives back. The
renderer is the same everywhere (Skia raster), so it is not GPU
contexts. Two separate things: Task Manager's private working set
excludes file-backed and shared pages that RSS and Activity Monitor
count (the executable alone is 40–60 MB of text); and glibc keeps
freed 8 MB frame buffers in its arena once its mmap threshold has
risen — allocator retention, not a leak. Measure before touching
code: `/mem` at boot, while viewing and after stopping (the app's
own buckets vs the process figure); on Linux `smaps_rollup`
(`Anonymous` vs `Rss`) and the experiment
`MALLOC_MMAP_THRESHOLD_=131072 ./baucord`; on macOS
`vmmap --summary`. Expected fix: mimalloc as the global allocator in
the hosts (one line, returns large blocks eagerly, builds for
Android). Every stop-watching path does remove the `Viewer` (checked
in the 2026-10-10 review), and a disconnect now sends `StopWatching`
for each one it drains, so the decoder goes with it (review P0-6).

## 5. What is left

`PRODUCTION-PLAN.md` — the one prioritized list (hardening, licensing,
release integrity, the stores, product), the decisions waiting on the
user, and what was done. The diary of earlier sessions that stood here
is in the repository's git history (the user's clone; uploads leave
`.git` out); every decision in it is in the specs' decision logs.

**Waiting on the phones** (the next logs should show these; record the
outcome in the spec's decision log, then take the line out):

- iPhone: `safe area: top 20 … (status bar 20)` and the main page
  below the status bar (IOS-SPEC D46); in voice on the loudspeaker
  `VPIO voice processing on: 48000 Hz mono both ways, route Speaker`,
  the voice-chat mode in `audio session: mode`, `aec: platform
  (speakers)` and nobody hearing themselves — with headphones under
  Automatic, `aec: none (headphones)` (D47); a phone call in the
  middle, then `voice processing: interruption over; reopening`;
  `lifecycle: background` and `host: app hidden` on leaving the app,
  with voice carrying on (D42); whether the half-drawn glyphs come back
  under `run.sh --renderer skia-software` (D40); on a stream, mute and
  deafen from its bar, a pinch that follows the fingers (D44) and
  `screen kept on (watching a stream)` (D45).
- Android: `call routed to device N after M ms` (ANDROID-SPEC D51);
  typing a word and sending it without a space (D48); the service's
  type following the room — `microphone` in it, `specialUse` in the
  lobby — and a rejoin from the background falling back to the lobby's
  type until the app is shown (D63). The echo canceller itself is
  confirmed (`echo canceller on`, a Galaxy S24 FE, 2026-10-07; D46).
- Both: a DM with the app in the background → a notification with the
  dog icon, a tap opens the conversation; on silent, no chime with the
  app open; someone joining empty rooms → "X joined Y" (ANDROID-SPEC
  D52, IOS-SPEC D50); on iOS, the permission prompt on the first
  connect of a launch.
- Both: no microphone indicator in the lobby; in a room it goes off on
  mute (iOS: `VPIO microphone muted on the unit`) and comes back on
  unmute, and the first words after unmuting are heard (ANDROID-SPEC
  D53, IOS-SPEC D51); the battery over an hour in the lobby with the
  screen off.
- Both, on speakers: a watched stream is heard and the volume buttons
  change it (`Stream audio: playing through the voice output (call
  path)`), the others hear no stream audio through the phone's
  microphone, and headphones move it back (`back on its own output`;
  ANDROID-SPEC D56, IOS-SPEC D52).
- Both: a long press on a chat row opens its menu, and Block, Report
  and Delete my account work from there and from Settings → Profile
  (ANDROID-SPEC D64, IOS-SPEC D56).

## 6. Site screenshots

`web/` holds the site (`web.yaml` deploys it; its `img/` and `js/`
were not in the 2026-10-07 upload). Its screenshots were rendered from
the real UI at 2× with the viewer and fixtures (call, popover, prompt,
connect, settings-servers, settings-audio; `share.png` is the viewer
chrome composited over `call.png` with a drawn scene, since the viewer
cannot play a stream headless). After a UI change, re-render with the
commands in §1 and replace the files.
