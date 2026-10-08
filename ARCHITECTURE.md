# baucord — architecture

For people working on the code. It assumes you know Rust, QUIC and
real-time media in general; it explains baucord's choices, not the
fundamentals. Read the [readme](readme.md) first for what the product
does and how it is run.

## 1. Shape of the system

```
 ┌──────────── desktop process ─────────────┐         ┌──── server process ────┐
 │ UI thread (Slint)                        │         │                        │
 │   frontend::app ── AppState ── settingsui│         │  main.rs: UDP socket,  │
 │      ▲ NetworkMessage        │ NetworkControl       │  tick loop, signals    │
 │      │                       ▼                      │           │            │
 │   ┌──────────── network thread ─────────┐  QUIC    │           ▼            │
 │   │ client::network  (Driver + UdpIo)   │◀────────▶│  ChatServer::update    │
 │   └──▲──────────────────────▲───────────┘          │   admission, channels, │
 │      │ VoiceControl         │ ScreenControl        │   relay, reports       │
 │   voice thread           screen thread             │           │            │
 │   (audio crate)          (video crate)             │        Driver          │
 └──────────────────────────────────────────┘         └────────────────────────┘
```

Three long-lived threads on the client, one process-wide loop on the
server, and one transport crate shared by both. Everything crosses
thread boundaries as owned messages over crossbeam channels; nothing
network-related is `async`.

The one design rule that shapes everything below: **the transport is
sans-IO**. `transport::Driver` is a state machine — datagrams in, `now`
in, datagrams out, events out. The caller owns the socket and the clock.
That is what lets the server's whole behavior, including a 0.5 Mbit/s
viewer on an 8 Mbit/s share or a 20-second blackout, run as an ordinary
`cargo test` on a fake clock in milliseconds.

## 2. Crates

| crate       | depends on                    | owns                                                                    |
| ----------- | ----------------------------- | ----------------------------------------------------------------------- |
| `protocol`  | bytes, serde                  | wire types, lanes, framing, voice/video/system-audio packing            |
| `transport` | protocol, quinn-proto, rustls | `Driver`, `UdpIo`, identity/pinning, budgets, the simulator             |
| `server`    | protocol, transport           | `ChatServer` (lib) + binary; relay, admission, config, hot reload       |
| `client`    | protocol, transport, audio, video | network/voice/screen threads, settings, invites, client key         |
| `app-core`  | —                             | UI-agnostic view models: roster, chat, composer, images, emoji, stats   |
| `frontend`  | client, app-core, ui, i18n    | the UI logic every host runs: `AppState`, the models, settings/admin/viewer, the host seams |
| `desktop`   | frontend                      | the desktop binary: `main.rs` only (logging, config dir, single instance, panic hook) |
| `android`   | frontend                      | the Android host: a cdylib under `NativeActivity`, JNI, the `Hooks` |
| `ios`       | frontend                      | the iPhone host: a binary on UIKit through `objc2`, the `Hooks`        |
| `ui`        | slint, app-core               | `.slint` files and the generated Rust                                   |
| `i18n`      | fluent                        | `t!()` and the three `.ftl` locales                                     |
| `audio`     | cpal, opus, …                 | devices, capture/playback, encoder + rate ladder, jitter buffer, PLC/FEC/RED, AEC, native backends |
| `video`     | ffmpeg-next, ndk, objc2       | capture backends, H.264 encode/decode (FFmpeg, MediaCodec, VideoToolbox), frame chunk buffer |

`server` is a library with a thin binary, so tests can drive it;
`client` is a library the hosts (`desktop`, `android`, `ios`) run. `protocol` and `transport` have no platform dependencies
and build anywhere.

Lints: workspace-wide `clippy::all = deny`, `pedantic = warn`. Keep new
code clean under `cargo clippy --all-targets`; the pedantic warnings
that exist are in the media crates' platform paths.

## 3. Protocol (`crates/protocol`)

### Lanes

One QUIC connection per client, six *lanes* on it. A lane is a delivery
class, not a topic:

| lane          | carrier                             | reliability                       | used for                                   |
| ------------- | ----------------------------------- | --------------------------------- | ------------------------------------------ |
| `Control`     | one bidi stream, client-opened      | reliable, ordered                 | Hello/Welcome, roster, chat, state         |
| `Signal`      | one bidi stream, client-opened      | reliable, ordered, higher priority| subscribe/keyframe/channel join, health    |
| `Voice`       | DATAGRAM                            | unreliable                        | Opus packets                               |
| `SystemAudio` | DATAGRAM                            | unreliable                        | the shared screen's audio                  |
| `Video`       | one uni stream **per group**        | reliable within a group; a group is droppable | H.264 chunks                   |
| `Media`       | one uni stream **per upload**       | reliable, paced, lowest priority  | chat images, log uploads                   |

