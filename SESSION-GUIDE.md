# baucord — session guide

Read this first in a new chat. It says how the assistant works on
this codebase without a Rust toolchain, where things are, the rules
that were learned the hard way, what is shipped, and what is left.
The design record is `PERSISTENCE-SPEC.md` (the 1.1 line's contract,
with §12 holding every decision taken since); `ARCHITECTURE.md`
describes the runtime; `AEC-SPEC.md` is the echo-cancellation design
(1.1.5); `ANDROID-SPEC.md` is the Android client's plan. Both collect
later decisions in their last section the same way. Keep all five in
the repo root.

## 1. Setting up the assistant's environment

There is no `cargo` in the sandbox. Rust is compiled by the user; the
assistant edits files, compile-checks and renders every `.slint`
change, and hands over a tarball. Do this at the start of a session:

```sh
# the workspace: the user uploads crates.tar.gz (the crates/ folder)
mkdir -p /home/claude/work && cd /home/claude/work && tar -xzf /mnt/user-data/uploads/crates_tar.gz

# slint-viewer 1.17.1 (the app pins Slint 1.17.1 — match it)
mkdir -p /tmp/sv && cd /tmp/sv && curl -sSL -o sv.tar.gz \
  https://github.com/slint-ui/slint/releases/download/v1.17.1/slint-viewer-linux.tar.gz \
  && tar -xzf sv.tar.gz     # → /tmp/sv/slint-viewer/slint-viewer

# the icon library the UI imports as @lucide
mkdir -p /tmp/lucide && cd /tmp && curl -sSL -o lucide-slint.crate \
  https://static.crates.io/crates/lucide-slint/lucide-slint-1.34.0.crate \
  && tar -xzf lucide-slint.crate -C /tmp/lucide   # → /tmp/lucide/lucide-slint-1.34.0/lucide.slint

# Slint's own sources, for checking runtime behaviour instead of guessing
for c in i-slint-core i-slint-compiler i-slint-backend-winit; do
  curl -sSL -o /tmp/$c.crate https://static.crates.io/crates/$c/$c-1.17.1.crate
  mkdir -p /tmp/src-$c && tar -xzf /tmp/$c.crate -C /tmp/src-$c
done
```

Any crate the code depends on can be fetched the same way
(`https://static.crates.io/crates/<name>/<name>-<version>.crate`) to
read its real API before writing against it — done for `interprocess`,
`device_query`, `quinn-proto`. `crates.io/api/v1/crates/<name>` lists
versions (send a `User-Agent`).

### The loop

```sh
cd /home/claude/work/crates/ui/slint
V=/tmp/sv/slint-viewer/slint-viewer; L="-L lucide=/tmp/lucide/lucide-slint-1.34.0/lucide.slint"

$V --check $L main.slint                                   # every .slint change
$V --screenshot out.png --load-data props.json $L main.slint          # render the main window
$V --screenshot out.png --component SettingsWindow --load-data p.json $L settings.slint
$V --screenshot out.png --component AdminWindow --load-data p.json $L admin.slint
SLINT_SCALE_FACTOR=2 $V --screenshot out.png ...          # 2× for site screenshots
```

`--load-data` takes JSON: top-level keys are the window's own
properties (`tab`, `phase` lives in `State`) and globals by name
(`State`, `SettingsState`, `AdminState`, `Tr`). Rows of a model must
carry **every** field of their struct — a missing `color`/`image`
field is filled with unit and panics at render (`binding was of the
wrong type: ()`); a missing string field just warns and the model is
left empty. `Timer`s do not fire in a screenshot (the hover card, the
follow), and `image` properties cannot be set from JSON. The
site-screenshot fixtures (`site/*.json`, delivered next to
`web.tar.gz`; keep them in the site repo under `web/site/`) are a
good starting point for any fixture.

Rendered PNGs are read back with a small pure-Python decoder (no PIL
needed, though PIL is available) and cropped to the region of
interest before `view`ing — a full 1000×640 view costs tokens.

Delivery: `tar -czf /mnt/user-data/outputs/crates.tar.gz crates` from
`/home/claude/work`, plus the docs that changed, then `present_files`.
The user untars over the repo root and runs

```sh
cargo test -p protocol -p app-core -p server -p transport -p client
cargo clippy -p protocol -p transport -p server -p client -p desktop --all-targets
cargo run -p desktop        # and -- --profile bob for a second instance
```

