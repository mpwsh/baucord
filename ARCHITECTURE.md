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

Three long-lived threads on the client (the screen "thread" is two
since 1.2.1 — `screen-share` for our own share, `screen-watch` for the
streams we watch — behind one sender, `ScreenTx`, that routes each
command; review P2-44), one process-wide loop on the server, and one
transport crate shared by both. Everything crosses
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
that decides which lane a message rides. The match names every variant
(no `_` arm), so a new message does not compile until its lane is
chosen there; `is_upload_chunk()` is "rides the Media lane".
`ServerMessage::required_feature()` is the same kind of table for the
capability bit a session must have negotiated to be sent the message
(below).

### Messages

`ClientMessage` and `ServerMessage` (`protocol::messages`) are the
list. The 1.0 core is the session (`Hello`, `Welcome`, `Refused`,
`Init`), presence (`UserJoined/Left`, `Speaking`, `AudioState`,
`UserMovedChannel`), channels, chat, screen sharing (`ScreenShare*`,
`Subscribe*`, `KeyframeRequested`, `StreamHealth`), receiver reports
and telemetry. 1.1 appended accounts, roles, history, DMs, blobs,
profiles and the admin commands (PERSISTENCE-SPEC §10, §12); 1.2 the
safety messages — `Block`, `Report`, `DeleteAccount`, `ChatFrom`,
`Authors` (PERSISTENCE-SPEC §12 S1).

Serialization is `postcard` (positional, compact varints) via
`encode_*_message` / `decode_*_message`; an undecodable message is
skipped, never fatal. Because postcard is positional, a shipped
struct never gains a field and a new variant is appended under a new
feature bit. Chat messages carry a server-assigned `id` and `ts`;
relayed images carry a SHA-256 of the whole image, verified on
completion.

The wire is pinned by `protocol/tests/wire_golden.rs`: every wire enum's
variants in order, and every variant's and struct's layout (decoded
from fixed byte patterns and compared with tables written when it
shipped). A variant inserted mid-enum or a field added to a shipped
message fails there, which no round-trip test can catch.

### Versioning

- Breaking wire change → bump `protocol::ALPN` (`baucord/1`). Mismatched
  peers fail the TLS handshake and the client shows `OUTDATED_MESSAGE`.
- Additive capability → a bit in `protocol::features`, offered in
  `Hello`, announced in `Welcome`, `negotiate()`d to the intersection.
  Bits run up to `ADMIN_PAGES` (19) in 1.2.1; every appended variant
  gets a new one, since a client that negotiated an older bit cannot
  decode it (PERSISTENCE-SPEC §12). The server enforces this in one
  place: `send_msg` drops a message whose `required_feature()` the
  session did not negotiate. `QUIET_REPORTS` adds no message: a client
  with it is sent no `VoiceReceiverReport` for an interval in which
  the relay expected none of its voice.
- A message is at most 17 KiB (`MAX_CONTROL_MESSAGE_BYTES`, what every
  shipped client decodes), so a list is fitted by its sender. An admin
  listing longer than that goes out as pages (`send_admin_result`):
  `AdminRows` for the rows ahead, then the `AdminResult` that closes
  the answer; the client joins them (`network::AdminPages`) and the
  app gets one list. A session without `ADMIN_PAGES` is sent the first
  page alone. At most 64 messages an answer.

### Media packing

`voice.rs`, `video.rs`, `system_audio.rs` define the packet headers the
media threads produce and the relay validates (`parse_*`). The relay
never decodes media; it prefixes each packet with the authenticated
sender id (the layout of `relayed::wrap_sender` / `split_sender`) so a
client cannot spoof another's audio.

## 4. Transport (`crates/transport`)

### Driver

`Driver` wraps a `quinn_proto::Endpoint` plus a peer table. Both roles
use it: `Driver::new_client(pin, Budget::client())` and
`Driver::new_server(&identity, max_peers, Budget::server())` — the
latter, with `ServerIdentity` and its `rcgen` dependency, behind the
crate's `server` feature, which clients do not enable. The caller's
loop is always the same four calls:

```rust
udp.recv_batch(|d| driver.handle_datagram(now, d));   // in
driver.handle_timeouts(now);                           // timers
/* application work: driver.recv(...), driver.send(...) */
while let Some(t) = driver.poll_transmit() { udp.send(&t); }   // out
udp.wait_readable(driver.next_deadline());             // sleep
```

The loop sleeps until a datagram lands or `next_deadline()` comes
(review P1-2): quinn's timers, a deferred close, a video group's oldest
queued byte (+500 ms) or ack deadline, the upload pacer's refill.
Anything the driver does on a clock needs a deadline there, or it waits
for the next datagram; the simulator's `step_to_next_event` tests catch
a missing one. `wait_readable` is `ppoll` on Linux and Android, `poll`
or `WSAPoll` elsewhere; the client waits on a `ReadWaiter` thread
instead (§6), which exists on every platform since review P1-1.

