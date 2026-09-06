# baucord 1.1 — handoff

Read with `PERSISTENCE-SPEC.md` (the design; §11 status, §12 decisions)
and `ARCHITECTURE.md` (how the code is laid out). This note is the
rest: how the work happens, where it stands, what was learned, what is
next. It was rewritten at the end of the session that built the 1.1
client UI, profiles and DMs; the previous version's history is folded
into §2 and §3.

## 1. How we work

- The assistant has no Rust toolchain. It edits files in the workspace
  and delivers a tarball (`crates.tar.gz`, plus `ARCHITECTURE.md` and
  `PERSISTENCE-SPEC.md` when they changed); the user untars over the
  repo root and runs
  `cargo test -p protocol -p server -p transport -p client`,
  `cargo clippy -p protocol -p transport -p server -p client -p desktop --all-targets`,
  `cargo run -p desktop`, then pastes errors. A step usually takes one
  or two compiler rounds; that is expected, not a failure. Paste the
  first `error:` line with its `-->` location; the note chain under it
  is not needed. Empty attachments happen — paste text instead.
- The assistant's edit scripts must assert their anchors (`assert old
  in s`): a silent miss once shipped a function that did not exist.
- Workspace: edition 2024, Rust 1.95, `clippy::pedantic = warn`,
  `clippy::all = deny`; rustfmt at 100 columns (the assistant wraps
  by hand, `awk 'length > 100'` is its check). Every server behaviour
  is an `e2e.rs` scenario on the simulator (`World`, `TestClient`,
  fake `Instant`, `advance_wall`), with a real SQLite file in a temp
  dir when persistence matters. 68 e2e tests.
- Harness rules: `test_config` sets `limits.chat.burst = 1000` so
  scenarios can send a dozen lines in a few simulated ms (only
  `chat_is_rate_limited_per_session` lowers it); images and blobs go
  through `TestClient::send_image` / `upload_blob` (the Media lane is
  not a `send()` lane); the channel sweep runs every 30 s, so
  idle/temporary scenarios settle past a sweep boundary; **`wait_for`
  does not consume the inbox** — a second verdict in one test must
  clear or `retain` earlier answers first (see `set_profile`); writes
  and reads reach the persistence thread on different channels, so a
  read right after a write can race it — tests wait for
  `persistence().stats().queued == 0`.
- Crates: `protocol` (wire, positional postcard — never add a field to
  a shipped struct or enum in the middle; append variants behind a
  feature bit), `transport`, `server` (tick in `server.rs`;
  `moderation` / `history` / `channels` / `profiles` / `dms` as child
  modules; persistence thread in `persistence/`), `client` (network
  thread, config, identity), `desktop` (Slint app: `app.rs`,
  `settingsui.rs`, `media.rs`, `viewers.rs`), `ui` (`.slint`, the
  `convert` glue), `i18n` (Fluent: en, es-AR, pt-BR — every new string
  goes in all three, and every `Tr` property in `tr.slint` is filled
  in `fill_tr`), `audio`, `video`, `app-core` (pure view models,
  tested).
- Two desktop instances on one machine: `cargo run -p desktop --
  --profile bob` (or `--config-dir DIR`, or `BAUCORD_CONFIG_DIR`);
  each profile has its own settings, identity key, avatar and cache.
- Slash commands stay ("one authorization path, two front ends"):
  every UI action sends the same `NetworkControl` a command does.

## 2. Where it stands

Everything below is green (tests) and was tried by hand on two
clients unless marked.

### Server (spec steps A–E, plus profiles and DMs)
- 1.1.0 admission, invites, roles, restrictions, history, progress,
  channels from the UI — complete, with e2e scenarios.
- **Profiles (`server/profiles.rs`, spec §7.1, amended in §12):**
  `Profile { name_color, name_style, avatar }` on the user row;
  `SetProfile` validated (3:1 contrast on `#1a1d21`, the gold band —
  HSV hue 38°–65°, sat ≥ 0.35 — refused without `ADMIN`, one change
  per 30 s, the `profile` restriction as an admin lock via
  `ResetProfile { lock }`); `UserProfile` after each `UserDetails`
  (batch after `Init`), `ProfileUpdated` on change; `SetNickname` →
  `UserRenamed` with the `Hello` name rules. All behind `PROFILES`.