Every stream starts with a one-byte lane tag, every datagram is
prefixed with one, so the receiver routes without an out-of-band table.
Video group streams carry a `u64` *track* after the tag (§5). Stream
lanes frame messages with a `u32` little-endian length prefix
(`protocol::frame`); datagrams are one packet each.

`ClientMessage::lane()` / `ServerMessage::lane()` is the single place
that decides which lane a message rides. Adding a message means adding
it there too — the match is exhaustive on purpose.

### Messages

`ClientMessage`: `Hello`, `Chat`, `Speaking`, `AudioState`,
`ScreenShareStart/Stop`, `Subscribe/UnsubscribeScreen`,
`RequestKeyframe`, `ChatImageChunk`, `JoinChannel`, `Telemetry`,
`TelemetryEvent`, `LogUploadChunk`.

`ServerMessage`: `Welcome`, `Refused`, `Init`, `UserJoined/Left`,
`Chat`, `Speaking`, `AudioState`, `ScreenShareStarted/Stopped`,
`ShareDenied`, `SubscribeDenied`, `KeyframeRequested`, `StreamHealth`,
`ChatImageChunk`, `UserMovedChannel`, `JoinChannelDenied`,
`ChannelListUpdated`, `VoiceReceiverReport`.

Serialization is bincode-style via `encode_*_message` /
`decode_*_message`; an undecodable message is skipped, never fatal.
Chat messages carry a server-assigned `id` and `ts`; relayed images
carry a SHA-256 of the whole image, verified on completion.

### Versioning

- Breaking wire change → bump `protocol::ALPN` (`baucord/1`). Mismatched
  peers fail the TLS handshake and the client shows `OUTDATED_MESSAGE`.
- Additive capability → a bit in `protocol::features`, offered in
  `Hello`, announced in `Welcome`, `negotiate()`d to the intersection.
  Only `TELEMETRY` and `IDENTITY` are defined today.

### Media packing

`voice.rs`, `video.rs`, `system_audio.rs` define the packet headers the
media threads produce and the relay validates (`parse_*`). The relay
never decodes media; it prefixes each packet with the authenticated
sender id (`relayed::wrap_sender` / `split_sender`) so a client cannot
spoof another's audio.

## 4. Transport (`crates/transport`)

### Driver

`Driver` wraps a `quinn_proto::Endpoint` plus a peer table. Both roles
use it: `Driver::new_client(pin, Budget::client())` and
`Driver::new_server(&identity, max_peers, Budget::server())`. The
caller's loop is always the same four calls:

```rust
udp.recv_batch(|d| driver.handle_datagram(now, d));   // in
driver.handle_timeouts(now);                           // timers
/* application work: driver.recv(...), driver.send(...) */
while let Some(t) = driver.poll_transmit() { udp.send(&t); }   // out
```

Application-facing API, per peer: `send(now, peer, lane, bytes)`,
`recv(peer, lane)`, `send_video` / `video_new_group` / `video_finish_group`
/ `video_reset_group` / `recv_video`, `send_media(frames)` with
`upload_progress`, `close` / `close_after`, `export_keying_material`,
`stats(peer) -> LinkStats`, and `drain_events()` yielding `Connected`,
`Disconnected { reason }`, `DatagramsBlocked`.

`DisconnectReason` is the one type the app reasons about:
`LocallyClosed`, `ApplicationClosed { code, reason }`, `TimedOut`,
`Reset`, `AlpnMismatch`, `IdentityChanged`, `Transport(_)`.
`is_transient()` is `TimedOut | Reset` — the app auto-retries those.

### Video groups (why the Video lane is shaped this way)

A group is one keyframe and the delta frames that depend on it, sent as
one unidirectional QUIC stream. The sender opens a stream at each
keyframe (`video_new_group`), writes chunks, finishes it at the next
keyframe. A group is *abandoned* (stream reset) when:

- **superseded** — a new keyframe arrives while bytes of the previous
  group are still waiting to be handed to QUIC;
- **deadline** — queued bytes waited > 500 ms, or > 1 MB is pending;
- **unacked** — a finished group is not acknowledged within
  500 ms + 2·RTT.

The receiver sees `GroupStart / Chunk / GroupEnd / GroupReset` in group
order per track (QUIC may reorder streams; the driver serializes them).
The point: **a slow viewer loses whole groups and keeps its voice**.
Nothing older than one GOP ever queues, and a reset costs the viewer at
most one keyframe interval (2 s) of freeze.

`LinkStats` counts each cause separately (`groups_reset_superseded /
_deadline / _unacked`) and, separately, `groups_reset_upstream` for
resets the application asked for (the relay mirroring the sharer's own
reset, an unsubscribe) — those are not the peer's fault and must not be
read as "behind".