Application-facing API, per peer: `send(now, peer, lane, bytes)`,
`recv(peer, lane)`, `video_new_group` / `video_finish_group`
/ `video_reset_group` / `recv_video`, `send_media(frames)` with
`upload_progress`, `close` / `close_after`, `export_keying_material`,
`stats(peer) -> LinkStats`, `track_stats(peer, track)`, and
`drain_events()` yielding `Connected`, `Disconnected { reason }`,
`DatagramsBlocked`. `readable()` lists the peers with anything to
`recv`, so a server asks only those.

A send queues and marks the peer; the peer's connection is drained
once, when the loop next calls `poll_transmit` (or `handle_timeouts` /
`next_deadline`). So what one pass queues for a peer — five voice
packets and a control frame — leaves as one UDP datagram, and a
keyframe relayed in thirty chunks is one drain, not thirty.
`handle_timeouts` touches only peers that are marked or whose deadline
has come. For a payload going to many peers, `Shared::new` /
`Shared::relayed` puts it on the wire once and `send_shared` /
`send_video_shared` share the bytes; stream data is handed to quinn by
reference (`write_chunks`).

A server driver built `with_waiting_room` keeps connections its
application has not admitted (`Driver::admit`) apart from the
admitted: a few seats of their own, a cap per address (an IPv6 /64
counts as one), and a QUIC Retry — proof of the source address — once
half are taken. `max_peers` is then for the application to enforce.

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
client keep-alive (5 s; it was 2 s until 1.2.1 — the simulator holds
an idle connection through 40 % loss on it), server keep-alive (a PING
after 20 s of quiet, `SERVER_KEEP_ALIVE`: it wakes a phone asleep in
the lobby and keeps its NAT mapping, review 2026-10 P1-6, ANDROID-SPEC
D74; the cost is that a client that vanished is noticed after up to
about 50 s — the 30 s idle timeout restarts at the server's own
keep-alive into the dead link — where it was about 30), ACK delay, datagram
buffers, stream/connection
receive windows, `send_window`, socket buffer sizes, media pace. Values
are documented inline with their rationale; change them there, not at
call sites.

### Identity

`identity.rs`:

- `ServerIdentity` — an Ed25519 key in `identity.key`, a self-signed
  certificate regenerated at load (the key is the identity; the cert is
  a container), and `retired/` keys that vouch for a successor: each
  is `retired/<stem>.<unix-secs>.key`, and vouches for two years from
  the retirement in its name (a `<stem>.key` from before 1.2.1 counts
  from its file date; review P1-15).
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
on a LAN. Both loops re-read the GSO segment count every pass, and once
quinn-udp turns GSO off, `send` splits a batch into its datagrams and
resends the one whose failure turned it off (review P1-14).

### Simulator

`transport::sim::SimNet` (feature `sim`; on by default in tests) hosts
any number of `DriverHost`s on links with delay, jitter, loss, a
bandwidth bottleneck with a bounded queue, and address changes, on a
fake clock. `ChatServer` implements `DriverHost`, so
`server/tests/e2e.rs` runs a real server against real client drivers
through the full QUIC stack. The 24 transport scenarios in
`sim/tests.rs` are the acceptance suite for everything above.

## 5. Server (`crates/server`)

`main.rs` owns the socket, the loop, the log file, signals, and the
`rotate-key`, `backup` and `db check` subcommands. `ChatServer` is pure
policy.

### Tick

`ChatServer::update(now)`: transport timers → events → control messages
→ expire pending Hellos → voice relay → video relay → system audio →
periodic reports (stream health 1 s, voice receiver reports 2 s, config
watch 3 s, stats). Only the peers `Driver::readable()` names are asked
for messages and media; the relays send each packet as one
`transport::Shared`, and a broadcast is encoded once (`send_to`; a
relayed picture is framed once, `transport::frame_media`). Voice is
offered only to the speaker's room (`room_members`, rebuilt when
membership changes, never per packet). Stream health is
per sharer (`Driver::track_stats`) over the last five reports, so a
two-second group is never outside the window. The loop then waits on its socket until
`next_wake()`: the transport's next deadline, a Hello's, the next
periodic job, at most 1 s — 2 ms while a database read is out, whose
answer comes on a channel the socket wait cannot see (review P1-2). A
datagram is relayed the moment it lands; an idle server wakes a few
times a second. The loop never sleeps on a socket backlog.

### Admission (`server.rs`, `policy.rs`)