- **Blobs (spec §7, avatar-only for now, `BLOBS`):**
  `UploadBegin/Accept/Chunk/Done/Rejected`, `FetchBlob/BlobChunk/
  BlobMissing`; dedup by hash; chunks through `fold_upload`; sha and
  size verified, PNG/JPEG/WebP sniffed; `policy.blobs` + `blob_bytes`
  in memory, `blobs/<sha256>` + the `blobs` table with persistence;
  reads through `Query::Blob` with waiters coalesced per hash;
  reference counts follow profiles. `persistence.avatars.max_kb`.
- **DMs (`server/dms.rs`, spec §6, `DM`):** both ends need `DM`
  (guests refused), the `dm` restriction refuses, the chat rate limit
  applies; stored first with a store (`Query::StoreDm` on the ask
  path, 2 s deadline, `MessageAck` + `Direct` only on
  `Answer::Stored`, `MessageFailed { Store }` otherwise); live only
  without one (`Offline`); `Conversations` after the general replay
  on join; `MarkRead` moves the mark (`message_reads`, forward only);
  `FetchHistory { conv: Dm(user) }` pages through the store
  (`ConvKey`).

### Desktop / UI
- **Roster:** rounded-square avatar tiles (`widgets::AvatarTile`,
  picture or tinted initial), a ring for voice; role glyphs (guest
  dashed ring, organizer wrench, admin gold shield); a window-level
  **profile card** on hover (roster rows and chat pictures) with
  level, role and, for `MANAGE_USERS`, the last telemetry; the row
  menu is a `ContextMenuArea` with audio items, "Message…" and the
  moderation entries gated by `State.perm-*` and the server's rank
  rule mirrored per row; kick/ban/mute/disconnect confirm in a
  dialog; channel headers offer Edit/Delete when `may_manage` holds.
- **Chat:** Slack-shaped cozy rows (52 px gutter with a 36 px picture
  on the first message of a run, header with name in the profile's
  color/style + time, bodies below; a run = same author within 5 min,
  no system line or divider between) or compact one-liners; day
  dividers (Today / Yesterday / localized weekday-month-day, year
  when not this one) on every push and inside loaded pages; edit and
  delete from the menu (own; `RESTRICT` deletes any) with in-place
  rendering and a 3 s "the server didn't apply it" toast; "load
  earlier" at the top; the memory-mode banner; styled rows switch to
  a selectable plain view; typing on any row goes to the composer;
  ⌘A / Ctrl+A select all, macOS Ctrl+A/E, ⌘←→↑↓ handled explicitly.
  The pin-to-bottom rule: exact bottom pins, any upward move unpins,
  a downward arrival within 48 px pins; sending and switching
  conversations pin (`ChatPane.pin-to-bottom`).
- **DMs:** the sidebar's lower half is **Messages** — a General row
  with its own unread badge, then DM partners newest first with a
  16 px picture (ring = online) and an unread badge; open one from
  the list, "Message…" in the roster menu, or `/dm <name> <text>`.
  The chat pane holds one conversation at a time (`history`,
  `chat_model`, `oldest_history_id`, `history_more`); the others are
  `parked` and `swap_in` exchanges them; `with_conversation(key, f)`
  runs `f` on a parked one silently — **every general-chat push goes
  through it to General**, so a DM being read never swallows the
  room. A DM header (name, back, "Direct message" or the memory-mode
  note), composer placeholder "Message <name>", optimistic send
  (pending row under `u64::MAX - client_ref`, confirmed by
  `MessageAck`, discarded with a toast on `MessageFailed`), page
  fetched once per conversation, `MarkRead` on open and on live
  arrival while open, the DM chime. DMs are immutable in 1.1.1 and
  refuse images.
- **Profiles on the client:** the Profile tab (nickname — renames
  live while connected; color palette measured to read on the chat
  background, gold only for admins; name style; picture with a ✕ on
  its corner; identity block with a two-click Regenerate); edits go
  out as one `SetProfile` on Save & close (`profile_dirty`); other
  people's pictures cached at `avatars/<sha256>` next to
  `settings.json`; `apply_profile_look` prefers the server's copy.