The QUIC `send_window` (512 KiB both sides) is the depth bound: once it
is full, writes block, bytes wait in the driver's queues *with
timestamps*, and the deadlines can act. That is the whole mechanism;
there are no watermarks.

### Tracks

A viewer can watch several sharers, so outgoing groups are keyed by a
`Track` (`u64`): the relay uses the sharer's `ClientId`, a sharer uses
`OWN_TRACK = 0`. The track rides in the stream header and the receiver
keeps per-track order.

### Media pacing

`send_media` opens one stream per upload and trickles it at
`Budget::media_pace_bytes_per_sec` (client 512 KiB/s, server
`limits.media_relay_kb_per_sec` per receiver) with a token bucket, so
bulk never fills the bottleneck queue that voice datagrams share.
`upload_progress` is the source of the UI's progress bar.

### Budgets

`config::Budget` holds every tunable: idle timeout (30 s both sides),
client keep-alive (2 s), ACK delay, datagram buffers, stream/connection
receive windows, `send_window`, socket buffer sizes, media pace. Values
are documented inline with their rationale; change them there, not at
call sites.

### Identity

`identity.rs`:

- `ServerIdentity` — an Ed25519 key in `identity.key`, a self-signed
  certificate regenerated at load (the key is the identity; the cert is
  a container), and `retired/` keys that vouch for a successor.
  `rotate()` signs the new key with the old one and stores the proof
  in a custom X.509 extension (OID `2.25.3276867917789066850` — never
  change it).
- `Fingerprint` — `SHA256:<base64>` of the SubjectPublicKeyInfo; what
  clients pin and what the invite string carries.
- `PinnedVerifier` — the client's rustls verifier: pinned → accept;
  no pin → accept and report `FirstUse`; mismatch but valid successor
  proof → accept and report `Successor`; otherwise reject before the
  handshake completes (`IdentityChanged`, nothing sent).
- `ClientKey` — the client's own Ed25519 key. `Hello` carries the public
  key plus a signature over the TLS exporter
  (`CLIENT_AUTH_EXPORTER_LABEL`), so the server derives a stable
  `user_id = SHA-256(pubkey)` that cannot be replayed on another
  connection.

### UDP

`UdpIo` is `quinn-udp` (GSO/GRO, ECN) behind a non-blocking socket.
`recv_batch` drains until `WouldBlock` and reports `drained: false`
when it hit its per-call budget — both loops **never sleep while a
backlog remains**. This matters: macOS and Windows return one datagram
per `recv`, and a loop that took one per tick built a second of queue
on a LAN.

### Simulator

`transport::sim::SimNet` (feature `sim`; on by default in tests) hosts
any number of `DriverHost`s on links with delay, jitter, loss, a
bandwidth bottleneck with a bounded queue, and address changes, on a
fake clock. `ChatServer` implements `DriverHost`, so
`server/tests/e2e.rs` runs a real server against real client drivers
through the full QUIC stack. The 19 transport scenarios in
`sim/tests.rs` are the acceptance suite for everything above.

## 5. Server (`crates/server`)

`main.rs` owns the socket, the loop, the log file, signals, and the
`rotate-key`, `backup` and `db check` subcommands. `ChatServer` is pure
policy.

### Tick

`ChatServer::update(now)`: transport timers → events → control messages
→ expire pending Hellos → voice relay → video relay → system audio →
periodic reports (stream health 1 s, voice receiver reports 2 s, config
watch 3 s, stats). `suggested_sleep()` is 1 ms with media flowing,
5 ms quiet, 20 ms empty; the loop also wakes for the transport's next
timer and skips sleeping on a socket backlog.

### Admission (`server.rs`, `policy.rs`)

The device key is the identity and invites are the door
(PERSISTENCE-SPEC.md §3). A connection is `pending` from `Connected`
until its first Control frame, which must be `Hello` within 10 s. Then,
in order:

1. per-IP failure rate limit (5 per 60 s → `RateLimited`);
2. `Auth::Shared` (a 1.0 client) → `BadHello`, not counted;
3. the `ClientKeyProof` must verify against this connection's TLS
   exporter → else `BadHello`; the key hash is `SHA-256(pubkey)`;
4. bans: an active `server` restriction on the user, key or address →
   `Banned { until, reason }`, not counted;
5. who the key is: known → that user (`Auth::Key`; a code sent anyway
   burns no use); retired → `NeedInvite { KeyRetired }`; unknown without
   a code → `NeedInvite { UnknownKey }` (not counted); unknown with a
   code → the code is normalized (uppercase, separators dropped) and
   checked → `InviteInvalid { Unknown | Expired | Exhausted | Revoked }`
   (counted);
6. names: unique among users online, case-insensitively, ignoring this
   user's own sessions; a guest cannot take a stored member's name while
   that member is offline → `NameInUse`;