The device key is the identity and invites are the door
(PERSISTENCE-SPEC.md §3). A connection is `pending` from `Connected`
until its first Control frame, which must be `Hello` within 10 s; until
then it sits in the transport's waiting room (§4: a quarter of
`max_clients` again, between 16 and 64 seats, eight per address), so a
full server still shakes hands and a returning user can replace their
own session. Then, in order:

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
7. `ServerFull` when `max_clients` sessions are admitted, again not
   counting this user's own;
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
admin commands. `server invite create|list|revoke` (persistence
on) writes the database directly; the running server picks the change
up within about 3 s (the 2 s `data_version` poll, then the tick's idle
wait of at most 1 s).

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
`FetchBlob/BlobChunk/BlobMissing` messages: avatars
(`persistence.avatars.max_kb`) and, on a persistent server, the files
and pictures posted in General and DMs (`files.rs`, under
`persistence.files`' quotas and the transfer budget). An announced
hash the server already holds is acknowledged without transfer only
for the file's uploader, or for someone who has already sent those
bytes during this run; anyone else is asked for them once, which is
their proof of holding the file (review P0-4; the proofs are in
memory and start empty after a restart). Otherwise chunks fold through the same `fold_upload` as images, the
bytes are hashed, sniffed and stored: metadata in `policy.blobs`,
bytes in `blob_bytes` (the only copy without persistence) and, with
it, at `blobs/<sha256>` next to the database plus a `blobs` row.
Fetches are served from memory or read from disk through
`Query::Blob`, with concurrent fetchers of one hash coalesced.
Reference counts follow profiles, messages and DM conversations; an
unreferenced blob leaves memory, and the hourly prune deletes its
file after `files.retain_days`.

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
- **Reads** (`Query`) carry an id and a deadline; every answer comes
  back on one channel with its id. `Persist::poll` runs once per tick
  and resolves each as answered or `TimedOut`; a reply that lands after
  its deadline is discarded. The tick keeps what each read was for in
  one table (`Waiting`, keyed by the ask).
- **Writes that are one event are one transaction.** A first visit
  (the user, the key, the role, the invite's use, the audit line) and
  every admin command are bracketed by `Persist::begin_group` /
  `end_group` and reach the store as one `Command::Together`: all of it
  lands, in one commit, or none of it.
- **File reference counts** are kept by the tick and written as it
  goes. The store recounts them from the rows at every start (live
  messages and avatars; `messages_blob` index, migration 6), and a
  retention pass takes off the references of the messages it removes
  in the same transaction, then tells the tick (`Notice::Pruned`),
  which drops those messages from its replay ring too.
- **Telemetry** is coalesced per user on the tick (last wins) and handed
  over as a batch once a second; a full queue hands the batch back.
- The thread checkpoints the WAL every 60 s, prunes retention windows
  hourly, and polls `PRAGMA data_version` every 2 s to notice another
  process (the CLI, `sqlite3`) writing to the file; when it does, it
  applies the writes already queued, reloads the policy tables and
  hands the tick a `Notice::Policy`, which sends only what changed —
  `UserDetails` to the users whose roles moved, `Session` to sessions
  whose permissions did, `RoleList` if the roles table changed (review
  P1-22). Each snapshot says how many of the tick's policy writes the
  thread had taken when it read the tables; one older than a write the
  tick has queued since (a ban applied between the read and the swap)
  is set aside by `Persist::poll`, which asks for a fresh one behind
  that write, so the swap never drops a change from memory. The CLI's
  listings open the database read-only, so reading is not a change. Policy writes that are dropped under backpressure are
  logged, not just counted.
- `persistence.enabled: false` starts no thread: writes are dropped at
  the handle, reads answer `Unavailable` on the spot, no file is created.
- With nothing queued the thread sleeps until its next periodic job
  (the `data_version` poll, every 2 s).

Migrations are forward-only (`persistence/migrations/`), applied on the
main thread before the socket binds; a database from a newer build
refuses to open. Wall time on the tick comes from `Clock`
(`clock.rs`): `base_wall + (now − base_instant)`, so the simulator's
fake `Instant` moves wall time too.

### Shutdown

SIGTERM/SIGINT (the `ctrlc` crate with `termination`) sets a flag; the
loop calls `begin_shutdown(now, "server restarting")`, which closes
every session with that reason and flushes pending telemetry; pumps
the transport for up to 500 ms so the close frames leave; then calls
`finish_shutdown()`, which has the persistence thread write what is
queued and exit, waiting at most 6 s for it (one deadline for the
whole wait); and exits 0. The close frames go first so that a slow
database never holds them past Docker's 10 s stop timeout (review
P2-5). The reason string is a wire contract with the client
(`SERVER_RESTART_REASON`).

## 6. Client (`crates/client`)

### Threads and channels

- **network** (`network.rs`) — owns `Driver` + `UdpIo` + the session
  state machine (`Connecting → AwaitingWelcome → Admitted → Closed`).
  Input: `NetworkControl` from the UI/voice/screen threads. Output:
  `NetworkMessage`, routed in `route_message`: received voice goes
  straight into the engine through the `VoiceLink` the voice thread
  handed over (stamped at the socket read), video and link feedback to
  the screen and voice threads (`try_send`), state to the app (an
  unbounded channel, so this thread never waits for the UI — review
  P1-16 — and a wake of the Slint event loop).
- **voice** (`voice.rs`, `client-voice`) — owns the audio engine;
  `VoiceControl` in, `VoiceEvent` out. At each start it hands the
  engine's packet handles to the network thread
  (`NetworkControl::VoiceLink`), so packets never pass through it
  (review P1-5). It wakes on commands, the speaking light's edges and
  the engine's `trouble()` latch (a stream failed, a default device
  moved), and otherwise sleeps until `VoiceChat::next_service()` — a
  side waiting to be reopened, or the 2 s default-device poll where
  streams are open and the OS sends no device events — or an hour
  (`IDLE_WAIT`). A phone in the lobby with nothing open, or a Windows
  desktop whose device watcher runs, has nothing due at all; it ticked
  every second whenever an engine existed (review 2026-10, P2-41).
  Receives `SetUplinkLoss` / `SetLinkRtt` from receiver reports.