and pastes the first `error:` with its `-->` location. One or two
compiler rounds per step is normal. Edit scripts must assert their
anchors (`assert old in s`): a silent miss once shipped a function
that did not exist, and once left a script's changes unwritten.

Style: edition 2024, Rust 1.95, `clippy::pedantic = warn`,
`clippy::all = deny`, rustfmt at 100 columns (`awk 'length > 100'`
is the check; pre-existing long lines in `store.rs`, `network.rs`
and `e2e.rs` are not ours). Markdown wraps at 80. Every new string
goes into all three Fluent catalogs (en, es-AR, pt-BR) **and** every
`Tr` property in `tr.slint` is filled in `fill_tr` — a checker for
catalog parity and unfilled `Tr` props is a ten-line Python script
worth re-running.

## 2. Map

- `protocol` — the wire. Positional postcard: **never** add a field
  in the middle of a shipped struct or enum, never reorder; append
  variants, and gate every appended variant on a **new** feature bit
  (`features.rs`; a client that negotiated an old bit cannot decode a
  variant added later under it). Permission bits and names in
  `permissions.rs` (`PERMISSION_NAMES`, `ROLE_ICONS`,
  `derived_rank`). Admin commands in `admin.rs`.
- `transport` — quinn-proto driver, lanes, media pacing.
- `server` — the tick in `server.rs`; child modules `moderation`,
  `history`, `channels`, `profiles`, `dms`, `files`, `admin`; the
  persistence thread in `persistence/` (`store.rs` is SQLite, the CLI
  helpers `cli_*` live there too); `main.rs` is the CLI; migrations
  in `persistence/migrations/` (0001–0003).
- `client` — network thread (`network.rs`), config, identity.
- `desktop` — the Slint app: `app.rs` (the host: `AppState`, all
  network handling, the slash commands), `settingsui.rs`,
  `adminui.rs`, `media.rs` (images/files/GIFs), `viewers.rs`,
  `hotkeys.rs` (watched keys), `instance.rs` (one per profile),
  `tray.rs`, `updater.rs`.
- `ui` — the `.slint` files and `convert` glue (`lib.rs`):
  `main.slint` (window, overlays, card, dialogs), `sidebar.slint`,
  `chat.slint`, `settings.slint`, `admin.slint`, `viewer.slint`,
  `widgets.slint`, `theme.slint`, `tr.slint`, `globals.slint`
  (`State`/`Logic` and every struct the host fills).
- `app-core` — pure view models with unit tests: `chat.rs`
  (`ChatHistory`, `ChatItem`, `FileMeta`), `roster.rs`, `menu.rs`,
  `markdown.rs`, `images.rs`, `emoji.rs`.
- `i18n` — Fluent bundles; `audio`, `video` — codecs and devices.
- `frontend` (1.1.6, versioned with the public crates) — the client's UI logic: `app.rs` (`AppState`,
  the event pump, the models), `settingsui`, `adminui`, `media`,
  `viewers`, `present`, and — until the platform trait replaces them
  (`ANDROID-SPEC.md` §5) — the host-only modules `hotkeys`,
  `clipboard`, `instance`, `updater`, `resources`, all behind the
  `desktop` feature (on by default). `desktop` is now `main.rs`
  alone: logging, the config directory, the single instance, the
  panic hook, `frontend::app::run()`. Its `tray` feature forwards to
  `frontend/tray`. Every `crate::` path inside the moved files still
  resolves: they moved together.
- Planned, not present: `android` (the cdylib host) and the Gradle
  project in `android/` beside `crates/`.

Server config keys the user runs with are in the spec §1; the
CLI is `server user|role|invite|restriction|key|telemetry|backup|
restore|db check|db prune|purge` (`server role --help` etc.).

## 3. Shipped