7. `ServerFull`, again not counting this user's own sessions;
8. this user's own live sessions get `Removed { Replaced }` and yield
   (auto-reconnect after a blackout makes this routine);
9. a first visit creates the user (`user_id` = the founding key's hash),
   links the key, grants the code's role and counts the use; a return
   refreshes the name of record and last-seen;
10. feature negotiation (`SERVER_1_1`), `Welcome { user_id: Some }`,
    `Session { user_id, permissions, persistent }`, then the `Init`
    snapshot — filtered: lobby members other than the newcomer are
    left out, because a fresh session sits in the **lobby** (spec
    §12, 1.1.5) and is announced to nobody. The lobby is the channel
    the server appends after the config and UI channels (empty name,
    `config::with_lobby`). The first `JoinChannel` into a room is
    what a connect used to be: `announce` broadcasts `UserJoined`
    with the room, the details/profile/progress/audio state, and
    sends the newcomer the streams it may now watch. The way back
    (an admin's move, a `ChannelKick`, a hot reload that removed the
    room) is `withdraw`: its stream stops, it stops watching, the
    others get `UserLeft`. Voice from the lobby is dropped like AFK's,
    nothing accrues there, and `ChannelListUpdated` is filtered per
    recipient like `Init`.

A refusal sends `Refused { reason }` and closes 300 ms later so the
reason lands. `remove_session` (replacement, bans, kicks) sends
`Removed { kind, reason, by }`, closes 300 ms later and drops the
session from the rosters at once.

`policy.rs` is the in-memory policy the tick consults (users, keys,
roles, invites, restrictions) in both modes; every change is applied
there first and then queued to the persistence thread. With nobody
holding `ADMIN` at startup the server mints a one-shot admin invite
(1 h, 1 use) and logs its code, the address and the fingerprint at warn
level (`No admin yet. Bootstrap invite …`). `ChatServer::mint_invite / revoke_invite / restrict /
lift_restriction / unlink_key` are the single implementations behind
`ClientMessage::CreateInvite`, the bootstrap code, the tests, and the
admin commands to come. `server invite create|list|revoke` (persistence
on) writes the database directly; the running server picks the change
up within 2 s.

### Relay

`relay.rs`:

- `VoiceRelay` — per voice datagram: validate, note the sequence number
  (for receiver reports), wrap the sender id, fan out to the sender's
  channel minus the deafened.
- `VideoRelay` — per sharer group item: `GroupStart` opens a group on
  every subscriber's track, `Chunk` is validated and forwarded (a viewer
  whose group was reset simply misses the rest), `GroupEnd`/`GroupReset`
  are mirrored. Subscriptions, share scope (channel/everyone) and
  keyframe-request rate gating live here. `stream_health()` computes,
  per sharer, the worst viewer "behind" ratio over the last interval
  from transport counters — measured on the video, never on transport
  ack accounting.

### Channels

Membership is `channel_of: ClientId → u16`. Everything is derived from
the config's channel list; `apply_channel_config` handles hot reload by
*name*: kept channels keep their members, removed or renamed ones move
members to the default, share visibility is re-derived with
exactly-once `ScreenShareStarted/Stopped` corrections. Validation
(`validate_channel_list`) rejects a bad list before anything changes.

### Profiles and blobs (`server/profiles.rs`)

A profile (spec §7.1: name color, name style, avatar hash) lives on
the user row and follows the key. The server is the gate: 3:1
contrast against the chat background, the gold band (HSV hue
38°–65°, saturation ≥ 0.35) refused without `ADMIN`, one change per
30 s, the `profile` restriction as an admin lock (`ResetProfile {
lock }`). `UserProfile` rides after each `UserDetails` (a batch after
`Init`), `ProfileUpdated` on change, both only to sessions that
negotiated `PROFILES`. `UserProgress { client_id, xp, level }` rides
the same way under `PROGRESS` — the batch after `Init` (the newcomer
included), the newcomer to everyone, and once a minute from
`flush_progress` for whoever gained xp — so every client can draw the
card's bar (spec §12). `SetNickname` renames a live session under the
`Hello` name rules and broadcasts `UserRenamed`.

Blobs (spec §7) are the `UploadBegin/Accept/Chunk/Done/Rejected` and
`FetchBlob/BlobChunk/BlobMissing` messages, currently admitting only
avatar-sized images (`persistence.avatars.max_kb`). An announced
hash the server already holds is acknowledged without transfer;
otherwise chunks fold through the same `fold_upload` as images, the
bytes are hashed, sniffed (PNG/JPEG/WebP) and stored: metadata in
`policy.blobs`, bytes in `blob_bytes` (the only copy without
persistence) and, with it, at `blobs/<sha256>` next to the database
plus a `blobs` row. Fetches are served from memory or read from disk
through `Query::Blob`, with concurrent fetchers of one hash
coalesced. Reference counts follow profiles; an unreferenced blob
leaves memory.