- **screen-share** and **screen-watch** (`screen.rs`) — one loop, two
  threads behind one `ScreenTx`, which sends each `ScreenControl` to
  the thread it is for: our own share (capture, encode, the bitrate
  ladder on `UplinkStats`, `StreamHealth`) and the streams we watch
  (reassembly, the jitter buffers, decode, their sound). On one thread
  a sharer who also watched lost capture ticks to decoding (review
  P2-44). `ScreenEvent` out, read by the frontend's present thread.
- **net-inbound** (`inbound_wake.rs`) — blocks on the socket and rings
  the network loop when a datagram lands (below). Its wait is a 30 s
  backstop (`WATCH_SLICE`): the end of a session wakes it with a
  datagram of its own; it was 500 ms, two wakes a second for the whole
  session (review 2026-10, P2-40).
- **upload-prep** — short-lived workers the network thread starts for
  a blob upload: one hashes the file before the announcement, one
  frames its chunks once the server wants them
  (`NetworkControl::BlobPrepared` brings each result back). Hashing and framing a large file on the network
  thread held it, voice included, for 25–100 ms (review 2026-10,
  P2-39).
- Below them, in `video`: **dxgi-capture** (Windows; §7.3) and the
  parked **yuv-band-N** converters; in `audio`, each open device's
  worker and **sysaudio-procloop** (Windows: a share's sound, the
  system mix minus what our own process plays).

The loop in `run_network_loop` selects on the control channel, the
voice encoder's packets and the inbound wake (a `ReadWaiter` thread
blocked on the socket that rings when a datagram lands), with a timeout
of the transport's next deadline or the 2 s stats (review P1-2) while a
session exists, and none otherwise. A socket backlog switches it to a
non-blocking poll; with no inbound wake it polls the socket every 1 ms.

### Session lifecycle

`NetworkControl::Connect { addr, nickname, invite_code, fingerprint,
client_key, portable }` → `Driver::connect`. On the transport's
`Connected` event the client signs the exporter with its key and sends
`Hello`. `Welcome` → `Admitted`, then `NetworkMessage::ServerIdentity
{ fingerprint, first_use }` (the app persists the pin) and `Connected`.

Ending a session produces one of:

- `Disconnected { kind: DisconnectKind, detail }` — final. `UserRequested`,
  `Refused(reason)`, `Outdated`, `LocalError`, `Other`.
- `ConnectionLost { kind: LossKind, detail }` — the app reconnects with
  backoff, five attempts: 2/4/8/16/30 s, and 5/10/20/40/60 s for
  `Full`, each moved by up to half either way so the clients of one
  restarted server do not all knock at once (`reconnect_wait`, review
  2026-10, P2-16). `Timeout`, `Reset`, `Unreachable` (handshake or
  Hello never got through), `ServerRestart`, `Full` (the server refused
  the connection, `DisconnectReason::Refused`: it is full).
- `NeedInvite { why }` — the server wants a code (first visit, or the
  one sent was refused); has its own inline prompt.
- `IdentityChanged { expected, observed }` — refused before any data;
  the user edits or clears the pin.

`kind` is what the UI shows (one localized sentence each); `detail` is
the transport's wording and goes to the log only.

### Media on the wire

- Voice: the engine's packets reach the network thread on the
  `VoiceLink` it was handed (`send_voice` → `pack_voice_with_flags` →
  one datagram). Inbound datagrams → `split_sender` → `VoiceData` →
  straight into the engine through the link's `IncomingSender`, with
  no hop through the voice thread (review P1-5).
