# baucord 1.1 — handoff

Read with `PERSISTENCE-SPEC.md` (the design; §11 status, §12 decisions)
and `ARCHITECTURE.md` (how the code is laid out). This note is the
rest: how the work happens, where it stands, what was learned, what is
next. It was rewritten at the end of the session that built the 1.1
client UI, profiles and DMs, and amended by the session that replaced
the native context menus (§2 "Context menus", §3, §5); the earlier
history is folded into §2 and §3.

## 1. How we work

- The assistant has no Rust toolchain, but it can fetch the prebuilt
  `slint-viewer` (GitHub release `slint-viewer-linux.tar.gz`, 1.17.1)
  and the `lucide-slint` crate, so every `.slint` change is
  compile-checked (`slint-viewer --check -L lucide=… main.slint`) and
  rendered (`--screenshot out.png --load-data props.json`, with
  `State`/`Tr` globals set from JSON) before it is handed over. It
  also fetches Slint's own sources from crates.io to verify runtime
  behaviour instead of guessing (see §3). Rust still goes through the
  user's cargo rounds: the assistant edits files in the workspace and
  delivers a tarball (`crates.tar.gz`, plus `ARCHITECTURE.md` and
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
- **Context menus (this session):** drawn by the app, not the
  toolkit — `ContextMenuArea` is native (muda) on macOS/Windows with
  the winit backend and cannot be restyled or switched off (§3). The
  pieces: `app-core::menu` builds a menu as data (`Entry { Label,
  Action, enabled, separator, danger, submenu }`) from facts —
  `user_menu(Session, UserFacts, channels)`, `channel_menu`,
  `message_menu(MessageFacts)`, `composer_menu` — mirroring exactly
  the gating the `.slint` used to do, with 16 unit tests;
  `ui::convert::menu_entries` flattens it into `State.menu-entries`
  + `State.menu-submenus` (`[[MenuEntry]]`, referenced by index) and
  `menu_action` / `menu_action_back` map `Action` ↔ the Slint
  `MenuAction` enum + `arg`; `widgets::MenuPanel` draws one column
  (180–320 px, 30 px rows, chevron for submenus, `Theme.danger-text`
  for destructive entries — measured 5.0:1, the raspberry fill is
  2.9:1 as text); the overlay in `main.slint` (`menu-layer`) places
  the column inside the window, opens the submenu column beside the
  hovered entry (right, or left when out of room), closes on Escape
  or any click outside (consumed, like a native menu). The host:
  rows call `Logic.user-context(index, x, y)` / `row-context(row,
  selecting, x, y)` / `composer-context(x, y)` with window
  coordinates from the click; `open_user_menu` / `open_row_menu` /
  `show_menu` fill the state and remember a `MenuTarget` (roster
  index, channel id, message `{row_id, msg_id, body}`, composer);
  `menu_activated` resolves the pick against it and calls the same
  methods the slash commands do. Labels are `t!()` in `menu_label`;
  the `Tr.menu-*` strings that only the old menus read are gone from
  `tr.slint` and `fill_tr`. `selecting-row` (the styled row shown as
  selectable plain text) moved from `ChatPane` to `State` so the host
  can toggle it. **The volume is a slider row inside the menu**
  (`MenuAction.volume` draws as `HSlider` + percentage, bound to
  `State.menu-volume`; every move goes to the audio engine through
  `Logic.menu-volume-changed` → `menu_volume(v, false)` so it is
  heard at once, the release → `menu_volume(v, true)` →
  `handle_set_user_volume` persists it and refreshes the badge; the
  menu stays open) — the per-user popover overlay, `user-popup-open`,
  `popup-*` and `open_user_popup` are gone.
- **Sidebar sections:** a **Voice** header (`Tr.voice-header`, the
  user count muted beside it, the `+` after) above the channel list,
  and the **Messages** header folds the DM list away
  (`settings.ui.messages_collapsed`, pushed as
  `State.messages-collapsed`, flipped by `Logic.messages-toggle` →
  `toggle_messages`): the whole header row is the target, an `IconBtn`
  chevron (down = fold, up = open) says which, and while folded the
  unread total (DMs, plus General's when a DM is open) sits on the
  header as a badge so nothing goes unseen. The title is the
  **server's name** (`State.server-name`, set from `Init`; the
  greeting in chat no longer repeats it), with two chips on the row
  below — "New channel +" and **"Invite users"**
  (`State.perm-create-invites` ← `CREATE_INVITES`;
  `Logic.invite-create` → `handle_invite_command("")`, the `/invite`
  path: the code lands in chat and on the clipboard) — then the
  channels; the user count moved to the General header's second
  line. The DM rows'
  **✕ works**: the row's one TouchArea sits under the content and
  decides by `mouse-x` (the rightmost 28 px close, the rest open),
  the ✕ is a pure affordance lit by the same test, and every row in
  that list keeps 16 px clear of the `ScrollView`'s bar.
- **Roster:** rounded-square avatar tiles (`widgets::AvatarTile`,
  picture or tinted initial), centred in the row (a fixed-height
  layout child sits at the top otherwise — wrapped and centred by
  hand, as the DM rows already were), a ring for voice; role glyphs
  (guest dashed ring, organizer wrench, admin gold shield); a
  window-level **profile card** on hover (roster rows and chat
  pictures) with level, role and, for `MANAGE_USERS`, the last
  telemetry; the row menu (above) has audio items, "Message…" and the
  moderation entries gated by `State.perm-*` and the server's rank
  rule mirrored per row; kick/ban/mute/disconnect confirm in a
  dialog; channel headers offer Edit/Delete when `may_manage` holds
  — and now actually open on right-click (§3).
- **XP bar on the card:** `ServerMessage::UserProgress { client_id,
  xp, level }` (spec §12; `user_progress` in `moderation.rs`, sent
  with the `Init` batch, the join broadcast and the minute flush in
  `history.rs`; e2e `everyone_sees_everyones_progress`) →
  `NetworkMessage::UserProgress` → `UserState.xp: Option<u64>` →
  `RosterRow.xp` → `UserRow.xp-pct` / `xp-line` via `xp_progress`
  (`protocol::xp_for_level` / `cost_of_level`; the caption is
  `card-xp-line`, "128 / 250 xp to level 4"). The card draws a 6 px
  accent bar under the level line once the server has said; our own
  `ProgressUpdate` refreshes our row too. The card's second line is
  now "Level 5 · 🔧 Organizer" (`level-label` + `role-label`, the
  role glyph and name in its color: gold admin, accent organizer,
  muted guest); the old bottom role line is gone. **Guest card:**
  hovering a chat author who has since left opens the same card
  from `State.card-guest` (`guest_card_row`: name, look from the
  profile cache, "Offline" where the level goes; `card-key` -2 in
  main.slint drives the same dwell timer).
- **Invite dialog (spec §3.1):** "Invite users" opens a modal — role
  (guest/member/organizer), expiry (1 h / 1 day / 7 d / 30 d; a note
  when longer than a day without `MANAGE_INVITES`, which the server
  caps), uses (1 / 5 / 10 / unlimited) — then shows the minted code
  big and monospace with a copy affordance and the server's summary
  (`invite_dialog_create` sends the same `CreateInvite` the slash
  command would; `InviteCreated` / `InviteDenied` land in the dialog
  while it is open, in chat otherwise; the code also goes to the
  clipboard on mint, and "Copy code" toasts).
- **Greeting** lives in the General header (`State.greeting`, from
  `Init`) between the title and the user count; nothing about the
  server prints in chat on login any more.
- **Rename follows the history:** `ChatHistory::rename_author(old,
  new)` (tested) rewrites every row's author in every conversation,
  and `rename_author` on the host rebuilds those rows, so a live
  nickname change never leaves two names for one person.
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
  conversations pin (`ChatPane.pin-to-bottom`). **Following is owed,
  not reactive:** the host calls `chat-follow-appended` after
  appending rows (`sync_history_tail`), which sets `follow-pending`
  only if the reader was at the bottom; a bounded 16 ms timer nudges
  to the end while that is owed, and the flag clears on reaching the
  exact bottom or the moment the reader scrolls up. Window resizes still
  follow while pinned. The
  old "scroll on every height change while pinned" fought a
  scrollbar drag (§3). **General has a header** like the DM one (`#`,
  "General", `Tr.general-subtitle`), the composer keeps `gap-sm` of
  extra room above it, and the **list delegate hosts one row kind
  through `if`** rather than five instances gated by `visible` — the
  horizontal scroll's cause (§3).
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
- **Settings:** Profile / General / Audio / **Sounds** / Keyboard /
  Servers / Credits / About with glyph nav (`settingsui::TAB_*`
  mirrors the indices — Servers is 5 now), `PanelHeader` + `Setting`
  grammar, 560 px content column. General: language, text size
  (`Theme.scale` on every window), Startup (auto-connect, start in
  the tray when compiled, check updates), Chat (timestamps, compact
  rows, image budget), Files, Privacy (what a server sees). Sounds
  (its own tab, `alignment: start` so a short panel stacks at the
  top): master switch; join/leave, **share started** (someone else in
  our channel starts sharing — `ScreenShareStarted` gated on
  `our_channel()`), **viewer joined** (softer, to the sharer: the
  count only reaches the app through the screen thread's
  `StreamHealth` sample, so `update_stream_stats` plays it on a rise
  of `stream_viewers` over `stream_viewers_seen`, reset when sharing
  stops), DM on by default, mention off; ordinary messages never
  sound. the sharer hears share-started too (they are live);
  `SoundType::ShareStarted` / `ViewerJoined` are
  `synth_notes` (A-major rise at 0.30; a short high pair at 0.16),
  `SoundSettings.share_started` / `viewer_joined` with `_on()`
  gates. The Sounds tab is a global "Enable sounds" switch, a rule,
  then the five picks at one level (`CheckRow.enabled` now greys the
  box and text itself). The share-started sound is two low held notes
  a fifth apart (G4→D5) — the first cut was a rising arpeggio too
  close to the DM chime. Audio: Transmission = voice activity
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
- **One instance per profile** (`desktop/src/instance.rs`,
  `interprocess` 2.4): `main` claims a local socket named after the
  config directory (named pipe on Windows, abstract socket on Linux,
  `/tmp/baucord-<hash>.sock` on macOS) before anything else starts.
  **The newcomer knocks before binding, on every platform:** a
  refused bind means different things per OS — Windows refuses a
  held pipe name with `ERROR_ACCESS_DENIED` (`PermissionDenied`, not
  `AddrInUse`), which is how the first cut let a second copy through
  there. A second launch knocks ("show\n") and exits; the first one runs
  `show_main` through `invoke_from_event_loop` — the connect page or
  the room, whichever the window is on. A corpse socket (a crash on
  macOS) is told from a live instance by the knock failing, then
  reclaimed with `try_overwrite`. `show_main` now also un-minimizes
  and focuses the winit window, so the tray's "Show" raises a window
  that was merely behind others. Not tried by hand here (no second
  process in this sandbox).
- **Custom roles (spec §4, 1.1.3):** `server role list` /
  `create <name> [--like guest|member|organizer] [--grant a,b]
  [--deny c] [--rank 15]` / `edit <role> [--name] [--like] [--grant]
  [--deny] [--rank]` / `delete <role>` — `cli_role_*` in `store.rs`
  with the rules in `check_custom_role` (name `[a-z0-9-]{1,32}`, not a
  built-in's; never `ADMIN`); the **rank is derived**
  (`permissions::derived_rank`: the tier of the highest built-in whose
  whole set the role contains, +5 — so "everything a member has" is
  15, "everything an organizer has" 25, never 30; a first cut took a
  `--rank` argument and the user rightly disliked the knob);
  built-ins refuse edits (their sets are rewritten from the binary at
  every start). Permission *names* live in
  `protocol::permissions::PERMISSION_NAMES` (`join`, `speak`, `text`,
  `dm`, `share`, `upload`, `invite`, `create-channels`, …, `gold`).
  **Gold is its own bit** (`GOLD_NAME`): the server's band check and
  the Profile tab's palette both read it, the admin role holds it,
  and `--like admin` is legal for a custom role (with `ADMIN`
  stripped) — so `server role create steward --like admin --deny gold`
  is the administrator without the gold name, ranked above
  organizers (e2e
  `gold_is_a_permission_of_its_own`). `user
  grant/revoke` and `invite create --grants` resolve custom names.
  The running server's reload (`apply_reloaded_policy`) now also
  re-sends each session its `Session` — a CLI grant used to leave
  the client's own gates stale until reconnect — and broadcasts
  `RoleList` (`role_list()` in `moderation.rs`; also after `Session`
  on join). Client: `NetworkMessage::RoleList` → `AppState.roles`
  (the built-ins until it arrives); `role_name_of` / `parse_role_name`
  / `rank_of_client` / `top_role_name` read the table, so the card
  says "streamer", `/grant bob streamer` works, and the Role submenu
  (`menu::RoleChoice`, `Session.our_rank`, `Action::SetRole(id)`)
  lists custom roles at their rank. e2e:
  `custom_roles_come_from_the_cli_and_reach_the_room`.
- **Files (spec §7, 1.1.3) — end to end, untested by cargo here:**
  *Wire* (`FILES` bit 13): `Attachment { blob_id, name, mime, size }`,
  `ClientMessage::SendFile { conv, blob_id, client_ref }`,
  `ServerMessage::FileMessage { conv, from, msg, attachment }` and
  `HistoryAttachments { conv, items: (msg_id, Attachment) }` after a
  page; `UploadRejectReason::{Quota, Full, NoPermission}` and
  `DmFailReason::NoSuchBlob`, sent only to sessions with the bit;
  `MAX_BLOB_BYTES` = 50 MB caps the wire (the client's blob
  reassembly took at most the 10 MB image chunk count before).
  *Server* (`server/files.rs`): an upload is avatar-sized (as before,
  anyone) or a file — `UPLOAD`, `files.max_mb` (≤ 50), the daily
  allowance per user (`BYPASS_LIMITS` skips it, in memory, UTC
  days), the total cap with oldest-unreferenced eviction
  (`Command::DeleteBlob`: row + bytes); file bytes never sit in the
  server's memory (`store_blob(.., is_file)`), a fetch reads them
  back through `Query::Blob`. `SendFile` posts your own blob (never
  another's by hash) into General — broadcast, sessions without the
  bit get `Chat("📎 name (size)")` — or a DM through the same
  store-then-deliver path (`PendingDm.attachment`). `messages.blob_id`
  (in the schema since 1.1.0) marks a card; replay sends the cards
  after the page; deleting a message unrefs the blob. Memory-mode
  servers refuse files. e2e `files_are_posted_as_cards_and_replayed`,
  `a_file_goes_into_a_dm_like_a_message`.
  *Client:* `NetworkControl::SendFile`, `NetworkMessage::FileMessage`
  / `HistoryAttachments`; app-core `ChatItem::File { meta, file:
  FileMeta }` with `push_file`, `attach_file(id, ..)` (a replayed
  empty row becomes the card), `has(id)`, `file_at(i)`,
  `FileMeta::size_text/kind_text`; row kind 5 = `FileRow` in
  chat.slint (a bordered card: type tile, name, size, Save; right
  click: Save file / Delete). The paperclip opens to *any* file:
  images preview as before, anything else `begin_file_upload` →
  `UploadDone` → `file_uploaded` (a pending card in the conversation
  it was picked for, `pending_files` by client ref) → `SendFile` →
  `MessageAck` confirms (General's own echo is skipped by `has(id)`,
  a DM's never comes). Save = `FetchBlob` → `Blob` →
  `media::save_to_downloads` (Downloads folder, never overwrites) →
  toast with the path. **Transfer indicator:** a file card shows a
  determinate ring (a `Path` arc — `rotation-angle` exists only on
  `Image`/`Text` in 1.17) in the Save slot, "Uploading 37%" /
  "Downloading 82%" on its second line, and a 3 px accent rail
  along its bottom edge. The card goes in at 0 % the moment a file
  is picked (placeholder id, `transfers` keyed by (conversation,
  message id) since row ids are fresh on every rebuild, read in
  `chat_row_at`); the media lane's percent reaches it through
  `UploadProgress` (`uploading` names the card; the blob path now
  sets the same tracker the image send does), `UploadDone` gives the
  card its blob and posts, the ack confirms; a Save shows the ring
  from `NetworkMessage::BlobProgress` (chunk count, new). Images
  keep the composer's upload line. Not done: images onto the blob
  path; a ring for an *incoming* image (no placeholder row exists
  before the last chunk).
  **Replay of cards, fixed after the cargo round:** the join replay
  and the "ring covers it" branch of `FetchHistory` build pages from
  the in-memory ring, not the store — `send_rows_as_history` now
  attaches the cards (or the plain name) on all three page paths.
- **Admin panel — wire and server done, window next:** `ADMIN_PANEL`
  bit 14; `AdminCommand::{AuditLog { before, limit }, CreateRole,
  EditRole, DeleteRole, ListBlobs, DeleteBlob, ServerStats}` with
  `AdminOutcome::{Audit(Vec<AuditEntry>), Blobs(Vec<BlobSummary>),
  Stats(StatsSummary), RoleCreated(id)}`. `server/admin.rs`:
  `AuditLog` is a disk read (`Query::Audit` → `Answer::Audit`,
  `pending_audits`, routed before the synchronous dispatch; empty
  without persistence; needs `ADMIN`); roles over the wire apply the
  CLI's rules plus "the role stays below the actor's rank", pick the
  id on the tick (`max + 1`, `Command::InsertRole/UpdateRole/
  DeleteRole` with explicit ids), broadcast `RoleList`, and refresh
  holders' `Session`s; `ListBlobs`/`DeleteBlob` need `RESTRICT`;
  `ServerStats` needs `ADMIN` (uptime from a new `started` instant).
  e2e `the_panel_manages_roles_and_blobs_and_reads_the_audit`. The
  panel window (`admin.slint`, an `AdminState`/`AdminLogic` pair like
  settings, opened by an "Admin" chip under the server name shown
  with `ADMIN` and the negotiated bit) is what remains: tabs Users ·
  Roles · Restrictions · Invites · Channels · Files · Audit · Server,
  each a list from the matching `AdminOutcome` with row actions
  routed through `send_admin(.., PendingAdmin::Panel)`.
  **The window is in** (`ui/slint/admin.slint`, `desktop/src/
  adminui.rs`, an "Admin" chip in gold under the server name for
  `ADMIN`): `AdminState`/`AdminLogic` like settings, `NavItem` and
  `PanelHeader` exported from settings.slint. Users (online dot,
  roles, last seen; Disconnect · Kick · Ban · Reset profile · Purge —
  the destructive ones ask once more via `AdminState.confirming`;
  a Reason field the actions carry; a Set role dropdown over the
  table in rank order), Roles (the client's table; an inline editor
  with the name and a checkbox per permission — `ADMIN` excluded —
  saving as `CreateRole`/`EditRole`), Restrictions (Lift), Invites
  (Revoke; Create opens the main window's dialog), Channels (the
  client's list; Edit opens the channel dialog, Delete for user
  channels), Files (`ListBlobs`, a storage bar against the cap the
  Server tab reported, Delete), Audit (newest first, "Load older"
  pages before the oldest id via `PendingAdmin::AuditMore`), Server
  (the counters). `take_outcome`/`take_error` fill the tab; `Done`
  refreshes it. Rendered: Users and Roles. Not tried: every action
  end to end (needs a running server). **After the first hands-on
  pass:** the door is its own permission, `PANEL` ("panel", bit 21,
  in the admin set, `--like admin` includes it) — the chip, `AuditLog`
  and `ServerStats` read it, so a steward gets the panel without
  `admin`; the chip sits to the right of the server name; the
  **sidebar resizes** by dragging its edge (an 8 px `TouchArea` at
  `x: width - 4px`, `col-resize`, 220–480 px, `State.sidebar-width`,
  kept in `ui.sidebar_width` on release); and **roles wear a badge**
  — `RoleInfo { icon, color }`, `roles.icon`/`roles.color`
  (migration 0003), `permissions::ROLE_ICONS` (16 lucide names, a
  closed list mapped in `widgets::RoleGlyph`), the name palette for
  the color, picked in the panel's editor and by `server role …
  --icon star --color #e06c75`; `UserRow.role-icon/role-color` from
  the top role (`top_role_info`), drawn on the roster badge and the
  card ahead of the built-in glyphs. **Second pass:** `Btn` had no
  horizontal padding (a layout gave it the label's width) — it has
  `min-width: label + 2 × gap-lg` now, and the sidebar chips pad by
  gap-md; **every biting action in the panel confirms in a dialog**
  (`AdminState.confirm-*`, `adminui::PanelAction` kept in
  `AppState.admin_pending`, `ask` → `confirm_accept`), with the reason
  typed *there* for Disconnect/Kick/Ban and a sentence saying what
  the action does — Purge is "erase the user and every message, for
  ghosts and spam", Reset profile is "clear their color, style and
  picture"; the inline "Sure?" and the top Reason field are gone;
  the tab head shows "Loading…" in the refresh icon's slot so the
  list never shifts; the Users tab's role dropdown shows the user's
  highest role (`role-index`) and only sends on a different pick.
  **The Audit tab is a table** (WHEN · WHO · WHAT · ON · DETAILS
  with headers; `format_wall_stamp` dates entries from other days;
  `audit_action_text` turns `user.kick` into "kicked" and so on,
  falling back to the stored action). The server's audit *writers*
  stopped using `{:?}` (`user.roles` names the roles, `invite.create`
  says "as member, unlimited uses, expires in 1 d", `restrict` says
  "voice until 2026-09-09 13:50 UTC: reason", `user.move` names the
  channel, `role.*` list permission names), and the read side
  (`audit_target_name`) turns short user ids into names, role and
  channel ids into names, and restriction/message ids into `#N`.
  Rows written before this keep their old text.
  `push_tr` and `apply_text_scale` now cover the admin window too
  (it is created once and kept, so a language change after the
  first open left it in the old language). What still reads in
  English there is the server's own text — audit details and the
  permission names, which are wire names.
  **A download that "reset" and stuck** was the blob reassembler
  capping total bytes at `MAX_CHAT_IMAGE_BYTES` (10 MiB): every 10 MiB
  the partial was dropped, the next chunk began a fresh one at 0 %,
  and the discarded chunks were gone for good — a counter that reset
  and never landed. It caps at `MAX_BLOB_BYTES` now, its 120 s window
  counts idle time (`last_chunk`) instead of age, and both failures
  (over the cap, hash mismatch) surface as
  `NetworkMessage::BlobFailed` → the card's ring clears and a toast
  says to try again. A finished download also writes a gray system
  line ("Saved to …") into the conversation the card is in.
  **File cards, third pass:** a saved card shows a green check, Open
  (`open::that_detached`; gone from disk → back to Save with a toast)
  and a small Download-again button (`AppState.downloaded` by blob,
  this session; the gray "Saved to" line is gone). A ✕ sits
  beside the progress ring: `Logic.file-cancel` — an upload's
  media stream is reset (`Driver::cancel_media`, new;
  `NetworkControl::CancelUpload`, the pending card goes), a
  download's partial is dropped and later chunks of that blob are
  ignored until it is asked for again (`CancelFetch`,
  `ignored_blobs`). **Sidebar drags:** the width drag draws only a
  line and lands on release (re-laying out every chat row per pointer
  move flickered the window); the Voice/Messages divider drags
  the same way — a line during the drag, the height on release
  (`State.messages-height`, `ui.messages_height`, 100–480 px, the
  section's own height whatever the DM count). The sidebar sits
  at `z: 1` so a tooltip at its edge is not painted over by the chat.
  **Images go up as cards too:** `send_image_with_card` pushes the
  picture itself as a pending image row (`image_upload` holds its
  placeholder id, `transfers` its ring), the media lane's percent
  feeds it, and our own `ImageMessage` echo replaces it
  (`drop_image_card`: the placeholder row, its `images` entry and
  GIF player go, then the real row is pushed); ✕ over the picture
  cancels (`NetworkControl::CancelImage` resets the media stream).
  The sidebar's "Uploading… %" line is gone (`State.upload-line`
  removed; a pasted URL's fetch toasts instead).
  **Regression fixed while there:** `chat_row_at` gave every rebuilt
  image row a *fresh* row id and a placeholder picture — the texture
  is installed once by `push_image_row`, and `images`/`gifs` are
  keyed by row id — so anything rebuilding rows through it
  (`rename_author`, for one) blanked pictures and orphaned GIF
  players. An image row rebuilt in place now keeps its id and
  picture. This is the likely cause of "GIFs stopped showing after a
  rename".
  **Pictures in DMs (spec §7, done):** a picture sent in a DM goes
  up as a file (`begin_file_upload("picture.png", "image/png")`,
  `pending_picture_uploads` → `picture_acks`), and once acked the
  sender's own card becomes the picture (`media::picture_from_file`
  → `ChatHistory::attach_image`); on the receiving end — and in a
  replay — a card whose mime is `image/*` and size within the image
  limit fetches its blob (`want_picture`, `pending_pictures` by
  blob) and turns into an image row when the bytes land. General
  keeps `ImageMessage`. **Moderation rule change (spec §12):**
  disconnect reaches anyone with the permission (`check_reachable`),
  kick and ban refuse only administrators (`check_not_admin`); mutes,
  restrictions, roles and purges keep the rank rule; the roster menu
  mirrors it (`UserFacts::is_admin`); e2e
  `disconnect_reaches_anyone_and_kick_spares_admins`.
  **Chat rows highlight on hover** like the roster: the row's
  TouchArea is now the *parent* of its content (a `TextInput`
  accepts idle moves, so a sibling underneath never saw hover; a
  parent's hover is set in the before-children filter).
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

## 4. Rough edges (known, not urgent)

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
  and the row's hover highlight stay blocked under a `StyledText`.
- `Tr` properties nothing reads: `ptt-tooltip`, `audio-devices-header`,
  `audio-system-default`, `keybinds-header`, `keybind-record` (older
  leftovers, left alone).

## 5. Next steps

1.1.3 is released; every crate on the 1.1 line reads `1.1.4` now.
**1.1.4, all in this drop (untested by cargo here):** blob retention
(`Sqlite::prune_blobs`, `Retention.file_days`), purge drops the
person's blobs, chat xp (`XP_MESSAGE` at the flush), pictures as
files everywhere the server is persistent (`server_persistent` picks
the path; memory mode keeps `ImageMessage`), `limits.max_transfer_kb_
per_minute[_global]` (serde aliases for the old names; the same
sliding-minute budget now gates file uploads in `admit_file_upload`),
`server restore <tar> [--force]` (staging dir, the old db kept as
`baucord.db.before-restore`, refuses while `-wal` exists) and `server
db prune`, the admin channel (`ADMIN_CHANNEL` permission, `AuditLine`
under the `ADMIN_FEED` bit from the audit writer itself, a reserved
conversation key `ADMIN_CHANNEL_KEY` in the Messages list with a
shield, the last page on first open via `PendingAdmin::Channel`,
commands only), and push-to-talk as a watched key (`PttWatcher` on
`device_query`; a registered hotkey consumed the key system-wide —
falls back to registering where the state cannot be read, e.g.
Wayland or macOS without the accessibility grant).

**After the first 1.1.4 build:** a command's output — and the answer
to any admin command, a roster-menu pick included — lands in the
conversation it was asked from (`AppState.command_conv`, set in
`handle_slash_command` and in `send_admin` for `PendingAdmin::Show`;
`push_command_output`), so the admin channel answers in the admin
channel; server notices keep `push_chat_system` (General). Closed
DMs stay closed: `ui.hidden_dms` (hex user ids) is written by the ✕
(`set_dm_hidden`), cleared by "Message" on that person or by a new
message from them, and `Conversations` skips a hidden partner unless
they have unread.

The chat's follow no longer ends at the first "exact bottom": the
ListView estimates the height of rows it has not drawn, so a tall
last row (a picture) lands at the bottom of an estimate and grows
once measured. The timer holds at the bottom for 8 consecutive
frames (`follow-settled`) before it lets go, and the viewport-y
handler no longer clears the follow on reaching the bottom — only a
scroll *up* does. In-place row replacements that change height
(`attach_file`, `picture_from_file`) re-follow when pinned. Roster
rows pad 20 px on the right so the overlaid scrollbar clears the
badges.

The 1.1.3 line as it was (spec §11 item 4), for reference:

1. **Custom roles** — done (§2). Try by hand: `server role create
   streamer --like member --grant manage-channels`, `server user
   grant <who> streamer`, then the card and the Role submenu on a
   running client.
2. **Files** — done (§2), files in DMs included; needs its cargo
   round and a hands-on pass (post a PDF, a 40 MB video, Save both,
   a guest trying the paperclip, a replay after reconnect, a DM file).
3. **Admin panel** — done (§2); needs its cargo round and a
   hands-on pass through every tab as an admin, plus a look as a
   non-admin (no chip).
4. **`server restore`** and **`server db prune`** — small, after.
5. Images onto the blob path — the follow-up the spec allows.

Then the "later" list in spec §11 (channel topics, reserved names, a
manual away toggle, animated avatars, progress features).
