# baucord 1.1 — persistence: accounts, roles, history, DMs, files

**Status:** proposal. Builds on the 1.0 line as shipped (Part C seams:
`Hello.auth` enum, feature bits, client keys sent on every connect,
server-assigned chat `id`/`ts`, content-hashed images, telemetry).
Identity follows TeamSpeak 3: a key on the device, roles on the key,
invite codes as privilege keys, no registration. Nothing here changes
the media path.

## 0. Rules that everything below obeys

1. **The relay tick never touches disk.** One `persistence` thread owns
   the SQLite connection. The server loop talks to it over two bounded
   crossbeam channels: writes are fire-and-forget commands; reads carry
   a reply channel and a deadline. If the thread falls behind, writes
   coalesce or drop *with a counter in the stats line* — they never
   block voice, video, or admission.
2. **Authorization is decided by the server, from the database, per
   message.** The client renders what the server says it may do; it
   never tells the server what it is. There is no client-side field
   that influences a role, a permission, or a display badge.
3. **Additive wire only.** Every new message is a new enum variant
   behind a feature bit; 1.0 clients keep working against a 1.1 server
   (they see no history, no DMs, no roles, and are treated as guests).
   The ALPN string does not change.
4. **Live state stays in memory.** Channels, memberships, roles and
   restrictions are loaded once at startup and on change; the tick
   consults RAM. The database is the source of truth across restarts,
   not a lookup on the hot path.
5. **Policy and data are different things.** *Policy* — roles,
   restrictions, invites, channels — is small and always in memory;
   with persistence on it is also written to SQLite, with persistence
   off it lives exactly as long as the process. *Data* — history,
   offline DMs, files, profiles at rest, telemetry rows, audit, stats
   — needs SQLite and does not exist without it. So a server with
   persistence off still has admins, invites, kicks and mutes for the
   session, and loses only what needs a place to put bytes (§1.1).
6. **The config file is input.** `server.yaml` is read at startup and
   its `channels` section is hot-reloaded on change; the process never
   writes to it. Anything the process persists goes to the database or
   nowhere.

## 1. Storage

**Engine:** SQLite via `rusqlite` (bundled), `data/baucord/baucord.db`,
WAL mode, `synchronous=NORMAL`. One file next to the identity key, so
`setup.sh`'s state directory and backups already cover it. The server
binary grows ~1.5 MB. The client never embeds SQLite.

**Large bytes live on disk, not in the database:**
`data/baucord/blobs/<sha256>` for files, images and avatars; the
database holds metadata and reference counts. SQLite is fine with
megabyte blobs but a backup with 5 GB of movie files inside it is not,
and content-addressed files on disk are trivial to dedup, cap and prune.