- Our share's frames and sound leave screen-share and the
  system-audio encoder as `MediaOut` on their own bounded channel
  (`MEDIA_QUEUE`, `try_send`): a burst drops frames and never queues in
  front of a command. Commands and state travel on unbounded channels,
  so neither the UI nor the network thread waits for the other (review
  P1-16). Each pass handles commands before media, which keeps
  `ScreenShareStart` ahead of the first frame.
- Video out: a keyframe opens a group (`video_new_group(OWN_TRACK)`),
  chunks follow on it; a delta frame with no open group (the group was
  abandoned) is dropped until the next keyframe. `send_screen_share_stop`
  finishes the group.
- Video in: `recv_video` items per track; `GroupReset` forgets that
  sharer's partial frame in the reassembler and requests a keyframe.
  A frame of a stream nobody can see (a minimized desktop viewer,
  `set_stream_hidden`) is dropped here, before it reaches the
  screen-watch thread (review 2026-10, P1-2).
- Stream sound in: once screen-watch has started a stream's decoder it
  registers the decoder's `StreamAudioSender` (`route_stream_audio`),
  and the network thread hands each packet to it directly; until then
  (and after the decoder goes) packets go through screen-watch, which
  starts one. Each 20 ms packet of every watched stream was a wake of
  the screen thread (review 2026-10, P2-45).
- Images: `send_media` on one paced stream; progress from
  `upload_progress`. Inbound chunks are reassembled per (sender,
  upload) and hash-checked.
- Blobs: `UploadBlob` takes its turn (`upload_turns.rs`: one announced
  upload a session at a time), is hashed on an `upload-prep` worker,
  announces (`UploadBegin`), sends the chunks only on `UploadAccept {
  needed: true }`, framed on a worker too, and synthesizes
  `UploadDone` itself on `needed: false`. `FetchBlob` answers arrive as `BlobChunk`
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
threads last reported through `TelemetryAudio/Sharing/Viewing`. A quiet
interval — no share, nothing watched, under 2 kbit/s either way — is
skipped, one a minute still going (review 2026-10, P3-40).
Notable moments (`ShareStarted`, `SendBufferPressure`, …) go
immediately as `TelemetryEvent`.

### Settings (`config.rs`)

`Settings` persists to `settings.json`, next to the executable when
writable (portable) else the platform config dir. A save that would
write what the file already says writes nothing (review 2026-10,
P3-43); otherwise it writes a file beside it, syncs it to the disk and
renames it into place, keeping the previous one as
`settings.json.bak`; a file that does not parse is set aside as
`settings.json.corrupt` and the backup (else the defaults) is used —
never another install's file. `ServerEntry { name, address,
invite_code, fingerprint, channel_passwords }`. Per-person volume and
mute are filed under the person's user id (`peer_id_key`), moved there
from the display name the first time their `UserDetails` arrives.
The connect card splits a typed `host:port#SHA256:…` into address and
pin. Malformed pins
are dropped on load rather than trusted. `load_client_key()` creates
`identity.key` beside the settings file on first use.

Beside `settings.json` also live `avatar.png` (our own picture,
128×128, `ProfileSettings.avatar` says whether it exists) and
`avatar-cache/<sha256>`, the cache of other people's profile
pictures — one transfer per hash per client; only a picture someone's
look names goes in (chat pictures never do), and the folder before
it, `avatars/`, which took every chat picture too, is deleted at
startup. They are decoded on an `avatar-decode` worker that starts
when there is a picture and ends with an empty queue, the cache read
included, and a picture over 512 px a side is refused
(`MAX_RECEIVED_AVATAR_DIM`): the initial shows instead (`avatars.rs`). `Settings` sections added in 1.1:
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
instance and the updater — sit behind `frontend`'s `desktop` feature;
without it each compiles to a same-API twin under `src/stubs/`, which
is what `cargo check -p frontend --no-default-features` builds.
Native dialogs and the URL launcher are the `host` module's
`DesktopHooks`, one implementation of the trait the phones implement
too. Keybinds are watched, not registered (`hotkeys.rs`): the
`keywatch` crate's `key-watch` thread sleeps on the OS's key events
and reads the state of the bound keys when one arrives, so a bound key
still types everywhere else; where the state cannot be read the
bindings are registered hotkeys instead. Until the events have
announced three presses in the process, a slow clock watches over them
in case they never come (another registration took them): 250 ms after
a key event or the start, doubling while nothing happens up to one
look every 4 s, and only a look that ran its full wait counts against
the events — Windows wakes the thread for other messages too. Once
proven, for every later watcher of the process as well (a rebind, a
reconnect), an idle keyboard costs nothing (review 2026-10, P2-34).

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
(the worker threads wake the loop via `WakeFn`), a bounded share each
pass — up to 256 network messages, 32 voice events and 64 screen
events — and wakes itself again at once when more is waiting, so one
busy source cannot hold the loop (review 2026-10, P2-1). The methods are filed
by concern in child modules under `src/app/` — `events` (the drain
and `handle_network_message`, which is exhaustive over
`NetworkMessage`; the compiler will tell you where to add an arm),
`commands`, `presence`, `session`, `menus`, `devices`,
`conversations`, `chat`, `activity`, `profiles`, `safety`, `stats`,
`alerts` — each a child of `app` so nothing private had to open up;
new methods go in the child that owns the concern.