- **Settings:** Profile / General / Audio / Keyboard / Servers /
  Credits / About with glyph nav, `PanelHeader` + `Setting` grammar,
  560 px content column. General: language, text size
  (`Theme.scale` on every window), Startup (auto-connect, start in
  the tray when compiled, check updates), Sounds (master switch;
  join/leave, DM on by default, mention off; ordinary messages never
  sound), Chat (timestamps, compact rows, image budget), Files,
  Privacy (what a server sees). Audio: Transmission = voice activity
  / push to talk — PTT is real (hotkey edges, `effective_mute`,
  mic button lights while held).
- **Viewer:** the picture fills the window; a floating name chip and
  control bar fade after 2.5 s of pointer rest (speaker = mute
  toggle keeping the level, volume, fixed-width stats, fullscreen —
  also double-click / F / Esc — Stop watching); controls centered in
  full-height slots.
- **Polish:** `text-muted` #929598 and `accent-text` #4A9AE0 (both
  measured for AA), 11/12 px type floor, `ToastKind` (info / success
  / warning / error, click to dismiss), "Join channel" and other
  verb-first buttons, `CheckRow`, `IconBtn.tooltip-below`.
- **Repaint workaround** (`repaint_on_reveal`) now also flips on
  `Moved`, `Resized`, `ScaleFactorChanged`, on all three windows —
  the "white parts after moving or maximizing" hypothesis; not yet
  confirmed on a machine that showed it.

### Not tried by hand (no macOS / Windows / tray here)
The macOS composer shortcuts, push-to-talk with a bound key, "start
in the tray", the move/maximize repaint fix.

## 3. Things learned the hard way

- `wait_for` closures must not borrow the client being pumped: copy
  `x.id` first. And `wait_for` does not consume (see §1).
- Tests about media connect people with `connect_member`; `connect`
  makes guests, who cannot share, upload, or DM.
- A CLI change reaches a running server within 2 s (`data_version`
  poll) and is applied to live sessions (bans close, kicks close,
  roles refresh).
- `/kick` forgets keys and roles (needs a new invite); `/ban` is the
  restriction; a ban never catches the actor or an equal rank, and
  skips an address someone else is on.
- Chat earns no xp; an image counts as a chat message for mutes and
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
- Only text chat enters the server's edit/delete ring (200 most
  recent); images cannot be edited or deleted; the server is
  *silent* when it refuses an edit or delete — the client's 3 s
  timer is the only feedback.
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

## 4. Rough edges (known, not urgent)

- No scroll anchoring on "load earlier": the list stays at the top,
  showing the oldest of the new page.
- The `+` (new channel) cannot grey at the per-user cap: the cap is
  not on the wire (a `ServerLimits` message would fix it).
- DM unread clears when the DM is open even if the window is in the
  background (no focus tracking).
- Dialog `Field`s have no explicit select-all (no FocusScope); add
  one if ⌘A turns out flaky there.
- `profile_locked` on the user row is unused (the lock is the
  restriction). `chat-dm-received` and `nickname-applies-later`
  strings are unused.
- The roster's Role submenu sends a single built-in role, so a
  custom role would be dropped — fixed by `server role` work.
- Version numbers still read 1.1.0 while the wire carries profiles,
  blobs and DMs.

## 5. Next steps

1. **Release prep:** pick the version (1.1.1 with profiles, or
   renumber) and set it everywhere the updater reads it; the hands-on
   pass in §2's "not tried" list.
2. **Three small server items:** `server role` (custom roles — the
   table and `SetRoles` accept them, nothing creates them), `server
   restore`, `server db prune`.
3. **1.1.2, second half:** files as cards, daily quotas, the 20 GB cap
   and eviction, images migrating onto the blob path, pictures in
   DMs — the blob messages and store are in place.
4. **1.1.3:** the admin panel, the admin channel with slash commands,
   the audit stream over the wire.
5. Then the "later" list in spec §11 (channel topics, reserved names,
   a manual away toggle, animated avatars, progress features).