**Threading:** the `persistence` thread runs at normal priority (the
relay's tick thread is the one that matters), applies migrations at
startup before the socket binds, checkpoints WAL every 60 s, prunes
retention windows hourly, and exits cleanly on shutdown after draining
its queue (bounded: 5 s).

**Backpressure:** write queue 4,096 commands. Message inserts past the
limit are dropped and counted (`persistence_dropped_writes`); stats and
telemetry writes are coalesced per user (last wins). Reads (history on
join, DM inbox, blob metadata) time out at 2 s and the client gets an
empty/partial answer plus a retry hint — a slow disk must never hold a
`Welcome` hostage. The tick-side API is `Persist::send(cmd)` and
`Persist::ask(query, deadline) -> Receiver<Answer>`; the loop polls
answers each tick, so a reply arriving late is delivered late, not
blocking.

**Operations:** `server backup <file.tar>` — the database via the
online backup API (copying a WAL database with `cp` is unsafe) *and*
the `blobs/` directory, in one archive, because a backup with one and
not the other restores to dangling references; `server restore`,
`server db check`, `server db prune`.

**CLI against a running server** (persistence on only — with
persistence off there is no CLI; administration is over the wire,
§8): the CLI writes SQLite; the persistence thread polls `PRAGMA
data_version` every 2 s and, on an external change, reloads the policy
tables and hands the tick a fresh snapshot behind an `Arc` swap. This
also covers an operator editing the database by hand.
Migrations are forward-only, recorded in `schema_version`; a downgrade
refuses to start rather than guess.

### 1.1 Persistence on and off

| capability                         | persistence **off** (memory only)                  | persistence **on** (SQLite)           |
| ---------------------------------- | -------------------------------------------------- | ------------------------------------- |
| admin                              | ✔ whoever redeems the bootstrap invite from the startup log | ✔ once; the database remembers |
| roles on keys                      | ✔ until the process exits                          | ✔                                     |
| invites                            | ✔ until they expire or the process exits           | ✔                                     |
| kick / ban / mute / restrictions   | ✔ until the process exits                          | ✔                                     |
| disconnect                         | ✔                                                  | ✔                                     |
| channels from the UI               | ✔ temporary only (vanish when empty, or with the process) | ✔ temporary or permanent       |
| general chat                       | ✔ live only                                        | ✔ with history replay                 |
| DMs                                | ✔ online recipients only, no history               | ✔ offline delivery, unread, history   |
| files                              | ✔ live relay to online users (how images work today), nothing stored | ✔ stored, dedup, retention, cards in history |
| profiles (color, style, avatar)    | ✔ client-held, sent on connect, broadcast          | ✔ also stored on the user row         |
| telemetry queries, audit, purge                      | –                                | ✔                                     |
| progress (xp / level)              | ✔ for the session                                  | ✔ kept                                |
| admin panel / admin channel        | ✔ over the in-memory policy                        | ✔                                     |

Persistence-off is the LAN/dev/"just tonight" server: everything in
this document works for the session, and a restart forgets roles,
bans, invites and UI channels. The server tells clients
(`Welcome.persistent: false`), and the client shows a standing notice
in the admin panel and on moderation dialogs: *"This server does not
save changes. Roles, bans and invites last until it restarts."*

**Bootstrap, both modes:** when the server starts with no admin —
always, with persistence off; only on an empty database with it on —
it prints a one-shot bootstrap invite in the startup log next to the
identity fingerprint: `Bootstrap invite (admin, 1 use, 1 h):
baucord://…/CODE#SHA256:…`. Whoever runs the server has the log;
nobody else does. This replaces "the first key to connect is admin",
which is unsafe the moment clients auto-reconnect after a restart
(whichever friend's laptop reconnects first would win). `server
invite create` on the box does the same for a running server with
persistence on; with persistence off the log code is the only way in
after a restart, by design.

**Config:**

```yaml
persistence:
  enabled: true                       # false = everything in memory for the session (§1.1)
  path: data/baucord/baucord.db
  history: { replay: 50, retain_days: 90 }
  files:   { max_mb: 50, per_user_daily_mb: 500, total_cap_gb: 20, retain_days: 30 }
  avatars: { max_kb: 256 }
  telemetry: { retain_days: 30 }
user_channels:                        # `channels:` stays the hot-reloaded base list
  max_per_user: 2                     # UI-created channels an organizer may own
  temporary_after_min: 30             # empty user channels are deleted after this
  afk_channel: AFK                    # where idle people are moved (a config channel)
  afk_after_min: 30                   # default idle timeout; a channel may set its own
limits:
  max_sharers: 8                      # relay egress guards (§13), not user policy
  max_viewers_per_stream: 16
  chat: { burst: 5, per_secs: 5 }     # per-user chat rate limit
```

Invites are the only door in both modes (§3); `authentication.password`
is gone. There are no `roles:`, `invites:` or `restrictions:` sections:
the config file is input, never a store. A LAN/dev server that wants
"anyone may join" mints one long-lived guest invite once per run and
pins it to the wall.

## 2. Schema

```
schema_version   (version)
users            (user_id PK, name, created_at, last_seen, invited_with NULL,
                  name_color NULL, name_style {normal,bold,italic}, avatar_blob NULL,
                  profile_locked, profile_updated_at)
user_keys        (key_hash PK, user_id FK, label, added_at, last_seen, retired_at NULL)
                                                    -- devices; a user = one or more keys
roles            (role_id PK, name UNIQUE, permissions BIGINT, rank, builtin)
user_roles       (user_id, role_id, granted_by, granted_at)
restrictions     (id PK, user_id FK NULL, key_hash NULL, ip NULL,
                  kind {server,voice,general_text,dm,upload,share,profile},
                  until NULL, reason, by, created_at)
invites          (code PK, created_by FK, created_at, expires_at NULL, max_uses NULL,
                  uses, grants_role FK, revoked_at NULL)                   -- TS3 privilege key
channels         (channel_id PK, name UNIQUE, password NULL, user_limit NULL,
                  min_role NULL, permanent, afk_after_min NULL, is_afk,
                  source {config,user}, created_by NULL, created_at, position, empty_since NULL)
conversations    (conv_id PK, kind {general,dm}, user_a NULL, user_b NULL)  -- dm: a < b
messages         (msg_id PK, conv_id FK, ts, user_id, body, blob_id NULL, deleted_at NULL)
message_reads    (conv_id, user_id, last_read_msg_id)                     -- unread counts
blobs            (blob_id PK = sha256, mime, size, name, created_by, created_at, refs)
audit            (id PK, ts, actor_user_id, action, target, detail)
user_progress    (user_id PK, xp, talk_seconds, share_seconds, watch_seconds,
                  online_seconds, messages, updated_at)
telemetry_samples(ts, user_id, key_hash, rtt_ms, path_loss, voice_loss, groups_behind,
                  rung, input_device, output_device, sample BLOB)
telemetry_events (ts, user_id, key_hash, kind, event BLOB)
```

Indexes: `messages(conv_id, msg_id)`, `messages(ts)`, `restrictions(user_id)`,
`restrictions(key_hash)`, `telemetry_samples(user_id, ts)`, `blobs(refs)`.

### 2.1 Ids and keys

- **`key_hash`** is SHA-256 of the raw 32-byte Ed25519 public key —
  what `ClientKeyProof.public_key` carries and what the 1.0 server
  already computes and logs. Not the SPKI (that is the *server*
  fingerprint's basis); for Ed25519 the SPKI is a fixed prefix plus the
  same 32 bytes, and switching would rename every id logged so far.
- **`user_id`** is the `key_hash` of the key that created the user,
  and it is permanent — it does not change when that key is later
  unlinked. It is exactly the 32-byte `UserId` on the wire
  (`Welcome.user_id`, `UserJoined.user_id`), it is stable across
  restarts even with persistence off (an admin's ban list in a text
  file still means something tomorrow), and the ids the 1.0 server
  has been logging become the real ones.
- **The derivation runs only at creation.** Everywhere else a key is
  resolved through `user_keys` (in memory: a `HashMap<key_hash,
  user_id>`), because once keys can be unlinked, `user_id` is no
  longer the hash of any current key. The 1.0 server derives it
  inline in `note_client_key`; that becomes the lookup.
- **A retired key cannot found a second user.** Its hash is taken by
  the user it created; it is refused with `NeedInvite { KeyRetired }`
  until an admin re-links it (`LinkKey`). Silently re-creating would
  undo the admin's unlink.
- **Same key on two devices is the same user** — that is what
  export/import does. `user_keys` rows beyond the first exist only for
  a *different* key an admin linked (a lost identity, a device the
  user set up fresh).

The 1.0 channel list in `server.yaml` remains the *base*: rows with
`source = config` are re-synced from the file at startup and on hot
reload; UI-created channels are `source = user` and survive restarts.
The effective list is config channels first (file order), then user
channels by `position`.

## 3. Identity, invites, and admission

The model is TeamSpeak 3's, which 1.0 already half-implements: **your
identity is the key on your device**, the server recognizes you by its
hash, roles attach to the key, and there is no registration and no
password anywhere.

### 3.1 What the invite string is today, and what it becomes

The 1.0 invite, `host:port#SHA256:…`, is the server's address plus its
identity fingerprint. It is **not a secret** — it exists so the client
can verify the server before saying hello. The room password is the
only door, everyone shares it, and locking one person out means
changing it for all.

1.1 replaces the shared secret with **invite codes** — TS3's privilege
keys: short, random, revocable, expiring, each carrying the role it
grants and recorded against the user it admitted. You join once with a
code; from then on your key *is* your membership. Nothing to rotate
when someone leaves — you ban them. There is no other door.

```
baucord://203.0.113.5:5000/k7Fq2mX9#SHA256:ECfs43…     # join: address, code, fingerprint
203.0.113.5:5000#SHA256:ECfs43…                         # 1.0 form, still parsed (verify only)
```

The URL form is registered as a scheme by the desktop app so a link
click opens the connect screen prefilled. The client stores the address
and fingerprint on the server entry as today; the code is used once and
forgotten.

```
invites: code PK (12 chars, unambiguous alphabet: no 0/O/1/l/I, case-insensitive on
         input, 60 bits — people read these out loud on voice),
         created_by, created_at, expires_at NULL, max_uses NULL, uses, grants_role, revoked_at NULL
```

**A code admits people as `guest` unless its creator chose otherwise,
and only admins get to choose.** A member can only mint guest codes;
an admin can mint guest or member codes (and, from the CLI, an admin
code for a second administrator). Defaults: `guest`, expires in 24 h,
unlimited uses; an admin may set a longer expiry or a use cap. Admins
see and revoke every code in the panel. Revoking a code stops new
joins; it never removes anyone who already used it — people leave
only by kick or ban. With persistence on, the user row records
`invited_with`, so "who let this person in" is one query.

So a person's level is set the moment they arrive, by the code, and
raised later by an admin (roster → right-click → "Make member"):

| arrived through                 | role on arrival | typical case                          |
| ------------------------------- | --------------- | ------------------------------------- |
| a guest code (the default)      | `guest`         | the friends group on a movie night    |
| an admin's member code          | `member`        | the regulars, promoted at the door    |
| the bootstrap code from the log | `admin`         | whoever runs the server               |

### 3.2 Ways in

All inside the existing `Hello.auth` enum (new variants; `Shared`
stays for 1.0 clients):

```rust
enum Auth {
    Shared { password: Option<String> },   // 1.0 clients: refused
    Key,                                    // known device: the key is the login
    Invite { code: String },                // first visit
}
```

- **Key** — the device key signs the TLS exporter (as in 1.0). If the
  key belongs to a user who is not banned, that is the login: no code,
  no password, no prompt. This is every visit after the first. It is
  stronger than a cookie: the proof is bound to this connection and
  cannot be replayed.
- **Invite** — a display name plus a valid code. The server creates the
  user, links the key, grants the code's role, counts the use. The next
  visit is `Key`. A *known* key that sends a code anyway is logged in
  as `Key` and no use is burned.
- **Shared** — the 1.0 variant, refused with `BadHello`, which a 1.0
  client already renders as *"This version of baucord doesn't match
  the server. Update baucord and try again."* — the right message,
  with no change to the old client.

**Names** are free-form and, as today, unique among the users online.
A name in use by a member's key cannot be taken by a guest — that, plus
badges that come from the key rather than the name, is all the
impersonation defense a friends server needs. Reserved names as a
feature can come with profiles.

**Second device:** Settings → "Export identity" writes the key to a
file; importing it on the other machine makes it the same user (TS3
does exactly this). Alternatively an admin links a second key to an
existing user from the panel (`user_keys`), which is also the recovery
path for a lost key. "Forget this device" deletes the local key.

**Bootstrap:** the one-shot admin invite printed in the startup log
(§1.1). With persistence on, additionally `server user grant <name>
admin` and `server invite create --grants member --expires 1h` on the
box.

**Rate limits and bans:** the 1.0 per-address failure limit bounds
invite guessing (60 bits of entropy makes it moot anyway): an
`InviteInvalid` counts as a failure, a `NeedInvite` does not. Bans are
checked at admission against `restrictions(kind=server)` by user id,
device key hash and source address, before anything else:
`Refused { Banned { until, reason } }`.

**Never lost on your own:** a user stays a user until kicked or banned.
Nothing expires a person — only codes expire.

Client side: the connect screen accepts a pasted invite (or arrives
prefilled from a link) and a name; a device the server already knows
skips straight to the roster. Settings gains "Export identity",
"Import identity" and "Forget this device".

**Later, if ever:** `Auth::Account { username, password }` for servers
that want people rather than devices. Nothing above depends on it.

### 3.3 Admission outcomes

The client needs exactly these, and nothing else, to drive its connect
screen:

| `Refused { reason }`          | when                                            | client does                                             |
| ----------------------------- | ----------------------------------------------- | ------------------------------------------------------- |
| `NeedInvite { why }`          | unknown key, no code — or a retired key (`KeyRetired`) | show the invite field: "This server needs an invite code. Ask whoever runs it."; for a retired key: "This device was removed from your user. Ask an admin to re-link it." |
| `InviteInvalid { why }`       | `expired`, `revoked`, `exhausted`, `unknown`    | say which; keep the field                               |
| `NameInUse`                   | a guest chose a name a member's key holds       | ask for another name                                    |
| `Banned { until, reason }`    | restriction `server` on key/user/address        | show it; no retry                                       |
| `BadHello`                    | a 1.0 client (password variant)                 | 1.0 client shows its "update baucord" message           |
| `ServerFull`, `RateLimited`   | as in 1.0                                       | as in 1.0                                               |

A known, unbanned key never sees any of these: `Key` → `Welcome`.

### 3.4 Rollout from 1.0

The 1.0 server only *logged* client keys; nothing migrates by itself,
and there is no transition period: the 1.1 server admits by invite
only. Day one: the operator redeems the bootstrap code from the
startup log (that key is now the admin), mints a member code in the
panel, and posts the link in the group. Everyone updates, joins once
with the code, and is a member by key from then on — permanently with
persistence on, for the run without it. Anyone still on 1.0 sees
"update baucord" until they do.

## 4. Roles and permissions

`roles.permissions` is a bitset; enforcement points already exist in
`server.rs` for shares, joins, viewer caps and images — they gain a
`has(user, PERM)` check.

| permission        | guest | member | organizer | admin | enforced at                                   |
| ----------------- | :---: | :----: | :-------: | :---: | --------------------------------------------- |
| `JOIN_CHANNELS`   |  ✔    |  ✔     |  ✔        |  ✔    | `JoinChannel`                                 |
| `SPEAK`           |  ✔    |  ✔     |  ✔        |  ✔    | voice relay (a muted user's datagrams are dropped, client told) |
| `TEXT_GENERAL`    |  ✔    |  ✔     |  ✔        |  ✔    | `Chat`                                        |
| `DM`              |  –    |  ✔     |  ✔        |  ✔    | `DirectMessage`                               |
| `SHARE`           |  –    |  ✔     |  ✔        |  ✔    | `ScreenShareStart`                            |
| `UPLOAD`          |  –    |  ✔     |  ✔        |  ✔    | file/image uploads                            |
| `CREATE_INVITES`  |  –    |  ✔     |  ✔        |  ✔    | `CreateInvite`                                |
| `CREATE_CHANNELS` |  –    |  –     |  ✔        |  ✔    | `CreateChannel`, at most `channels.max_per_user` (2) owned |
| `MANAGE_OWN_CHANNELS` | – |  –     |  ✔        |  ✔    | edit/delete channels you created              |
| `MANAGE_CHANNELS` |  –    |  –     |  –        |  ✔    | any channel; config channels can be renamed, not deleted (the file owns them) |
| `DISCONNECT`, `KICK`, `BAN`, `MUTE`, `RESTRICT` | – | – | – |  ✔  | moderation (§4.1)                             |
| `MANAGE_ROLES`, `MANAGE_USERS`, `MANAGE_INVITES` | – | – | – | ✔ | admin commands                       |
| `BYPASS_LIMITS`   |  –    |  –     |  –        |  ✔    | sharer/viewer caps, file quotas               |
| `GOLD_NAME`       |  –    |  –     |  –        |  ✔    | `SetProfile`: the gold name band (§7.1); a custom role may hold it or not |
| `PANEL`           |  –    |  –     |  –        |  ✔    | the admin panel's door (§8): the chip, `AuditLog`, `ServerStats`; each tab's data keeps its own bit |

Four built-in roles (`builtin = 1`, undeletable): `guest` (what a
code grants by default), `member` (an admin's member code, or
promotion), `organizer` (member + channels — the "special perm" for
channel creation, granted by an admin), `admin`. Nobody registers;
the code decides where you start, and an admin raises it later. Admins can add
roles and edit non-builtin permission sets from the panel. A user has
a set of roles; effective permissions are the union. The roster marks
users whose only role is `guest` with an icon and tooltip
(`UserJoined { roles }`); the name string itself is never altered.

### 4.1 Moderation actions

Three ways to remove someone from a session, with different aftermath:

| action         | connection                          | access afterwards                                              | user row                               |
| -------------- | ----------------------------------- | -------------------------------------------------------------- | -------------------------------------- |
| **Disconnect** | closed, with a reason               | unchanged — rejoining is one click, the key is the login       | kept                                   |
| **Kick**       | closed                              | join revoked until an admin lifts it (`restrictions(server)`, no `until`) | kept: name reserved, history intact |
| **Ban**        | closed                              | join revoked *and* the device key hash and source address are blocked under any identity; optional `until` | kept |
| Purge (§8)     | —                                   | —                                                              | deleted                                |

Disconnect exists for the deafened friend you cannot reach any other
way: the disconnect sound and the message get through when nothing
else does, and they are back in seconds. Kick is "out until we talk".
Ban is the one a fresh invite and a fresh key do not get around.

All three are delivered the way a refusal is: `ServerMessage::Removed
{ kind: Disconnected | Kicked | Banned { until }, reason, by }` on the
control lane, then the connection is closed 300 ms later so the
message lands. The client renders it as a final `Disconnected` — no
automatic reconnect for any of the three — with the reason and the
disconnect sound; for a plain disconnect the Connect button works
immediately. All three are audited.

### 4.2 What enforcement costs the media path

Nothing measurable. Permissions are checked on control messages —
join, chat, share, upload — never per media packet, with one
exception: a voice mute. The relay already does two `HashSet` lookups
per voice packet before fanning out (the sender's channel, and
`deafened` per recipient); a mute is one more, `muted.contains(sender)`
before the loop, on the single-threaded tick, no lock, no disk — tens
of nanoseconds, cheaper than what is already there. The restrictions
live in RAM (rule 4); the file or database is touched only when an
admin changes something. And the muted client is told
(`Restricted { voice }`) and stops sending, so the server check is the
backstop, not the path.

**Restrictions** are the ban family, one table, one shape:
`kind ∈ {server, voice, general_text, dm, upload, share}`, optional
`until`. They subtract from permissions at check time and are pushed to
the affected client as `Restricted { kind, until, reason }` so the UI
can show "you are muted until 22:30" instead of silently failing. The
relay enforces `voice` by dropping the user's datagrams (cheap: one
`HashSet<ClientId>` check per packet, already how deafen works).

## 5. Channels from the UI

```rust
ClientMessage::CreateChannel { name, password: Option<String>, user_limit: Option<u16>,
                               min_role: Option<RoleId>, permanent: bool,
                               afk_after_min: Option<u16> }
ClientMessage::EditChannel   { channel_id, … same fields … }
ClientMessage::DeleteChannel { channel_id }
ClientMessage::ChannelKick   { channel_id, user_id }          // owner/admin: to the default channel
ServerMessage::ChannelDenied { reason: ChannelDenyReason }    // NameTaken, NoPermission, ConfigOwned, Limit
```

Borrowed from TeamSpeak, because they are what make user-made channels
usable:

- **Temporary by default.** A UI-created channel that has been empty
  for `user_channels.temporary_after_min` **[default 30]** is deleted; the
  creator ticks "keep" to make it permanent. Abandoned rooms evaporate,
  and the two-per-user cap rarely bites. Config channels are always
  permanent.
- **Join gates.** `min_role` makes a channel visible and joinable only
  from that role up — a members-only room guests never see, a staff
  room. Enforced at `JoinChannel` and in what `Init` lists.
- **Owner powers.** The organizer who created a channel can edit it,
  kick a user *out of that channel* (to the default; not a server
  kick), and delete it. Admins can do all of that anywhere.
- **Auto-AFK.** One config channel is the AFK channel
  (`user_channels.afk_channel`). A user who has been idle in a channel —
  no speaking, no chat — for that channel's `afk_after_min` (its own
  value, set at creation, else the server default) is moved there by
  the server with an ordinary `UserMovedChannel` and told why. Voice
  in the AFK channel is not relayed to anyone, which is the point.
  Idle is measured on the server from events it already has; no
  client change is needed for the first version (a keyboard/mouse
  idle signal from the desktop can refine it later).
- Topic/description: later.

The server validates (same rules as `validate_channel_list`: unique
name, sane limits, and the creator owns fewer than
`user_channels.max_per_user` **[default 2]** — an admin is exempt), writes
the row, and reuses the hot-reload path to apply and broadcast
`ChannelListUpdated`. Deleting a channel moves its members to
the default, as a config removal does today. Wire channel ids stay the
list index (`u16`) the 1.0 clients understand; the stable `channel_id`
is internal and carried in `ChannelInfo` for 1.1 clients so the sidebar
can key on it across renumbering.

Sidebar: a `+` next to the channel header, shown only when the server
reported `CREATE_CHANNELS` in `Welcome.permissions`; a small dialog
(name, optional password, optional limit). Channels you own get edit/
delete in their context menu; config channels show a lock. A member at
the two-channel cap sees the `+` disabled with a tooltip.

## 6. Conversations: general and DMs

One `messages` table, two conversation kinds. The general chat is
`conv 1` and stays the one place everyone talks, whatever voice channel
they are in — the 1.0 model. A DM conversation is created on the first
message between two accounts. Per-channel text chat is deliberately
*not* in 1.1; the schema allows it later (a `kind = channel` row) if
people ask.

```rust
ClientMessage::Chat { text }                                   // unchanged: general
ClientMessage::DirectMessage { to: UserId, text, client_ref }
ClientMessage::FetchHistory { conv: ConvRef, before: Option<u64>, limit: u16 }
ClientMessage::MarkRead { conv: ConvRef, msg_id }
ServerMessage::Chat(ChatMessage)                               // unchanged: general
ServerMessage::Direct { from: UserId, msg: ChatMessage }
ServerMessage::MessageAck { client_ref, msg_id } | MessageFailed { client_ref, reason }
ServerMessage::History { conv: ConvRef, messages: Vec<ChatMessage>, more: bool }
ServerMessage::Conversations { dms: Vec<{ user_id, name, unread, last_ts }> }
```

- **General does not wait for the disk.** The server assigns `msg_id`
  from an in-memory counter (persisted watermark, rebased at startup),
  broadcasts immediately, and enqueues the insert. A dropped write
  under overload is a hole in history, never a lost delivery.
- **DMs are stored first** when there is a store. The recipient may be
  offline, so the sender gets `MessageAck` only after the insert reply
  (deadline 2 s, else `MessageFailed`). An online recipient gets
  `Direct` in the same tick; an offline one gets `Conversations` with
  unread counts at the next visit and pulls with `FetchHistory`. With
  persistence off, DMs are live only: delivered if the recipient is
  online, `MessageFailed { Offline }` otherwise, no history — the
  client says so in the conversation header. DMs need `DM` on both
  ends; a guest sender or recipient is refused with `NoPermission`,
  and the client hides the DM affordance for guests.
- **History on join:** `History` for general (last `history.replay`)
  right after `Init`; DMs on demand from the conversation list.
  Images and files in history are blob references.
- Retention prunes messages older than `history.retain_days`; blobs
  with `refs = 0` are deleted after `files.retain_days`.

## 7. Files and images

Generalize today's image chunks into a blob upload; images stay a
special case of it (rendered inline), everything else is a card.

```rust
ClientMessage::UploadBegin  { upload_id, name, mime, size, sha256 }  // Signal lane
ServerMessage::UploadAccept { upload_id, needed: bool }               // false = server already has this hash
ClientMessage::UploadChunk  { upload_id, index, count, data }         // Media lane, paced (existing path)
ServerMessage::UploadDone   { upload_id, blob_id } | UploadRejected { upload_id, reason }
ServerMessage::Message { …, attachment: Some(Attachment { blob_id, name, mime, size }) }
ClientMessage::FetchBlob    { blob_id }                               // Signal
ServerMessage::Blob… on a Media stream (existing paced relay), sha verified by the client as today
```

- **Dedup by content:** `UploadBegin` carries the hash; a known blob is
  acknowledged without transfer (`needed: false`), then referenced.
- **Without persistence** the same messages relay the file live to
  everyone online through the paced Media path — exactly how images
  work in 1.0 — and nothing is stored: no dedup, no later download,
  no card in history. The caps still apply.
- **Caps, server-enforced:** `files.max_mb` (50) per file,
  `per_user_daily_mb` (500), `total_cap_gb` (20; oldest unreferenced
  blobs evicted first, then a hard refusal). These replace the 1.0
  per-minute image budgets: images are files, the same quota applies,
  and the paced relay still bounds the instantaneous rate. Guests: no
  `UPLOAD`.
- **What is stored:** bytes on disk by hash, metadata in `blobs`. The
  server sniffs the first bytes and stores *its* MIME next to the
  declared one; the card shows the sniffed type. Filenames are
  sanitized for display (no control characters, length-capped) and the
  download is served with a fixed name the client chooses, never a path.
- **Rendering:** images (`image/png|jpeg|gif|webp`) inline as today;
  anything else a card with name, type, human-readable size, and a
  Download action that fetches on demand into the user's Downloads
  folder. Nothing is executed or previewed.
- The relay never opens the file. Same trust model as chat: the server
  can read everything; that is documented, not hidden.

### 7.1 Profiles: name color, name style, avatar

Everyone, guests included, can set how their name looks. It is stored
on the user row, so it follows the key across visits and devices.

```rust
ClientMessage::SetProfile   { name_color: Option<u32>, name_style: NameStyle, avatar: Option<BlobId> }
ServerMessage::ProfileUpdated { user_id, profile: Profile }
// `Init` and `UserJoined` carry `profile` for every user; chat and roster render from it.
```

- **Color** is an RGB value the server validates for contrast against
  the app's chat background (`#1a1d21`; minimum contrast ratio 3:1)
  and refuses otherwise — nobody is unreadable or invisible. The client
  offers a palette of ~16 known-good colors and a custom picker that
  greys out failing values.
- **Gold is a permission of its own** (`GOLD_NAME`, 1.1.3; before
  that, `ADMIN`), and the server is the gate, not the picker: a color
  in the gold band (hue 38°–65°, saturation ≥ 0.35) is refused with
  `ProfileDenied { ReservedColor }` unless the user has it. The
  built-in admin role holds it; a custom role may hold it or lack it,
  so "an administrator without the gold band" is `server role create
  steward --like admin --deny gold`. The client's picker greys the
  band out for everyone else,
  but that is courtesy; the check that matters runs on the server, and
  a client that skips the picker gets the same refusal. Because it is
  enforced, a gold name *is* trustworthy — the one deliberate
  exception to the rule below.
- **Style** is `normal | bold | italic`. System lines in the chat keep
  their own muted treatment so a user's italic never reads as a
  notice.
- **Avatar** is a blob (§7): the client resizes to 128×128 before
  upload, PNG/JPEG/WebP, `avatars.max_kb` (256), no animation for now.
  Cached by hash on every client, so it transfers once per client per
  change. Shown in the roster and next to the name in chat at the size
  the UI decides.
- **Other than gold, colors never mean roles.** A name color is only a
  name color. The role markers 1.1.0 ships are the guest icon (§4) and
  the server-enforced gold band above; badges for organizer and admin
  are a later addition, and when they come they are icons derived from
  the key's roles. That is what keeps "I picked admin red" from meaning
  anything.
- **Rate limit:** one profile change per 30 s per user; over it,
  `ProfileDenied { RateLimited }`.
- **Admin:** `ResetProfile { user_id, lock: bool }` clears it and,
  with `lock`, sets `profile_locked` (a `profile` restriction) so it
  cannot be set again until lifted. Audited like everything else.

The client keeps its profile locally and sends it in `SetProfile`
right after `Welcome` on every connect. With persistence on the server
also stores it on the user row and it survives the client's settings
being lost; with persistence off the server only validates and
broadcasts, and the avatar rides in as a live blob (256 KB, cached by
hash on the others' clients). Either way the server's validation — the
contrast check, the gold band, the size cap — is the same.

## 8. Admin: the panel, the channel, and why it can't be faked

**What the client shows** is driven by `Welcome.permissions` (a bitset)
and by `UserJoined { user_id, kind, roles }`. A client can draw whatever
badge it likes on its own screen; other clients draw badges from the
server's roster data, and the server's roster comes from `user_roles`.
There is no path by which a client asserts a role.

**Admin commands** are ordinary messages checked like everything else,
and they ship on the wire in 1.1.0: with persistence off there is no
CLI, so this is the only way to administer a memory-mode server. The
first front end is slash commands typed in the admin channel
(`/mute @name 30m reason`); the panel comes in 1.1.3.

```rust
ClientMessage::Admin(AdminCommand)
enum AdminCommand {
    ListUsers, ListRestrictions, AuditLog { before, limit },
    SetRoles { user_id, roles }, CreateRole { name, permissions }, EditRole { … },
    Restrict { target: UserId | Key | Address, kind, until, reason }, Lift { restriction_id },
    Disconnect { user_id, reason }, Kick { user_id, reason }, Ban { user_id, until, reason },
    MoveUser { user_id, channel_id },
    DeleteMessage { msg_id }, DeleteChannel { channel_id },
    CreateInvite { grants_role, expires, max_uses }, RevokeInvite { code },
    LinkKey { user_id, key_hash }, UnlinkKey { key_hash },
}
ServerMessage::AdminResult { request_id, result }
```

Every command writes an `audit` row (actor, action, target, detail) and
the persistence thread does the writes; the tick does the permission
check and the in-memory state change (a ban takes effect before the row
lands — the row is the record, not the gate).

**The admin channel** is a text conversation visible only to users with
`ADMIN`: the audit log is streamed into it as system messages ("mpw
muted oveeer for 30 min: spam"), and admins can also type commands
there (`/mute @oveeer 30m spam`) which the client turns into the same
`AdminCommand` messages — one authorization path, two front ends. The
panel (a tab: users, roles, restrictions, channels, files) is the
full-featured one; the channel is the fast one during a session.

**Bootstrap** is the only step that needs host access by design: the
one-shot admin code in the startup log, or `server user grant <name>
admin` on the box with persistence on. Admins cannot demote the last
admin, and with persistence on the CLI can lift any restriction —
including one an admin put on themselves by mistake — so the host is
always the way back in; with persistence off, a restart is.

**Purge:** `PurgeUser { user_id }` (panel or `server user purge`)
deletes the person's messages, files and row; the audit keeps a line
saying it happened. Cheap now, painful later.

**Connection info:** admins hovering a user in the roster see that
user's last telemetry (RTT, loss, devices, rung). The data already
arrives every 10 s; the tooltip turns a log investigation into a glance.

## 9. Stats and telemetry in the database

Answering the question directly: yes, move the *queryable* part of
telemetry into the database, keep the log lines.

- The log stays the live view (`tail -f` during a session, the tooling
  you already have).
- The persistence thread also stores each `TelemetrySample` as a row
  with the columns worth querying pulled out — `rtt_ms`, `path_loss`,
  `voice_loss`, `groups_behind`, `rung`, `input_device`,
  `output_device` — and the whole sample as a blob for anything else.
  Events (`ShareStarted`, `RateRungChanged`, `Disconnected`, …) go to
  `telemetry_events`. Retention `telemetry.retain_days` (30).
- Coalescing under load: one row per user per sample interval, last
  wins; never a backlog.

That makes "what device did oveeer have at 21:44 and what was his loss
before it" a query (`server telemetry <user> --since 21:40`) instead of
a grep across three log files — which is exactly what the last two
investigations were.

### 9.1 Progress: xp and levels

The foundation, deliberately small. `user_progress` is updated once a
minute **on the persistence thread**, from events the tick already
emits — presence, speaking on/off, share start/stop, subscribe,
messages — never per packet. In memory for the session with
persistence off.

| activity                                   | xp             |
| ------------------------------------------ | -------------- |
| online                                     | 1 / min        |
| talking, with someone else in the channel  | 5 / min        |
| watching a share                           | 2 / min        |
| sharing, with at least one viewer          | 10 / min       |
| a chat message (after the rate limit)      | 1              |

Talking and sharing count only with someone else there, so an empty
room farms nothing. **Level is derived from xp, never stored:** the
next level costs `250 × level^1.5` xp (levels 1–10 ≈ four hours of
ordinary use, 20 ≈ a couple of months). Changing the curve later is a
formula change, not a migration.

Wire: `UserDetails` carries `level`; `ProgressUpdate { xp, level }`
goes to the user on level-up; feature bit `PROGRESS` (bit 5). Shown in
the roster tooltip and the profile. No achievements.

## 10. Wire shape, feature bits, compatibility

**Postcard is positional and the protocol crate forbids new fields on
existing structs.** So nothing here adds a field: everything is a
*new variant*, sent only when its bit is negotiated —
`Session { user_id, permissions, persistent }` right after `Welcome`;
`UserDetails { client_id, user_id, roles, level, profile }` after each
`UserJoined` and as a batch after `Init`; `ChannelDetails { stable id,
min_role, owner, permanent, afk_after_min }` after `Init` and
`ChannelListUpdated`; `Removed`, `Restricted`, `ProfileUpdated`,
`ProgressUpdate`, `MessageEdited`, `MessageDeleted`, `History` (paged
in ≤64 KiB batches, `more: true`), and so on. Where earlier sections
wrote `Welcome.permissions` or `ChannelInfo.channel_id`, read them as
these variants.

**Feature bits** (`protocol::features`): the allocated `PROFILES` (2),
`HISTORY` (3), `ROLES` (4), `BLOBS` (6, the spec's files) stay; bit 5,
formerly `ACHIEVEMENTS`, becomes `PROGRESS` (never announced by any
server, so free); new `INVITES` (7), `DM` (8), `CHANNELS_UI` (9),
`ADMIN` (10). The server starts announcing `IDENTITY`.

**1.0 clients are refused.** They can only send `Auth::Shared`; the
server answers `BadHello`, which the 1.0 client renders as "update
baucord". The 1.1 client drops `Auth::Shared`; `ServerEntry.password`
stays in `settings.json` as an ignored legacy field so old files load.
A legacy `authentication:` section in `server.yaml` is warned about and
ignored, not fatal. No ALPN bump: the handshake succeeds and the
refusal carries the message.

**Names** are enforced at admission: any online duplicate is
`NameInUse`; with persistence on, a guest also cannot take a stored
member's name while that member is offline. The name of record is
refreshed from each `Hello`. One deliberate exception: a `Key` login
for a user who *already has a live session* — their old connection
still in its idle window after a blackout, which auto-reconnect makes
routine — is a **replacement**: the old session gets `Removed
{ Replaced }` and the new one is admitted. Never `NameInUse` against
yourself.

**Wall clock.** The tick runs on `Instant` (fake in the simulator);
invite expiry, `until` and `ts` need wall time. A `Clock` on
`ChatServer` derives it as `base_wall + (now − base_instant)`, so the
simulator's fake `Instant` advances wall time for free and an
"expired invite" test simply runs the clock forward.

## 11. Delivery plan — status

Each step was independently releasable and left the media path
untouched. All of it is shipped; the versions are the client's and
server's (they move together).

1. **1.1.0 — foundation.** Persistence thread, schema, migrations,
   `backup`. Invites (privilege keys) as the only door, key identity,
   export/import. Roles with the four built-ins and the restriction
   family. General-chat history replay, edit/delete, rate limit.
   Channels from the UI. Telemetry rows and the admin roster tooltip.
   The admin CLI. The first test written: "persistence thread stalled
   for 10 s — voice and video unaffected, writes counted as dropped,
   history answers empty with a retry hint."
2. **1.1.1 — DMs.** Direct messages with offline delivery, unread
   counts, `FetchHistory` paging, the conversation list.
3. **1.1.2 — profiles.** Name color and style, avatars over the blob
   path, the contrast check, admin reset/lock; the app's own context
   menus with the volume slider, the collapsible Messages section,
   the invite dialog, the XP bar (`UserProgress`), the share sounds,
   one instance per profile.
4. **1.1.3 — roles, files, the admin panel.** Custom roles with
   derived ranks and badges (`server role`, `RoleList`), `panel`,
   `gold` and `admin-channel` as permissions, files on the blob path
   in General and DMs (cards with a transfer ring and cancel), the
   admin panel (users, roles, restrictions, invites, channels, files,
   audit, server), a resizable sidebar.
5. **1.1.4 — the rest of this document.** Blob retention in the hourly
   prune, purge dropping the person's files, chat xp, pictures as
   files wherever the server keeps files (only a memory-mode server
   still gets the live `ImageMessage` relay, and refuses files), the
   transfer budget in place of the image one, `server restore` and
   `server db prune`, the admin channel (the audit as it happens over
   `AuditLine`, commands at hand), keybinds watched rather than
   grabbed.

**Later, if ever:** channel topics, reserved names, a manual away
toggle shown in the roster, animated avatars, anything built on
progress (a leaderboard, level-gated invites), `Auth::Account` only
if a server ever wants people rather than devices.

Deliberately not taken from TeamSpeak: nested channel trees and
spacers (a flat list is right at this scale), whisper lists, the
granular permission matrix (a bitset is enough), identity security
levels (invites gate entry), priority speaker (revisit with a
share-time use case).

Testing follows the existing pattern: every server behavior is an
`e2e.rs` scenario on the simulator with a real SQLite file in a temp
dir — "guest cannot DM", "ban takes effect mid-session", "files are
posted as cards and replayed", "persistence thread stalled for 10 s:
voice unaffected, writes counted as dropped".

## 12. Decisions taken

- **Profile is its own variant (1.1.2, in use).** §10's
  `UserDetails { …, profile }` predates 1.1.0 shipping `UserDetails`
  with four positional fields, and the protocol crate forbids a
  fifth. The look travels as `UserProfile { client_id, profile }`
  right after each `UserDetails` (batch after `Init`), changes as
  `ProfileUpdated { user_id, profile }`, refusals as `ProfileDenied {
  ReservedColor | LowContrast | RateLimited | Locked | BadAvatar }`;
  all behind `PROFILES`. A live rename is `SetNickname { name }` →
  `UserRenamed { client_id, name }` / `RenameDenied { Taken |
  Invalid | RateLimited }` under the same bit, checked with the
  `Hello` name rules. The avatar rides §7's blob messages
  (`UploadBegin/Accept/Chunk/Done/Rejected`, `FetchBlob`,
  `BlobChunk`, `BlobMissing`, bit `BLOBS`), which for now admit only
  avatar-sized images (`avatars.max_kb`); files as cards, daily
  quotas and eviction land on the same messages later. The lock is
  the `profile` restriction; `profile_locked` on the user row stays
  unused. Chat rows carry names only, by choice — the tile lives in
  the roster and the hover card.
- **Kick forgets, ban blocks (1.1, in use).** A kick is not a
  restriction: it closes the session and deletes the person's device
  keys and roles, so they need a new invite and come back as whatever
  it grants. There is nothing to lift. A ban is the `server`
  restriction (user, key, address; optional `until`), lifted by
  `/unban` or the CLI. §4.1's "join revoked until an admin lifts it"
  is therefore the ban.
- **No invite links.** People are told an address, a port and a code;
  `baucord://` was dropped because a portable app registers no scheme
  and the link was unreadable when spoken. §3.1's link form is gone.
- **Admins edit admins.** `SetRoles` allows equal rank (the last admin
  still keeps `admin`); kick, ban and mute keep refusing an equal or
  higher rank.
- **`min_role` channels stay listed.** Wire ids are list positions, so
  a per-client list would let two clients disagree on what id 3 is.
  Everyone sees every channel; `ChannelDetails.min_role` tells the
  client what to hide, and `JoinChannel` is where it is enforced.
- **Config channels are not renamed from the UI.** The file is the
  source; a rename made in memory would not survive the next reload.
  `EditChannel` on a config channel answers `ConfigOwned`.
- **Progress accrues on the tick.** Once a minute over at most 64
  sessions is cheaper than the events it would take to move it to the
  persistence thread, and it works with persistence off.
- **Disconnect reaches anyone; kick and ban spare administrators
  (1.1.3, in use).** §4.1's rank rule stays for mutes, restrictions,
  roles and purges. A disconnect costs the target a reconnect, so
  anyone holding `DISCONNECT` may drop anyone but themself. A kick or
  a ban is refused only when the target holds `ADMIN`; equal or
  higher rank below that is fair game — the built-in admins are the
  people who can undo it, and they are the ones protected. The
  roster menu mirrors this (`menu::UserFacts::is_admin`).
- **A new wire variant gets a new feature bit.** Postcard is
  positional: a client that negotiated a bit and then meets a variant
  appended later under the same bit cannot decode it and the
  connection is lost. `UserProgress` therefore rides `PROGRESS_ALL`,
  not `PROGRESS` (a 1.1.0 client has the latter), and `RoleList`
  rides `ROLE_LIST`, not `ROLES`. The rule going forward: append the
  variant, allocate a bit, gate the send on it.
- **Everyone's xp is on the wire (1.1.1, in use).** §9.1's
  `UserDetails.level` alone cannot draw a bar, so `UserProgress {
  client_id, xp, level }` (behind `PROGRESS`) goes to a newcomer for
  the whole roster (itself included), to everyone for a newcomer, and
  to everyone once a minute for each user whose xp moved — at most
  64² small control messages a minute. The client derives the
  fraction with `progress::xp_for_level` / `cost_of_level`, so the
  curve stays a server-side formula.
- **Chat earns no xp.** Presence, talking with someone there, watching
  and sharing to a viewer do; a message does not, so there is nothing
  to spam for. `user_progress.messages` still counts them.
- **The replay ring serves memory mode too.** With persistence off a
  joiner still gets the last `history.replay` of this run from the
  ring; only paging beyond it is empty. The "server forgets" notice
  stays.
- **The default channel is a lobby (1.1.5, decided 2026-09-16).**
  Connecting no longer exposes you. The lobby is not a config
  channel: the server appends one of its own (empty name, so it can
  collide with nothing; `config::with_lobby`) after the config and UI
  channels, and `default: true` in a file is ignored with a warning
  (decided 2026-09-17: a room named "Lobby" is a room). The lobby is
  a waiting room: from it you see the channel list with
  everyone in the channels your role can see, and you read General —
  but nobody sees you. The server leaves lobby members out of `Init`'s
  memberships, sends no `UserJoined`/`UserDetails` for them, reports
  them offline in DM presence, relays no voice from them (as the AFK
  channel already does), shows them no shares and accepts none from
  them, and grants them no xp. The moment a lobby user picks a
  channel, the server announces them (`UserJoined`, `UserDetails`,
  membership) and the client renders **"X joined <channel>"**
  (`chat-user-joined-channel`) instead of "X joined"; disconnecting
  from a channel renders "X left"; disconnecting from the lobby
  renders nothing, because nothing was ever announced. Admin moves and
  the auto-AFK keep their own lines. Posting in General from the
  lobby is allowed: a message carries the name, so posting is a
  choice to be seen, the same as joining. DMs work both ways. There
  is no "step out" action — leave a channel by disconnecting, or move
  to AFK; to be invisible again, disconnect and reconnect. The
  client's *automatic* reconnect (after a blackout) rejoins the
  channel it was in, remembered password included; a manual connect
  or a relaunch lands in the lobby. The client hides the lobby row
  and shows "not in a channel" at the top of the list, only when the
  server advertises feature bit `LOBBY` (bit 16) — against an older
  server nothing is hidden. Older clients on a new server simply
  never see lobby users, render their own lobby row empty, and —
  like everyone — must pick a channel to be heard; the greeting
  message is the operator's place to say so during the rollout. The
  admin panel lists lobby users with channel "lobby"; kick, ban and
  `MoveUser` reach them by name; `max_clients` counts them. `ChannelKick`
  sends its target to the lobby, which is what "out" now means.
- **No xp in the AFK channel either (1.1.5).** The presence minute
  accrues only in a channel that is neither the lobby nor AFK; the
  other sources already need company and cannot occur in either.
- **An image is a message with a blob (1.1.5).** Its menu is *Open
  image*, *Save image*, *Copy image*, then *Delete* under the same
  author-or-moderator rule as text, never *Edit*. The server always
  allowed this; only `app-core::menu` returned early for images.
- **Badges: gold is a colour, the band is the permission (1.1.5).**
  Gold joins the role-badge palette (admins assign badges, so a gold
  badge signifies what the admin says it does); the name *band* stays
  `GOLD_NAME`'s. The built-in admin badge turns green, as a theme
  token.
- **Drag a user onto a channel (1.1.5).** A `DragArea` on the user
  row, a `DropArea` on each channel row, `AdminCommand::MoveUser` on
  drop; the drag exists only for sessions holding `MANAGE_CHANNELS`,
  which the server enforces regardless. Full or gated targets answer
  with the existing `ChannelDenied` reasons.
- **The LIVE chip opens the viewer (1.1.5).** Same path as the
  context menu's "Watch stream".
- **`delete-messages` is its own permission (1.1.5, decided
  2026-09-17).** Bit 23, name `delete-messages`, in the admin set and
  therefore available to custom roles; `AdminCommand::DeleteMessage`
  needs it and nothing else, and nothing implies it — `RESTRICT`
  included, so a custom role that could delete by restricting keeps
  restricting and loses deleting until the box is ticked. DMs stay
  undeletable for everyone. The panel lists it by name like the rest.
- **Log level in Settings → General (1.1.5).** `logging.level` in
  `settings.json` (`error`…`trace`, default `info`), applied live
  through `log::set_max_level` with env_logger's filter left open;
  `RUST_LOG` in the environment overrides the setting entirely. The
  echo-cancellation design is in `AEC-SPEC.md`.

- Chat is general + DMs; no per-channel text (schema leaves the door open).
- Guests: icon + tooltip from `UserJoined { kind }`; the name is never altered.
- Files: 50 MB per file, 500 MB per user per day, 20 GB total.
- Identity is the device key, TS3-style: no registration, no passwords.
  Invites admit guests by default; admins can mint member codes and
  raise people from there. Channel creation is the `organizer` role,
  two channels per user.
- Invites are the only door, in both modes. No password anywhere:
  1.0 clients get "update baucord", 1.1 clients get "needs an invite".
- Policy (roles, invites, restrictions, channels) is always available
  in memory; persistence on also writes it to SQLite and switches on
  the data features. The config file is never written by the process.
- Bootstrap is a one-shot admin invite printed in the startup log, in
  both modes; "first key is admin" is gone (unsafe under auto-reconnect).
- A user never loses access on their own — only kick or ban; revoking
  a code only stops new joins.
- Everyone, guests included, can set a name color, a name style and an
  avatar; colors pass a contrast check. The gold band is reserved for
  the `GOLD_NAME` permission (the admin role's, grantable to custom
  roles) and enforced by the server; apart from that, colors never
  signify a role. The guest icon and gold are the role markers in
  1.1.0; other badges come later as icons from the key's roles.
- Disconnect, kick and ban are three actions: disconnect keeps access
  (rejoin in one click), kick revokes it until lifted, ban also blocks
  the device and address. None of them auto-reconnects; all keep the
  user row.

- Wire changes are new variants only (postcard is positional); 1.0
  clients are refused with "update baucord"; a legacy `authentication:`
  section is warned about and ignored.
- No CLI with persistence off; `Admin` commands on the wire from
  1.1.0. Progress (xp/levels) replaces achievements.

## 13. Why the sharer and viewer caps stay

The bitrate ladders handle a *user's* weak link: a sharer whose uplink
falls behind steps down, a viewer whose downlink falls behind loses
groups and keeps voice. Nothing handles the **relay's egress**: a
sharer's bitrate is multiplied by its viewer count, the sharer's ladder
sees only its own uplink, and when the relay's outbound link saturates
every viewer goes behind at once and nobody slows down. `max_sharers`
(8) and `max_viewers_per_stream` (16) are the operator's blunt cap on
that multiplication; `BYPASS_LIMITS` lifts them for admins. The
per-minute image budgets are gone (files quotas cover them). The right
replacement, later, is an egress budget — `limits.relay_egress_mbit` —
against which the relay refuses new subscriptions and asks the busiest
sharer to step a rung down; that needs relay-side rung control the
sharer honours, which is a small protocol addition on top of the
existing `StreamHealth`.