| version | what |
|---|---|
| 1.1.0 | persistence thread, schema, invites and device keys, four built-in roles, restrictions, history with the ring, channels from the UI, telemetry, progress, admin CLI |
| 1.1.1 | direct messages with offline delivery, unread counts, paging, the conversation list |
| 1.1.2 | profiles (name color/style, avatars, contrast check), the app's own context menus with the volume slider, collapsible Messages, invite dialog, XP bar (`UserProgress`), share sounds, one instance per profile |
| 1.1.3 | custom roles with derived ranks, badges (icon + color) and `RoleList`; `panel`, `gold` and `admin-channel` as permissions; files on the blob path in General and DMs (cards, transfer ring, cancel, Open after save); the admin panel (eight tabs, confirm dialogs, humanized audit); resizable sidebar and Messages divider; hover on chat rows |
| 1.1.4 | blob retention, purge drops files, chat xp, pictures as files wherever the server is persistent, transfer budget (`limits.max_transfer_kb_per_minute[_global]`, old names alias), `server restore` / `db prune`, the admin channel (`AuditLine` feed), keybinds watched not grabbed, command output in the conversation it was typed in, closed DMs stay closed |
| 1.1.5 | the lobby (connect invisible, `LOBBY` bit 16, implicit channel, "joined <room>" lines, reconnect-rejoin, no xp in lobby or AFK); relay port 51000 by default with the client following the official server's move; drag a user onto a channel; images get Copy/Delete and a gallery copy button; LIVE chip opens the viewer; gold badges, green admin marker; timestamps on system lines and channel-move lines; `delete-messages` permission; log-level setting; window centred instead of remembered; the DM avatar ring gone, "Admin logs" with a `#`; unchanged profiles no longer trip the rate limit |

Memory-mode servers (persistence off) still use the legacy
`ImageMessage` relay for pictures and refuse files.

## 4. Rules learned the hard way

- `wait_for` closures must not borrow the client being pumped: copy
  `x.id` first. And `wait_for` does not consume (see §1).
- **A new wire variant needs a new feature bit** (spec §12): the
  first cut gated `UserProgress` on `PROGRESS`, which a 1.1.0 client
  negotiates too — it would have choked on the variant. Now
  `PROGRESS_ALL` and `ROLE_LIST` (bits 11 and 12).
- `app-core`'s chat test helper multiplied a placeholder id near
  `u64::MAX` by 1000: fine in release, an overflow panic under
  `cargo test` (debug assertions). `saturating_mul` now.
- Tests about media connect people with `connect_member`; `connect`
  makes guests, who cannot share, upload, or DM.
- A CLI change reaches a running server within 2 s (`data_version`
  poll) and is applied to live sessions (bans close, kicks close,
  roles refresh).
- `/kick` forgets keys and roles (needs a new invite); `/ban` is the
  restriction; a ban never catches the actor or an equal rank, and
  skips an address someone else is on.
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
  `Timer` (`follow-ticks` ≤ 60) instead. Rule: no `changed` handler
  on a property the ListView rewrites while scrolling.
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
- A menu action lives in **three** places: `app-core::menu::Action`,
  the Slint `MenuAction` enum in `globals.slint` (append only — its
  ordinal crosses the bridge), and both conversions in
  `ui/src/lib.rs` (`menu_action`, `menu_action_back`). Miss one and
  only the compile says so, one round later.
- A render fixture's `image` fields (`avatar`, `viewer-image`) must
  point at a real file — `""` makes the whole model fail to load
  ("Failed to load image from path") — a 1×1 PNG written by the
  fixture script does; `/tmp/fx/main.json` is a full-field roster
  fixture (admin, sharer, guest, custom badge) worth regenerating.
- External drag-and-drop reaches the app only through
  `on_winit_window_event` (`WindowEvent::DroppedFile` and friends);
  Slint's `DropArea` is for in-app drags on this backend. The hook
  may be set once per window, so new event kinds join the existing
  closure in `repaint_on_reveal`.
- The DM ✕ does nothing because `dm-t` (the whole-row TouchArea) is
  declared after the layout holding the button and sits on top of it;
  the scrollbar collision is the `ScrollView`'s 14 px bar over a row
  with 4 px right padding.
- The chat scrollbar "sticks at the bottom": `changed viewport-height
  => scroll-to-end` while pinned; dragging the thumb near the bottom
  instantiates virtualized rows, the height refines, the pin snaps
  back. The fix is a host-driven follow (set on append) rather than a
  reaction to every geometry change.
- An XP bar needed a wire change: `UserDetails` carries `level`
  only, and `ProgressUpdate { xp, level }` goes to the user themself
  on level-up. `UserProgress` (spec §12) now carries everyone's xp.

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
decode (ours already are at pick time). Not done, and why: pooling
`SharedPixelBuffer`s for the stream viewer needs the buffer back
from a `slint::Image`, which the API does not give — the one 8 MB
allocation per 1080p frame stays. Left to measure: the `StyledText`
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


## 5. Remaining work

Everything in the spec except its "later" list is done. That list:
channel topics, reserved names, a manual away toggle shown in the
roster, animated avatars, anything built on progress (a leaderboard,
level-gated invites), `Auth::Account` only if a server ever wants
people rather than devices. Plus:

- **Hands-on passes the sandbox cannot do:** the watched keybinds on
  each OS (Wayland and macOS-without-accessibility fall back to
  grabs), the single-instance guard, the tray, `server restore` on a
  real backup, an image over 10 MiB (stays a card with Save).
- `ServerLimits` on the wire so the `+` can grey at the per-user
  channel cap.
- Scroll anchoring on "load earlier".
- Arrow-key navigation in the context menu.

From the port-5000 investigation (§4), none started:

- **`PreferredAddress`**: an appended `ServerMessage` under a new
  feature bit, sent after `Welcome` with `public_address`; the client
  rewrites the saved `ServerEntry.address` and uses it from the next
  connect (reconnects at once if its uplink shows the loss
  signature). Lets the `5000:51000/udp` mapping be dropped once the
  logs show nobody on it.
- **Default port 5000 → 51000** (done 2026-09-17): `ServerSettings`
  default, a startup warning for a server still on 5000, README. The
  client's official entry is `216.238.121.18:51000` and
  `normalize_servers` moves a saved `:5000` official entry to it on
  load (pin and passwords kept) — that is how the 5000 door empties.
  `PreferredAddress` (above) remains the general mechanism for other
  servers.
- **Ladder policy under flat-RTT loss** (`audio/src/adapt.rs`):
  `HEAVY_LOSS`/`SEVERE_LOSS` step down regardless of the RTT verdict,
  so a lossy-but-uncongested uplink ends at 16 kbps / 40 ms. Hold
  32 kbps / 20 ms and buy RED ×2 → ×3 instead, probe-guarded (loss
  rising after a copy is added = a policer, revert). Fixes what
  listeners hear on *any* lossy uplink.
- **ACK frequency for share viewers**: a viewer of a share sends ~500
  ACK packets/s upstream (quinn acks every second packet); quinn
  0.11's ack-frequency extension would cut it several-fold.
- **Telemetry**: audio rung, downlink voice loss and the marking
  state in `telemetry_samples`; `server telemetry` printing several
  users side by side over a window.

**Release plan (2026-09-15):** 1.1.5 = features on the current
structure; 1.1.6 = the `frontend` split with no behaviour change
(`ANDROID-SPEC.md` §5); then Android M0.

**1.1.6 — the split, round 1 (2026-09-18, awaiting its cargo
round):** the eleven modules moved from `desktop/src` to
`frontend/src` unchanged, `lib.rs` declares them `pub`, the manifest
carries desktop's dependencies (minus `env_logger`) and the `tray`
feature; `desktop/Cargo.toml` keeps `frontend`, `client`, `slint`,
`log`, `env_logger` and the `winresource` build script (its
`libx264` link directive still applies to the binary). The
workspace `Cargo.toml` must list the new member if `members` is
explicit. Round 2: gate the host-only modules on the `desktop`
feature so `cargo check -p frontend --no-default-features` passes;
round 3: split `app.rs` into modules inside the crate; then the
`Platform` trait as Android needs it.

**Tech-debt cycle, round 1 (2026-09-18):** the only clippy *error*
(`clone_on_copy` in `audio/src/stream/input.rs`) fixed — it had been
stopping clippy before `client`, `frontend`, `server`, `transport`,
`ui`, `desktop`, so their state is still unknown; the `libx264`
link directive moved from `desktop/build.rs` to `video/build.rs`
(where the ffmpeg dependency lives — `video`'s examples and tests
could not link on Windows, LNK2001 `x264_*`, which also aborted
`cargo build --all-targets` before the dead-code list existed);
unused dependencies dropped (`bytes` in `client` and `server` —
both use `protocol::Bytes`; `rustls-pki-types` in `transport` —
reached through `rustls::pki_types`). Next: the user runs
`cargo clippy --fix --workspace --all-targets --all-features
--allow-dirty` for the ~300 mechanical suggestions (290 are
`must_use_candidate` in `audio`/`video`), then fresh clippy and
build logs; the judgment items (casts, `match_same_arms`,
`unchecked_time_subtraction`, `float_cmp`, docs) go by hand. Rule
from the skill: fix, or `#[expect]` with a reason — never `allow`.