### Reports and logs

Every 2 s each client gets `VoiceReceiverReport { received, expected,
lost, late, rtt_ms }` — what the relay actually received of its voice.
This is the **only** loss figure the client's audio ladder may act on.
The same tick writes the `link:` line when
`logging.link_stats_interval_secs` is due. Telemetry samples and events
from clients are logged as `telemetry:` / `telemetry event:` lines;
log uploads are stored under `<identity.dir>/logs/`.

### Persistence (`persistence/`)

The tick never touches disk (PERSISTENCE-SPEC.md §0, rule 1). One
`persistence` thread owns the SQLite connection (`rusqlite`, bundled,
WAL, `synchronous=NORMAL`, `data/baucord/baucord.db`); the tick holds a
`Persist` handle:

- **Writes** (`Command`) go on a bounded queue (`persistence.write_queue`,
  4,096). A full queue *drops and counts* (`dropped_writes` in the stats
  line); nothing on the tick ever waits. General chat is broadcast first
  and inserted after — a drop is a hole in history, never a lost
  delivery. Message ids come from the tick's counter, rebased on the
  database's watermark at startup.
- **Reads** (`Query`) carry a reply channel and a deadline; `Persist::poll`
  runs once per tick and resolves each as answered or `TimedOut`. A reply
  that lands after its deadline is discarded.
- **Telemetry** is coalesced per user on the tick (last wins) and handed
  over as a batch once a second; a full queue hands the batch back.
- The thread checkpoints the WAL every 60 s, prunes retention windows
  hourly, and polls `PRAGMA data_version` every 2 s to notice another
  process (the CLI, `sqlite3`) writing to the file; when it does, it
  reloads the policy tables and hands the tick a `Notice::Policy` to
  swap in. Policy writes that are dropped under backpressure are
  logged, not just counted.
- `persistence.enabled: false` keeps the thread with a memory-only store:
  writes are discarded, reads answer `Unavailable`, no file is created.

Migrations are forward-only (`persistence/migrations/`), applied on the
main thread before the socket binds; a database from a newer build
refuses to open. Wall time on the tick comes from `Clock`
(`clock.rs`): `base_wall + (now − base_instant)`, so the simulator's
fake `Instant` moves wall time too.

### Shutdown

SIGTERM/SIGINT (the `ctrlc` crate with `termination`) sets a flag; the
loop calls `shutdown(now, "server restarting")`, which closes every
session with that reason, flushes pending telemetry, asks the
persistence thread to drain its queue (bounded to 5 s) and exit, pumps
for up to 500 ms so the close frames leave, and exits 0. The reason
string is a wire contract with the client (`SERVER_RESTART_REASON`).

## 6. Client (`crates/client`)

### Threads and channels

- **network** (`network.rs`) — owns `Driver` + `UdpIo` + the session
  state machine (`Connecting → AwaitingWelcome → Admitted → Closed`).
  Input: `NetworkControl` from the UI/voice/screen threads. Output:
  `NetworkMessage`, routed in `route_message`: media and link feedback
  go straight to the voice/screen threads (hot path, `try_send`), state
  goes to the app (blocking send + a wake of the Slint event loop).
- **voice** (`voice.rs`) — the audio engine; `VoiceControl` in,
  `VoiceEvent` out. Receives `SetUplinkLoss` / `SetLinkRtt` from
  receiver reports.
- **screen** (`screen.rs`) — capture/encode when sharing, jitter
  buffer/decode per watched stream; `ScreenControl` in, `ScreenEvent`
  out. Receives `UplinkStats` (the share of groups the transport
  abandoned) for its bitrate ladder and `StreamHealth` for display.