Some of that work is held to what can be seen or used. The roster model
is rebuilt at most once per event-loop pass and only while the main
screen is drawn (`flush_roster`); a speaking or audio-state edge
rewrites its one row (`refresh_voice_row`, review P1-9). "Drawn" is
`main_on_screen()`: not on the connect view, not under a page
(Settings, the admin panel, a phone's stream page or picture viewer),
not hidden to the tray or minimized, and on a phone not in the
background; `Logic.main-screen-shown` catches the roster and the GIFs
up when the main screen comes back (review 2026-10, P2-30). The stream
stats and the resource line wait on the same test. Global
keybinds are watched only between `Connected` and teardown, and only
the bound keys (`HotkeyManager::set_connected`, review P1-3). Pictures
from the wire decode only as PNG, JPEG or GIF
(`images::decode_received`, review P1-20).

Nothing that decodes, reads or hashes a file runs on the UI thread;
each worker starts when there is work and none stays behind idle:
`picture-decode` (a chat picture's preview while its row is on screen,
newest request first, at most 12 waiting; `pictures.rs`, review
2026-10, P1-4), `gif-decode` (every playing GIF's next frame, one
thread while any is open), `gallery-decode` (the picture viewer's full
size), `avatar-decode` (§6), `picture-load` (a picked, pasted or
dropped picture read, decoded and shrunk for sending, EXIF orientation
applied and its metadata dropped), `file-read` (a file to send, P2-6),
`save-download` and `cache-cleanup` (the old `avatars/` folder, once a
launch). Their results come back through `slint::invoke_from_event_loop`
as pixels; the `slint::Image` is made on the UI thread.

`main.rs` sets up file logging — `baucord.log` in the config folder,
beside `settings.json`, rolled over to `baucord.log.1` past 5 MB like
the phones' (`app_core::logfile::FileSink`; it was appended to for ever
in the working folder, review 2026-10, P2-47) — and a panic hook that
writes thread, location and backtrace into it: release builds abort on
panic and a GUI process has no stderr, so without the hook a crash
leaves no trace. Settings → Debug shows, sends and clears that file.

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

The hosts' hooks are one trait, `frontend::host::Hooks` (`host.rs`
— one module since 2026-10-12, the desktop's answers being its
`DesktopHooks`), that each phone installs at startup: pick a file,
save a download, open a target or a link, keep alive (the service, or
on iOS the moment to ask for notifications), keep the screen on, and
the notification calls (§7.4). A phone tells the app when it goes to the
background (`set_host_visible`), and what only feeds the screen waits
until it comes back (`set_app_shown`, the desktop's tray and minimize
too): the network thread stops handing over the link stats, the
resource line's timer stops, the GIFs pause, the stats lines skip
their update, and the device poll slows from every 2.5 s to every
10 s — it still runs, so a picked device that went away falls back to
the default (review 2026-10, P2-33). A phone also stops watching: the
stream page stays, but the app unsubscribes, nothing of the stream
comes, its sound included, and it subscribes again on return, starting
from a keyframe (review 2026-10, P1-2; ANDROID-SPEC D74).

### 7.3 Audio and video per platform

| | capture/playback | echo cancellation | video decode | screen capture |
|---|---|---|---|---|
| Windows | WASAPI, event-driven on an MMCSS thread (`native/wasapi.rs`); cpal enumerates, and opens only if that stream will not (CLI-12) | the Communications category in a voice room only, verified (AEC-SPEC §4) | FFmpeg (static) | DXGI (`dxgi.rs`), screenshots where it fails |
| macOS | cpal (CoreAudio) | none yet (VPIO planned, AEC-SPEC §9) | FFmpeg (static) | screenshots (xcap) |
| Linux | cpal (ALSA/PipeWire) | none | FFmpeg (system) | X11: damage-driven reads (`x11.rs`); Wayland: screenshots (xcap) |
| Android | AAudio voice-call streams (`native/aaudio.rs`, `native/android.rs`) | `AcousticEchoCanceler` on the capture session, read back (E9) | MediaCodec (`video::mediacodec`, `video::yuv`) | — |
| iPhone | the VPIO unit for both sides (`native/vpio.rs`, `native/avsession.rs`) | VPIO, bypass read back (E10) | VideoToolbox (`video::videotoolbox`, `video::annexb`) | — |