**Tech-debt cycle, round 2 (2026-09-18):** after the user's
`cargo clippy --fix` pass, 274 diagnostics remained, 4 of them
errors in `server` (the workspace denies `clippy::all`): a
`nonminimal_bool` in `channels.rs` (rewritten as `may_manage`), an
`if_same_then_else` in `profiles.rs::store_blob` (a pointless clone
in one branch), `large_enum_variant` on `persistence::Request`
(`#[expect]` with the reason — one channel message, nobody stores
it), and `too_many_arguments` on `VoiceRelay::process` — now takes a
`relay::Room<'_>` (Copy, all borrows) instead of nine arguments. Also
the one build warning (`i18n::init`'s return is now logged as
"Locale: …") and the five `{:?}`-formatted paths in
`client/config.rs`. **The tarball does not contain the user's
`--fix` edits** (must-use attributes and the like): after unpacking,
re-run `cargo clippy --fix --workspace --all-targets --all-features
--allow-dirty` — it is idempotent — then clippy again. What is left
is pedantic and splits into: ~140 numeric casts (proposal: allow the
`cast_*` lints in `[workspace.lints]` with a comment — DSP, video
and timing code casts by design), ~33 `needless_pass_by_value`
(mostly thread-entry arguments moved into closures; case by case),
23 missing `# Errors`/`# Panics` sections, 8 `too_many_lines`,
6 `similar_names`, and a handful of singles — one crate per round,
leaf crates first.

**The split, round 3 (2026-09-18, awaiting its cargo round):**
`app.rs` (8.6k lines) is now `app.rs` (2.4k: `AppState`, `run`,
`wire_logic`, the timers, the free helpers, `fill_tr`, `tray`) plus
ten **child modules** under `src/app/`, cut along method boundaries
of the one `impl AppState`: `events` (the drain, voice/screen events,
`handle_network_message`), `commands` (connect, slash commands, the
channel dialog, admin, invites), `presence` (`Init`, joins/leaves/moves,
channels, roles, ranks), `session` (join prompts, teardown, mute/
deafen, share toggles, sending), `menus` (context menus, moderation,
per-user audio, watching), `devices` (device changes, live settings,
`show_main`), `conversations` (history paging, General/DM switch,
DM rows), `chat` (pushing rows, pictures/files, edit/delete, GIFs),
`profiles` (row decoration, avatars, hover cards), `stats` (latency,
memory, toasts, update flags, shutdown). Each starts with
`use super::*;` (allowed `wildcard_imports`, with the reason) and its
own `impl AppState`; the 137 private methods became `pub(super)`
because a private item in a child is invisible to the parent and the
siblings — nothing became `pub`. Rule: **new `AppState` methods go
in the child that owns the concern; `app.rs` is for the state and
the loop.** Also: three stray doc lines before `ChannelFacts`
removed. Check: `cargo build -p desktop` — the likeliest complaint is
a `pub(super)` method whose signature names a type private to a
child (none should), or a method the regex missed at another indent.

**The split, round 2 (2026-09-18, awaiting its cargo round):** the
`desktop` feature now gates real things. Eleven crates are optional
behind it (`i-slint-backend-winit`, `arboard`, `global-hotkey`,
`device_query`, `interprocess`, `zip`, `self-replace`, `rfd`,
`pollster`, `open`, `objc2-app-kit`); `clipboard`, `hotkeys` and
`updater` have same-API twins under `src/stubs/` selected by
`#[path]` when the feature is off; `instance` is desktop-only; a new
`host` module (`pick_file`, `save_file`, `open_url`, `open_target`)
is the one seam for native dialogs and the system launcher — the
five `rfd` sites and three `open` sites now go through it — and it
too has a stub twin. In `app.rs` the winit-only pieces
(`raise_window`, `center_on_monitor`, `repaint_on_reveal`) have
no-op twins under `cfg(not(feature = "desktop"))`. `ureq`, `dirs`,
`sysinfo`, `video` stay unconditional: they build for Android. The
check that matters now:
`cargo check -p frontend --no-default-features` on the host (it
must not pull any of the eleven crates), then `cargo run -p desktop`
unchanged. Next: round 3 (`app.rs` into modules), then the
`Platform` trait replacing the stubs where Android needs a real
implementation (clipboard, open URL, dialogs).