The loop in `run_network_loop` blocks on the control channel with a
timeout of 1 ms (capped by the transport's next timer) while a session
exists, or indefinitely otherwise; a socket backlog switches it to a
non-blocking poll.

### Session lifecycle

`NetworkControl::Connect { addr, nickname, password, fingerprint,
client_key, portable }` → `Driver::connect`. On the transport's
`Connected` event the client signs the exporter with its key and sends
`Hello`. `Welcome` → `Admitted`, then `NetworkMessage::ServerIdentity
{ fingerprint, first_use }` (the app persists the pin) and `Connected`.

Ending a session produces one of:

- `Disconnected { kind: DisconnectKind, detail }` — final. `UserRequested`,
  `Refused(reason)`, `Outdated`, `LocalError`, `Other`.
- `ConnectionLost { kind: LossKind, detail }` — the app reconnects with
  backoff (2/4/8/16/30 s, five attempts). `Timeout`, `Reset`,
  `Unreachable` (handshake or Hello never got through), `ServerRestart`.
- `AccessDenied` — wrong password; has its own inline prompt.
- `IdentityChanged { expected, observed }` — refused before any data;
  the user edits or clears the pin.

`kind` is what the UI shows (one localized sentence each); `detail` is
the transport's wording and goes to the log only.

### Media on the wire

- Voice: `SendVoicePacket` → `pack_voice_with_flags` → one datagram.
  Inbound datagrams → `split_sender` → `VoiceData` → voice thread.
- Video out: a keyframe opens a group (`video_new_group(OWN_TRACK)`),
  chunks follow on it; a delta frame with no open group (the group was
  abandoned) is dropped until the next keyframe. `send_screen_share_stop`
  finishes the group.
- Video in: `recv_video` items per track; `GroupReset` forgets that
  sharer's partial frame in the reassembler and requests a keyframe.
- Images: `send_media` on one paced stream; progress from
  `upload_progress`. Inbound chunks are reassembled per (sender,
  upload) and hash-checked.
- Blobs: `UploadBlob` announces (`UploadBegin`), sends the chunks only
  on `UploadAccept { needed: true }`, and synthesizes `UploadDone`
  itself on `needed: false`. `FetchBlob` answers arrive as `BlobChunk`
  and are reassembled per hash (`accept_blob_chunk`), the hash being
  the blob's id and the only thing trusted.

### Link feedback

Every 2 s `sample_link_stats` publishes `NetworkStats` (path RTT/loss/
throughput — UI only) and `UplinkVideo` (groups started and abandoned,
path loss, RTT, send rate: the screen ladder and the sharer's summary
line), and logs `video uplink: abandoned N/M groups
(superseded/deadline/unacked)` at debug whenever it abandoned any. If no receiver report has arrived for 6 s
while admitted, it feeds the audio ladder a "bad link" reading so
silence is not mistaken for a clean link. The screen ladder weighs each
sample against what went out *and* what the encoder offered: an uplink
that collapsed sends little because it is overloaded, not idle
(CLI-13).

Every 10 s (when negotiated) a `TelemetrySample` goes to the server:
path numbers from `LinkStats`, plus whatever the voice and screen
threads last reported through `TelemetryAudio/Sharing/Viewing`.
Notable moments (`ShareStarted`, `SendBufferPressure`, …) go
immediately as `TelemetryEvent`.

### Settings (`config.rs`)

`Settings` persists to `settings.json`, next to the executable when
writable (portable) else the platform config dir. `ServerEntry { name,
address, password, fingerprint, channel_passwords }`;
`ServerEntry::from_invite` parses `host:port#SHA256:…`. Malformed pins
are dropped on load rather than trusted. `load_client_key()` creates
`identity.key` beside the settings file on first use.

Beside `settings.json` also live `avatar.png` (our own picture,
128×128, `ProfileSettings.avatar` says whether it exists) and
`avatars/<sha256>`, the cache of other people's pictures — one
transfer per hash per client. `Settings` sections added in 1.1:
`ui` (text scale, chat prefs, image budget), `profile`, `startup`,
`sounds`, `audio.push_to_talk`; every field has a serde default so
older files load unchanged.

## 7. The UI logic (`crates/frontend`) and the hosts

### 7.1 The shared UI logic

Since 1.1.6 the client's UI logic lives in `frontend`, shared by
every host; `desktop` is `main.rs` alone (the config directory from
`--profile`/`--config-dir`, file logging, the single instance, the
panic hook, `frontend::app::run()`), and the phone hosts (§7.2) are as
thin. Host-only pieces — the
winit window hooks, the system clipboard, global hotkeys, the single
instance, the updater, native dialogs and the URL launcher (the
`host` module) — sit behind `frontend`'s `desktop` feature; without it
each compiles to a same-API twin under `src/stubs/`, which is what
`cargo check -p frontend --no-default-features` builds.

Profile flow: `send_own_profile` runs after `Session` and on Save &
close of Settings when the look changed (`profile_dirty`); with a
picture the blob goes first under `AVATAR_UPLOAD_ID` and `UploadDone`
calls back with the hash. Everyone's look is `profiles: ClientId →
Profile`; pictures are `avatar_images: hash → slint::Image`, warmed
from the disk cache before any `FetchBlob`. `apply_profile_look`
prefers the server's copy and falls back to our settings for
ourselves. Moderation, channel and message actions in the UI send
the same `NetworkControl`s the slash commands do — one authorization
path, two front ends.

`app.rs` is the `AppState`: one `Rc<RefCell<_>>` reached through
`with_current` from Slint callbacks and timers. It drains
`NetworkMessage`/`VoiceEvent`/`ScreenEvent` on every event-loop pass
(the worker threads wake the loop via `WakeFn`). The methods are filed
by concern in child modules under `src/app/` — `events` (the drain
and `handle_network_message`, which is exhaustive over
`NetworkMessage`; the compiler will tell you where to add an arm),
`commands`, `presence`, `session`, `menus`, `devices`,
`conversations`, `chat`, `profiles`, `stats`, `alerts` — each a child of `app`
so nothing private had to open up; new methods go in the child that
owns the concern.

`main.rs` sets up file logging (append-mode `baucord.log`) and a panic
hook that writes thread, location and backtrace into it — release builds
abort on panic and a GUI process has no stderr, so without the hook a
crash leaves no trace.

`app-core` holds the view models the UI binds to (roster, chat with
markdown/links/images/gifs, composer, emoji, stats formatting) and is
deliberately free of Slint so it can be unit-tested and reused.

### 7.2 The hosts

Three hosts run `frontend::app::run()`; the UI is the same `.slint`
tree everywhere, with `State.touch-host` and the window's size deciding
the phone layout (the sidebar and the chat as pages, Settings and the
stream viewer as pages inside the one window, ANDROID-SPEC D22/D23).

- **Desktop** (`crates/desktop`): winit, Skia (software raster by
  default), the tray, global hotkeys, the self-updater, native dialogs —
  the `desktop` feature of `frontend`.
- **Android** (`crates/android`, `android/`): a cdylib loaded by
  `BaucordActivity` (a `NativeActivity` subclass) through Slint's
  android-activity backend, Skia on the GPU. What native code cannot do
  is a few lines of Java called through JNI (`java.rs`): permissions,
  the file picker, Downloads through MediaStore, opening files and
  links, the foreground `VoiceService` that keeps a connected session
  alive off screen, notifications (`Notices.java`). The process ends
  when the Activity is destroyed (D11).
- **iPhone** (`crates/ios`, `ios/`): a binary; winit on UIKit, Skia on
  Metal; `UIApplicationMain` never returns, so settings are saved on the
  way into the background (`lifecycle.rs`). UIKit is reached from Rust
  through `objc2` (`host.rs`: Photos/Files pickers, Quick Look, Safari;
  `notices.rs`: UserNotifications; `session.rs`: the audio session).
  No Swift.

The phones' hooks are one trait, `frontend::host::Hooks`
(`stubs/host.rs`), that each host installs at startup: pick a file,
save a download, open a target or a link, keep alive (the service, or
on iOS the moment to ask for notifications), keep the screen on, and
the notification calls (§7.4). A phone tells the app when it goes to the
background (`set_host_visible`), and everything that only feeds the
screen — stats lines, GIFs, stream frames, the device poll — stops
until it comes back.

### 7.3 Audio and video per platform

| | capture/playback | echo cancellation | video decode | screen capture |
|---|---|---|---|---|
| Windows | WASAPI, event-driven on an MMCSS thread (`native/wasapi.rs`); cpal enumerates, and opens only if that stream will not (CLI-12) | the Communications category, verified (AEC-SPEC §4) | FFmpeg (static) | DXGI |
| macOS | cpal (CoreAudio) | none yet (VPIO planned, AEC-SPEC §9) | FFmpeg (static) | xcap |
| Linux | cpal (ALSA/PipeWire) | none | FFmpeg (system) | xcap (X11; Wayland is screenshots) |
| Android | AAudio voice-call streams (`native/aaudio.rs`, `native/android.rs`) | `AcousticEchoCanceler` on the capture session, read back (E9) | MediaCodec (`video::mediacodec`, `video::yuv`) | — |
| iPhone | the VPIO unit for both sides (`native/vpio.rs`, `native/avsession.rs`) | VPIO, bypass read back (E10) | VideoToolbox (`video::videotoolbox`, `video::annexb`) | — |

Every backend feeds the same contract: one capture callback's samples
into a ring, one render callback's buffer out of one (`native/pump.rs`),
no locks or allocation on the real-time thread, the playout cushion
sized from the device's callback (ANDROID-SPEC D24). Whether echo
cancellation is on is decided from the stream that was opened, never
assumed from the device, and Settings → Audio says what was found.

`VoiceChat::service()` (the voice thread, every tick) supervises the
devices: a stream that failed is reopened, the OS default is followed,
and a side that is not open — it never opened, or its reopen failed
because the device was gone — is tried again every 2 s until it opens
(`input_pending`, `output_pending`; PRODUCTION-PLAN.md CLI-9).

The phones open only what their presence needs
(`VoiceChat::set_presence`, ANDROID-SPEC D53, IOS-SPEC D51): nothing in the lobby on
Android and a plain output on iOS (to stay alive in the background);
in a room both sides on the call path; muted, the microphone closed
(or muted on iOS's voice unit). The desktop keeps both streams open.

A watched stream's audio has an output of its own (stereo, its own
volume, untouched by deafen) — except on a phone's call path, where it
is mixed into the voice output instead (`stream::mix`, ANDROID-SPEC
D56, IOS-SPEC D52), so it shares the call's volume and route and the
echo canceller removes it. That path is mono: the stereo is mixed per
frequency at an aligned phase (`stream::downmix`, ANDROID-SPEC D60),
since a plain `(L + R) / 2` silences out-of-phase sound. A backlog
there (past 180 ms queued) drains by playing 1–4 % faster rather than
by a skip (ANDROID-SPEC D61). The decoder picks per frame; its own
output reopens when its device goes away.

A watched stream's video goes: the jitter buffer (3 s of frames at the
stream's rate, so at least one keyframe interval; a frame ≥ 1 s behind
live jumps to a newer buffered keyframe) → the screen thread decodes
(FFmpeg and VideoToolbox to RGBA; MediaCodec hands over its YUV planes
as they are, `video::PictureFormat`, one per frame fed even when the
codec finishes them in pairs, `video::held`; between frames
`VideoDecoder::poll` hands out a picture still waiting once the stream
pauses) → `FrameReady` → the frontend's
present thread (`frontend::present`), which drains its queue, keeps
the newest frame of each stream and only then converts or copies it,
straight into one of the stream's three reused `SharedPixelBuffer`s
(YUV over up to four cores, `yuv::to_rgba_into`) → the UI takes the
newest prepared frame per stream and sets it as an `Image`. A device
that cannot keep up shows fewer frames, never older ones: the picture
stays live with its sound (MOB-18). Frame buffers go round
`video::pool` instead of being allocated per frame. Closing a viewer
drops the stream's decoder, buffer and audio in the screen thread
(`ScreenControl::StopWatching`); the next viewer starts from a
keyframe.

### 7.4 Notifications on the phones

`app::alerts` is the policy, the hosts carry it out (ANDROID-SPEC D52,
IOS-SPEC D50). On screen, a DM or mention chimes — through the voice
engine where the phone's ringer allows it (`host::chime_route()`:
Android checks the ringer mode and Do Not Disturb), or as a sound-only
system notification on iOS, the only route that honours the ring/silent
switch. Off screen, a DM becomes a system notification whose tap opens
the conversation, and the first voice into empty rooms becomes "Ana
joined General". All of it lives in the running app: there is no push
service, so a phone whose app is closed or suspended gets nothing
(PRODUCTION-PLAN.md PRD-1).

## 8. Wire contracts and constants to respect

| item                                        | where                                   | change means                                   |
| ------------------------------------------- | --------------------------------------- | ---------------------------------------------- |
| `ALPN = "baucord/1"`                        | `protocol::lanes`                       | old clients refused; bump on breaking changes  |
| lane tags, `u32 LE` framing, track header   | `protocol::lanes`, `frame`, `transport` | every peer                                     |
| successor-proof OID `2.25.3276867917789066850` | `transport::identity`                | every pinned client after a rotation           |
| exporter label `baucord-client-auth`        | `transport::identity`                   | every user id changes                          |
| close reason `"server restarting"`          | server `main.rs`, `client::network`     | clients stop auto-reconnecting on deploys      |
| close reason `"no hello"`                   | `server::server`, `client::network`     | lost-Hello stops counting as transient         |
| `VoiceReceiverReport` cadence 2 s, ≥15 pkt gate | server, `client::network`           | audio ladder behavior                          |
| `KEYFRAME_INTERVAL_MS = 2000`               | `video::codec`                          | max freeze after a reset; group length         |

## 9. Testing

```bash
cargo test --all                # everything; media crates need their system libs
cargo test -p transport         # 45 tests, the sim scenarios among them
cargo test -p server            # 49 unit + 76 e2e through the simulator (2026-10-07)
cargo clippy --all-targets      # all = deny, pedantic = warn
cargo +nightly fmt --all        # imports_granularity/group_imports are nightly-only
```

Adding a server behavior: write it as an `e2e.rs` test first — `World`
gives you a server node and `TestClient`s that speak the real protocol
over the simulated network; `w.settle(…)` advances the fake clock.
Adding a transport behavior: `sim/tests.rs`, with `LinkSpec` for the
link you want to hurt.

## 10. Operational notes for developers

- `link:` and `telemetry:` lines in the server log are the primary
  diagnostic; the readme describes how to read them. The client logs
  `link:` only at ≥5 % path loss (at debug while our video flows: the
  `sharing` line has it), and one summary line per stream every 5 s
  (`viewing …`, `sharing …`, `client::viewlog`; SESSION-GUIDE.md §4
  says how to read them) — at warn only when something is wrong, so a
  log without warnings is a healthy one.
- A crash on a client ends `baucord.log` with `PANIC in thread … at
  file:line` — ask for that line first.
- The server's identity directory is the one thing that must never be
  lost between deploys; everything else is reproducible from config.