Every backend feeds the same contract: one capture callback's samples
into a ring, one render callback's buffer out of one (`native/pump.rs`:
the callback backends, WASAPI's loops and the cpal fallbacks all go
through its `CapturePump`/`RenderPump` — the downmix and the fan-out
existed three times and had drifted, review 2026-10, P2-49),
no locks or allocation on the real-time thread, the playout cushion
sized from the device's callback (ANDROID-SPEC D24). Whether echo
cancellation is on is decided from the stream that was opened, never
assumed from the device, and Settings → Audio says what was found.

`VoiceChat::service()` (the voice thread, when `trouble()` rings or
`next_service()` comes) supervises the devices: a stream that failed is
reopened, the OS default is followed (Windows' device notifications;
a 2 s poll elsewhere, while a stream is open), and a side that is not
open — it never opened, or its reopen failed because the device was
gone — is tried again until it opens (`input_pending`,
`output_pending`; PRODUCTION-PLAN.md CLI-9): after 2 s, then twice as
long each time up to a minute, from 2 s again on a device event, a
pick, a room change or a success (review 2026-10, P2-38). Each failure
is flagged in `AudioStats` (`Health`) and rings `trouble()`, so the
voice thread needs no tick to notice (P2-41).

The phones open only what their presence needs
(`VoiceChat::set_presence`, ANDROID-SPEC D53, IOS-SPEC D51): nothing in the lobby on
Android and a plain output on iOS (to stay alive in the background);
in a room both sides on the call path; muted, the microphone closed
(or muted on iOS's voice unit). The AFK room counts as out of a room
(review P1-6). The desktop keeps both streams open and, muted or out
of a room, drops what the microphone hears before anything processes
it (`InputConfig::in_room`).

The audio workers wait on their device's latch — the callback rings it
— with a 200 ms safety wait, and their owner rings it on drop so a stop
is immediate (review P1-4). Muted or out of a room, the capture callback
pushes nothing and stops ringing, and the worker waits 5 s at a time
(`SILENCED_WAIT`): unmuting rings it at once. It was the 200 ms safety
wait, five wakes a second for as long as one stayed muted or in the
lobby (review 2026-10, P3-24). The chain resets once, on the unmute
edge.

Windows' "Default" device means the Communications default unless echo
cancellation is Off, for the AEC decision, both streams and the device
watcher alike (`communications_default`, review P1-18); an output change
that moves the plan or the reference reopens both sides. The streams go
on the call path — the platform canceller, and on Windows the
Communications category that lowers every other app — in a voice room
only, on every platform (`call_path_wanted`): from connect on, the
lobby included, a speakers user under Automatic had everything else
turned down for nothing to cancel (review 2026-10, P1-5; AEC-SPEC
§4.6). The desktop re-plans on a presence change only when the plan
moves.

A watched stream's audio has an output of its own (stereo, its own
volume, untouched by deafen) — except on a phone's call path, where it
is mixed into the voice output instead (`stream::mix`, ANDROID-SPEC
D56, IOS-SPEC D52), so it shares the call's volume and route and the
echo canceller removes it. That path is mono: the stereo is mixed per
frequency at an aligned phase (`stream::downmix`, ANDROID-SPEC D60),
since a plain `(L + R) / 2` silences out-of-phase sound. A backlog
there (past 180 ms queued) drains by playing 1–4 % faster rather than
by a skip (ANDROID-SPEC D61). Its own output plays on the voice output
picked in Settings, or where voice opened (review P1-19), and its device
callback reads the ring through the same pacing (`stream::pace`): 60 ms
cushion on the desktop, the phones' 100 ms there. The decoder picks per
frame; its own output reopens when its device goes away or the pick
changes. A muted stream is neither decoded nor played, and its output
rests (review 2026-10, P3-27). Its packets reach the decoder from the
network thread directly (§6, P2-45). A sharer sends each 20 ms packet
of its sound as it is encoded (PRODUCTION-PLAN LAT-2).