**Tech-debt cycle, round 5 (2026-09-18) — e2e green (76/76), 105
pedantic warnings, 0 errors.** This round took the small crates and
`frontend`'s singles: `# Panics` sections on `i18n`'s three
lock-holding functions, two `StyledText` backticks, `hex()` built
with `write!` instead of a `format!`-collect, `MAX_AUTO_RECONNECTS`
hoisted, `arm_apply_timeout` made an associated fn, the `Volume` arm
folded into the wildcard with its comment, `text_muted` typed as
`Option<TextMute { until }>` (the other `Option<Option>`,
`history_page_pending`, `expect`ed: the inner option *is* the
conversation key used throughout), the updater's non-Unix stubs
`expect`ed (`?`-compatible signatures), one clone-then-assign
reordered. Left (~90): `needless_pass_by_value` 37 (`audio`
input/stream ctors, `client` thread entries, `server` handlers,
`video` dxgi), `too_many_lines` 25, missing `# Errors`/`# Panics` 29
(`audio` 13, `video` 7, `frontend` 5, `client` 2, `server` 1) — one
crate per round, and `audio` is half of it.

**e2e, third pass (2026-09-18):** two left. The harness's join echo
was still in the inbox (described, not implemented, the pass before
— now a `retain`). The other was a **real lobby gap**: a hot reload
that removes a room moved its members to the lobby by renumbering
`channel_of` directly, so neither their stream stopped nor did the
others see them leave. `apply_channel_list` now computes who was
demoted to the lobby and runs `withdraw` for each, before the
share-visibility re-derivation. The DM test passed alone: the race,
not a bug; noted, not fixed.

**e2e, second pass (2026-09-18):** five left after the harness fix,
four of them list-length predicates written for a list without the
lobby entry (`== 1` → 2, `== 2` → 3) and one stale `ChannelDetails`
— the lobby entry's id gets reused by a channel created after it, so
a predicate on `channel_id` alone finds the lobby's old details;
match `owner: Some(_)` for a user channel. The hot-reload share test
now asserts the new semantics: a removed room's members go to the
lobby, where the stream ends for the viewer and the room next door
never sees it. `a_file_goes_into_a_dm_like_a_message` failed with
`Store`, which the code returns both for a store error and for the
2-s simulated deadline passing before the real SQLite thread answers
— the harness advances simulated time a `TICK` per pump with no real
sleep, so this test is timing-sensitive; re-run alone with
`RUST_LOG=warn -- --nocapture` and look for "DM store failed" (a
bug) vs "did not answer in time" (the race).

**Tech-debt cycle, round 4 (2026-09-18):** with the cast lints
allowed, `clippy4` reached `frontend` for the first time and found
three *errors* there — orphaned `///` comments whose functions were
deleted long ago (`empty_line_after_doc_comments`), removed. 107
unique warnings remain: `needless_pass_by_value` 37, `too_many_lines`
23, `missing_errors_doc` 20, `missing_panics_doc` 12, and eleven
singles (`Option<Option<T>>` ×2, `unused_self`, `format!` in a
collect, an `unnecessary_wraps` pair, a `large_stack_arrays`, a
`clone` assignment, an `items_after_statements`, a `match_same_arms`,
two `doc_markdown`). Next rounds take them per crate.