A watched stream's video goes: the jitter buffer (3 s of frames at the
stream's rate, so at least one keyframe interval; a frame ≥ 1 s behind
live jumps to a newer buffered keyframe; a gap jumps to a buffered
keyframe or asks for one the moment a later frame lands, review P1-12)
→ screen-watch decodes
(FFmpeg and VideoToolbox to RGBA; MediaCodec hands over its YUV planes
as they are, `video::PictureFormat`, one per frame fed even when the
codec finishes them in pairs, `video::held`; between frames
`VideoDecoder::poll` hands out a picture still waiting once the stream
pauses, at the time `next_poll` asks for) → `FrameReady` → the frontend's
present thread (`present-prep`, `frontend::present`), which drains its
queue, keeps the newest frame of each stream and only then converts or
copies it, straight into one of the stream's three reused
`SharedPixelBuffer`s (YUV over up to four cores, `yuv::to_rgba_into`,
on a `yuv::BandPool` of parked `yuv-band-N` threads started by the
first YUV picture and ended with the last stream's buffers — a scope
per picture started up to three threads a frame, review 2026-10,
P2-35) → the UI takes the newest prepared frame per stream and sets it
as an `Image`. Skia then copies it into a buffer of its own to draw it
(SESSION-GUIDE §4, Memory). A device
that cannot keep up shows fewer frames, never older ones: the picture
stays live with its sound (MOB-18). Frame buffers go round
`video::pool` instead of being allocated per frame — DXGI's captures
too, and FFmpeg decodes straight into one when the width is a multiple
of 16 (review P1-11); the pool is held (`retain`/`release`) only while
something is shared or watched, and freed after (P2-43), and it hands
out no buffer more than twice the size asked for (review 2026-10,
P3-37). Closing a viewer drops the stream's decoder,
buffer and audio in screen-watch (`ScreenControl::StopWatching`);
the next viewer starts from a keyframe. The screen threads sleep until
a capture tick, a frame's playout slot, a decoder poll or a log line
(review P1-7), and reach the UI only through the present thread,
which wakes it. A stream nobody can see on the desktop (a minimized
viewer: `ScreenControl::SetVisible`) is not decoded: its frames are
dropped on the network thread (§6), its decoder is let go with the
pictures it held and a `ParkedDecoder` stands in (review 2026-10,
P2-44); its sound plays on, and seen again it starts from a keyframe
it asks for. A phone in the background stops watching instead (§7.2).
A decoder that lost its session (`VideoDecoder::needs_keyframe`: a
failed decode; iOS takes VideoToolbox's back in the background) asks
the sharer for a keyframe, once per loss — a still share sends none of
its own, and the picture stayed frozen (review 2026-10, P1-10).

Sharing, the capture side. On Windows a `dxgi-capture` worker owns the
desktop duplication and queues what changed: a new picture
(`Captured::Desktop`), or only a pointer move (`Captured::PointerMoved`,
which the consumer draws again into the picture it holds — reading the
unchanged desktop out once more was a full-frame copy per move, review
2026-10, P3-35). The pointer is not in what Windows hands over:
`dxgi.rs` draws it in (`pointer.rs`, P2-25). When the share's pictures
are at most half the desktop's size either way, the desktop is halved
on the GPU before it is read back (`Halver`: a two-level texture whose
second level `GenerateMips` builds — 8 MB read for a 4K desktop shared
at 1080p, not 33; off where the device cannot build BGRA mips, review
2026-10, P2-46). DXGI failing for good (an HDR desktop, a rotated
output — review 2026-10, P2-10) falls back to screenshots; a drought
(a locked screen) does too, and from there streaming is tried again
every 10 s (`STREAM_RETRY_INTERVAL`, P2-12). On Linux under X11,
`video::x11::X11Capturer` keeps one connection and reads a monitor
(`GetImage`, the server's own BGRX) only where the DAMAGE extension
reports drawing; xcap opened four connections and read the whole
monitor 30–60 times a second (review 2026-10, P2-43). The screenshot
backend (macOS, Wayland, Windows' fallback) looks at a screen still for
2 s five times a second instead of at the share's rate. A share nobody
watches — no `StreamHealth` from the server for 3 s — captures at the
still screen's pace, one picture a second; a viewer subscribing makes
the server ask for a keyframe, which ends it at once (review 2026-10,
P1-1). After 90 failed encodes in a row (each one already reopened the
session) the share moves to the next encoder in the list (review
2026-10, P2-11).

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
cargo test -p transport         # the sim scenarios among them
cargo test -p server            # unit tests, and the e2e suite through the simulator
cargo clippy --all-targets      # all = deny, pedantic = warn
cargo +nightly fmt --all        # imports_granularity/group_imports are nightly-only
```

CI (`pr-check.yaml`) runs the same with `--locked`, formats with a
dated nightly (`nightly-2026-10-10`: move it on purpose, with a format
commit), type-checks the Windows-only code from Linux with MinGW
(`windows` job: `clippy --target x86_64-pc-windows-gnu` over the crates
without a UI), and runs the chat pane's scrolling harness,
`tools/chat-scroll-sim` (`cargo run` in it): the real `.slint` pane
driven headless through 48 cases (ANDROID-SPEC D66), with the row caps
taken from `app_core::chat` (review 2026-10, P3-59).

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