**The e2e suite and the lobby (2026-09-18):** `cargo test -p server
--test e2e` had 19 failures, all one cause — the suite was written
for a world where connecting made you visible in channel 0, and
nobody ran it when the lobby landed (a rule now: **the server's
e2e suite runs before any presence change is called done**). Fix in
the harness: `connect_*` now joins room 0 after `Init` and waits for
its own `UserMovedChannel`, so the old tests keep meaning what they
say; `connect_lobby` stays invisible for the tests about the lobby
itself (one added: unseen until the first join, the join carries
the room, an admin's move to the lobby reads as `UserLeft`). Five
expectations updated for the implicit lobby: `validate_channels`
returns the lobby's index and `default: true` is not a rule
(`config_validation_rejects_bad_channel_lists`,
`hot_reload_rejects_invalid_lists`), `Init` carries one extra
unnamed entry, a renamed channel's members go to the lobby (last),
a `ChannelKick` sends its target to the lobby and the others see
`UserLeft`.

**Tech-debt cycle, round 3 (2026-09-18, on the user's post-`--fix`
tree):** the two remaining errors (both in `server/tests/e2e.rs`: a
redundant field name; `post_file`'s eight arguments, `#[expect]`ed
as a one-caller test helper) and the small pedantic items by hand:
unbalanced backticks in `app-core::chat` docs, `many_single_char_names`
on the HSL formula (`expect`, named), `match_wildcard` in the message
menu (the last kind named), `needless_continue`, `needless_for_each`
(`fill`), four `manual_let_else`, five `match_same_arms` (merged with
their comments kept; `UploadBegin`/`FetchBlob` joined the Signal-lane
arm), four `float_cmp` in tests (`expect`, exact by construction), two
`items_after_statements` (consts hoisted), three `struct_excessive_bools`
(`expect`, reasons given), the `_device` binding that was in use. Plus
a **workspace lint decision**: the four `cast_*` pedantic lints and
`similar_names` are allowed with reasons (`workspace-lints.toml` has
the exact table for the root `Cargo.toml`, which is not in the
archive) — that removes ~160 of the 272. Left for round 4, one crate
per round: 33 `needless_pass_by_value` (thread-entry arguments — `&`
where the callee only reads, `expect` where the value is moved into a
closure), 30 missing `# Errors`/`# Panics` sections, 18
`too_many_lines` (dispatch functions and tests — `expect` with a
reason, or a split where one is obvious).

**1.1.6 — drop files from the OS (2026-09-18, awaiting its cargo
round):** Slint 1.17's `DropArea` never sees external drops on the
winit backend, but the raw-event hook the app already has for
repaint-on-reveal does: `WindowEvent::HoveredFile` /
`HoveredFileCancelled` / `DroppedFile` drive `State.drop-hover` (a
whole-window "Drop to send" overlay with no TouchArea, so a stale
hover still lets clicks through) and `media::drop_path`, which is
the paste chain's road: a picture takes the preview, anything else
uploads as a card. Several files arrive as several drops. Everything
is deferred with `invoke_from_event_loop` — the hook runs inside
winit's dispatch. Wayland delivers no file drags in winit 0.30 (the
overlay simply never shows there).

**1.1.6 bug fixes (2026-09-18):** drag-to-move
payload keyed on the row's client id (the index-keyed binding went
stale after a reshuffle); own pictures and files deletable without
`delete-messages` (`row_delete` only knew text/code rows, and its
moderator check still read `RESTRICT`); gold badges no longer save
as grey (`badge_palette()` is the one list for swatches, edit form
and save — the save mapped the index into the 16-entry name list).

**Versions bumped to 1.1.5 on 2026-09-17** (desktop, server, protocol,
transport, client; app-core 0.3.4, ui 0.4.1). Wire: one new feature
bit (`LOBBY`, 16), one new permission bit (`DELETE_MESSAGES`, 23),
no new message variants — a 1.1.4 client on a 1.1.5 server works
(it renders its own lobby row empty).

**1.1.5 scope** — batch 1 written 2026-09-16 (image menu with Copy
image + Delete and a Copy button in the gallery, LIVE chip opens or
raises the viewer, gold in the badge palette, `Theme.admin` green for
every admin marker; `slint-viewer --check` clean on all three roots,
sidebar and gallery rendered). Cargo rounds 1–2 done (the bridge's
`MenuAction`, then our own LIVE chip being clickable); image delete,
copy and single-click watch confirmed working in QA:

- AEC on Windows per `AEC-SPEC.md` (Communications category when
  the output is speakers, verified tier in `/audio`, the setting).
- **Log level** picker in Settings → General (`logging.level`,
  applied live). *(done 2026-09-17: `LoggingSettings`, `LOG_LEVELS`
  in settingsui, env_logger filter at trace with
  `log::set_max_level` as the knob, `RUST_LOG` wins; the settings
  window grew to 800×740 preferred / 680×580 min — render it in the
  app, the viewer shows a Window with only preferred sizes as 6×23.)*
- **`delete-messages` permission** (done 2026-09-17): bit 23, own
  name, in the admin set; the server's `DeleteMessage` needs it and
  `RESTRICT` no longer implies it; client `delete_any`/`perm-delete-
  any` follow it. Existing custom roles need the box ticked.
- **Lobby banner** now reads "Join a voice channel to start
  talking" (three catalogs).
- **Drag a user to a channel** (`DragArea` wrapping each row,
  `DropArea` in front of channel rows; `AdminCommand::MoveUser`,
  which needs `MANAGE_CHANNELS` — the server already enforces it).
  *(batch 2, written 2026-09-17, awaiting its cargo round: the
  channel row tints `accent` at 35 % while an accepted drag hovers;
  dropping onto the user's current channel is refused client-side.)*
- **Invisible lobby**: the default channel becomes a waiting room —
  no roster presence for others, no voice relayed, no xp; hidden
  from the channel list behind feature bit `LOBBY` (16); the user is
  announced when they join a channel ("X joined <channel>"). No xp
  in the AFK channel either. Posting from the lobby allowed, no
  "step out" action, automatic reconnect rejoins the channel. The
  full design is `PERSISTENCE-SPEC.md` §12 (2026-09-16). *(batch 3,
  server round written 2026-09-17: `features::LOBBY`, `in_lobby`,
  `announce`/`withdraw` on the `place_in_channel` transitions,
  filtered `Init`, `UserLeft` only for the announced, relay and xp
  gates, AFK sweep skips the lobby, no shares seen or started from
  it. Client round written 2026-09-17: `NetworkMessage::Connected
  { lobby }` carries the bit; `lobby_name` is the channel `Init` put
  us in, hidden via `ChannelMeta.hidden`; `State.in-lobby` drives
  the banner above the roster and the greyed mic; `UserJoined`
  renders `chat-user-joined-channel`; `rejoin_channel` is set on a
  connection loss and spent by the next `Init` (a deliberate connect
  or disconnect clears it). Round 2 (2026-09-17, after QA of round
  1): the lobby became **implicit** — `config::with_lobby` appends an
  empty-named entry, `validate_channel_list` returns its index and
  only warns about `default: true`; `ChannelListUpdated` memberships
  are filtered per recipient like `Init`; the admin channels tab
  skips it. Also: system lines carry the local time
  (`push_chat_system`), channel moves get lines (`chat-user-moved`,
  `chat-you-moved`; our own move into the lobby is silent), the DM
  avatar ring is gone, the admin channel is "Admin logs" with a `#`.
  README updated. Awaiting the cargo round.)*
- **Window position no longer saved** (2026-09-17): `WindowSettings`
  keeps only the size; the first show centres the window on its
  monitor (`center_on_monitor`, winit physical pixels), later shows
  leave it alone. Old `x`/`y` keys in `settings.json` are ignored.
  Users were losing the window on a monitor that was gone.
- **"One profile change every 30 seconds" on relogin** (fixed
  2026-09-17): the client re-sends its profile after every `Session`
  and the server stamped `profile_changed_at` (keyed by user, kept
  across sessions) for every accepted `SetProfile`, identical or not.
  `handle_set_profile` now treats an unchanged profile as a no-op
  before the lock and rate checks — no apply, no broadcast, no stamp.
- **Image messages get Copy image and Delete** (`app-core::menu`
  returned early for images; the server already allowed it).
  *(batch 1)*
- **Click the LIVE chip** opens the viewer (same path as the context
  menu's "Watch stream"). *(batch 1)*
- **Badge colours**: gold allowed in the role-badge palette; the
  built-in admin badge turns green. *(batch 1)*

**Android** (v1: voice + chat, sideloaded, Slint) — `ANDROID-SPEC.md`,
milestones in its §12; M0 (toolchain + hello) follows 1.1.6.

## 6. Rough edges (known, not urgent)

- No scroll anchoring on "load earlier": the list stays at the top,
  showing the oldest of the new page.
- The chat's horizontal scrollbar is left at `as-needed` on purpose:
  with nothing to scroll it never shows, and if a future row inflates
  the list again the 2 px thumb above the composer is the canary.
- The `+` (new channel) cannot grey at the per-user cap: the cap is
  not on the wire (a `ServerLimits` message would fix it).
- DM unread clears when the DM is open even if the window is in the
  background (no focus tracking).
- Dialog `Field`s have no explicit select-all (no FocusScope); add
  one if ⌘A turns out flaky there.
- `profile_locked` on the user row is unused (the lock is the
  restriction). `chat-dm-received` and `nickname-applies-later`
  strings are unused.
- The context menu has no arrow-key navigation yet (Escape and click
  outside only); the native one had it. The hover cursor over links
  stays blocked under a `StyledText` (the row highlight works: the
  row's TouchArea is the content's parent).
- `Tr` properties nothing reads: `ptt-tooltip`, `audio-devices-header`,
  `audio-system-default`, `keybinds-header`, `keybind-record` (older
  leftovers, left alone).


## 7. Site screenshots

`web.tar.gz` holds the site; its `img/*.png` were rendered from the
real UI at 2× with the fixtures in `site/*.json` (call, popover,
prompt, connect, settings-servers, settings-audio; `share.png` is the
viewer chrome composited over `call.png` with a drawn scene, since the
viewer cannot play a stream headless). After a UI change, re-render
with the commands in §1 and replace the files.
