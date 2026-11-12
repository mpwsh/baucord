# baucord-gateway — a bot's door to a baucord server

A bot is a program that connects to a baucord server as a user of its
own, reads what happens there, writes to the chat and plays audio in a
voice channel. Speaking the server's own protocol means QUIC, postcard
and an Ed25519 key, which is fine in Rust and a tax in every other
language — so the gateway does that part once. On one side it is a
client of the server; on the other it serves a WebSocket on the
loopback where a bot in any language reads JSON events, sends JSON
actions, and streams raw PCM to play. A Python bot is thirty lines
(§3).

What exists today (1.4.0): everything a 1.3 client can do that a bot
needs — admission by invite then by key, the roster, chat and DMs both
ways, history, channel moves, nicknames, and voice *out* (the bot
plays; it does not listen). A bot in Rust needs no gateway process at
all: the same code is a library, `gateway::bot::Bot` (§3.2), and
`examples/music.rs` is a complete music bot built on it — one binary,
files and URLs decoded in-process, nothing to install. What waits on
the server: slash commands with ephemeral replies (`COMMANDS`, the
review document's bot section, steps 1–2) and the `bot` role. Until
then a bot reacts to chat lines (`!play …`) and is an ordinary member.

For people who will not open the repository, the same ground is
covered on the website: [baucord.com/bots.html](https://baucord.com/bots.html)
(`web/bots.html`; keep the two in step).

## 1. Running it

```
baucord-gateway --server play.example.com:51000 --name MusicBot \
    --key ./musicbot.key --invite ABCD-EFGH-IJKL
```

| Flag (env) | What |
|---|---|
| `--server` (`BAUCORD_SERVER`) | `host:port` of the server. |
| `--name` (`BAUCORD_BOT_NAME`) | The bot's name on the server. |
| `--key` (`BAUCORD_KEY`, default `bot.key`) | The bot's identity key, made on the first run. **Whoever has this file is the bot.** Keep it like the server's `identity.key`; never commit it. |
| `--invite` (`BAUCORD_INVITE`) | An invite code (`server invite create`, or `/invite` in the app). Needed on the first run only: it links the key, and from then on the key alone logs in. A used code is forgotten by the gateway, so leaving the flag in a service file is harmless. |
| `--pin` (`BAUCORD_PIN`) | The server's identity, `SHA256:…` (the server prints it at start; Settings → Servers shows it). Without one the first connection's identity is trusted and written to a `.pin` file beside the key (`musicbot.key` → `musicbot.pin`), and every later run insists on it — the app's trust-on-first-use. |
| `--listen` (`BAUCORD_GATEWAY_LISTEN`, default `127.0.0.1:8765`) | Where bots connect. |
| `--token` (`BAUCORD_GATEWAY_TOKEN`) | A secret the bot must present: `Authorization: Bearer …` or `?token=…`. Required when `--listen` is not the loopback — anyone who reaches the socket *is* the bot. |
| `--voice-kbps` (`BAUCORD_VOICE_KBPS`, default 64) | Opus bitrate for what the bot plays, 16–128. |

The log (`RUST_LOG=info` by default) says who it is, what it pinned,
when it was admitted, and when a bot connects. It reconnects by itself
when the link drops (one second, doubling to thirty); it exits with
status 1 when the server says no (a bad code, a kick, a ban, the
server's identity changed without a successor proof) — a supervisor
should not restart it in a loop then, since nothing changes until an
operator acts. Ctrl-C closes the session properly (the server sees a
goodbye, not a 30 s timeout).

Several bots may connect to one gateway; they share the one identity
and all see every event. One bot per gateway is the normal shape.

### Docker

`crates/gateway/Dockerfile` builds the same binary the server's
image does, on the same base; the voice path compiles libopus, so the
builder stage installs `cmake`. Run it beside the server with the key
on a volume:

```
docker run -v musicbot:/data -e BAUCORD_SERVER=server:51000 \
    -e BAUCORD_BOT_NAME=MusicBot -e BAUCORD_KEY=/data/musicbot.key \
    -e BAUCORD_INVITE=ABCD-EFGH-IJKL -e BAUCORD_GATEWAY_LISTEN=0.0.0.0:8765 \
    -e BAUCORD_GATEWAY_TOKEN=… -p 127.0.0.1:8765:8765 baucord-gateway
```

## 2. The wire

One WebSocket, text frames of JSON each way, binary frames for audio.
Every JSON frame is an envelope `{"op": …, "t": …, "d": {…}}`: `op`
says what kind of frame, `t` names it, `d` carries its fields.
Identifiers that are 64-bit (`client_id`, message ids' authors) travel
as **strings of decimal digits**, user ids as 64 hex digits, so
JavaScript keeps every bit. Message ids, timestamps (ms) and channel
ids are plain numbers.

### 2.1 From the gateway: `hello`, `status`, `error`

The first frame on a connection:

```json
{"op":"hello","d":{"gateway":"baucord-gateway 1.4.0","connected":true,"ready":{…}}}
```

`ready` is the same object `READY` carries (below), or `null` while the
gateway has no session. Then, as the session comes and goes:

```json
{"op":"status","t":"CONNECTED","d":{"client_id":"…","user_id":"…","server_features":…,"fingerprint":"SHA256:…"}}
{"op":"status","t":"DISCONNECTED","d":{"reason":"…","was_connected":true,"reconnecting":true}}
{"op":"status","t":"REFUSED","d":{"reason":"…","kind":"InviteInvalid { … }"}}
{"op":"status","t":"REMOVED","d":{"kind":"Kicked","reason":"…","by":"…"}}
```

A frame the gateway could not use comes back as
`{"op":"error","d":{"message":"…","action":"CREATE_DM"}}` — a JSON the
gateway does not know, an action with a field missing, an action sent
before the session is `READY` ("not connected to the server yet"), or
a refusal the server answered with words (a nickname it would not
take).

### 2.2 Events: `dispatch`

`{"op":"dispatch","t":"<name>","d":{…}}`. Discord's names where the
thing is the same.

| `t` | `d` | When |
|---|---|---|
| `READY` | `client_id`, `user_id`, `permissions`, `server_name`, `greeting`, `channels: [{id, name, has_password, user_limit}]`, `members: [{client_id, name, channel_id, user_id, roles, level}]` | Admitted and the roster received. `channel_id` is `null` for someone in the lobby. |
| `SESSION` | `user_id`, `permissions` (a bit set, `protocol::permissions`), `persistent` | What this bot may do on this server. |
| `MEMBER_ADD` / `MEMBER_REMOVE` | `client_id`, `name`, `channel_id` / `client_id`, `name` | Someone connected or left. |
| `MEMBER_UPDATE` | `client_id` and what changed: `name`, or `user_id`, `roles`, `level` | A rename; roles and level (also right after `MEMBER_ADD`). |
| `VOICE_STATE` | `client_id`, `channel_id` | Someone moved channel — the bot too, after `JOIN_CHANNEL`. |
| `CHANNELS` | `channels`, `memberships: [{client_id, channel_id}]` | The channel list changed. |
| `JOIN_DENIED` | `channel_id`, `reason` | A `JOIN_CHANNEL` the server refused. |
| `MESSAGE_CREATE` | `id`, `ts`, `client_id`, `user_id`, `name`, `text`, `conv` (`"general"` or `{"dm":"<user id>"}`), `own` (the bot's own line echoed back), `attachment` (`{blob_id, name, mime, size}`, when it is a file) | A line in General, a DM to the bot, a file. |
| `MESSAGE_UPDATE` / `MESSAGE_DELETE` | `id`, `text` / `id` | An edit, a deletion. |
| `MESSAGE_ACK` / `MESSAGE_FAILED` | `client_ref`, `id` / `client_ref`, `reason` | The fate of a `CREATE_DM`. |
| `RATE_LIMITED` | `retry_ms` | The bot writes too fast; the line was dropped. |
| `HISTORY` / `HISTORY_UNAVAILABLE` | `conv`, `messages: [{id, ts, user_id, name, text, edited}]`, `more` / `conv`, `retry_secs` | The answer to `FETCH_HISTORY`. |
| `SPEAKING` / `AUDIO_STATE` | `client_id`, `speaking` / `client_id`, `muted`, `deafened` | Lights in the roster. |
| `SHARE_START` / `SHARE_STOP` | `client_id`, `width`, `height`, `everyone` / `client_id` | Screen shares (the bot cannot watch them). |
| `VOICE_REPORT` | `received`, `expected`, `lost`, `late`, `rtt_ms` | Every 2 s while the bot plays: what the server got of it. |
| `RESTRICTED` / `RESTRICTION_LIFTED` | `kind`, `until`, `reason` / `kind` | A mute or a text restriction on the bot. |
| `ROLES` | `roles: [{id, name, rank, permissions, builtin}]` | The server's role table. |
| `MOVED_IDLE` | `after_min` | The server moved the bot to the AFK channel. |
| `CHANNEL_CLOCK` | `channel_id`, `running_ms` (`null` when empty) | A channel's timer started or stopped (1.5); a bot counts on from `running_ms`. |
| `RECORDING_START` / `RECORDING_STOP` | `channel_id`, `rec_id`, `elapsed_ms`, `by` / `channel_id`, `rec_id`, `duration_ms`, `reason`, `by` | A channel started or stopped recording (1.5). A bot in it is recorded too. |
| `RECORDING_READY` | `rec_id`, `channel`, `started_at`, `duration_ms`, `file_name`, `size`, `blob_id` | A recording the bot was in is ready, from Server. |

### 2.3 Actions: `action`

`{"op":"action","t":"<name>","d":{…}}`. Each becomes the client
message a person's app would send; the server checks the bot's
permissions as it would anyone's.

| `t` | `d` |
|---|---|
| `CREATE_MESSAGE` | `text` — a line in General (one text room today). |
| `CREATE_DM` | `user_id`, `text`, `client_ref` (optional; comes back in `MESSAGE_ACK`). |
| `EDIT_MESSAGE` / `DELETE_MESSAGE` | `id`, `text` / `id` — own messages, or with `DELETE_MESSAGES`. |
| `JOIN_CHANNEL` | `channel_id`, `password` (optional). **Voice goes to the channel the bot is in**: join one before playing; the lobby's voice is dropped. |
| `SPEAKING` | `speaking` — the light; the voice sink sets it by itself. |
| `AUDIO_STATE` | `muted`, `deafened`. |
| `SET_NICKNAME` | `name`. |
| `FETCH_HISTORY` | `conv` (default `"general"`), `before` (a message id, optional), `limit` (default 50, at most 200). |
| `MARK_READ` | `conv`, `id`. |

### 2.4 Voice: binary frames

A binary frame is PCM: 32-bit little-endian floats, 48 kHz, mono, in
`[-1, 1]`, any length (a multiple of 4 bytes). The gateway cuts it
into 20 ms frames, encodes them as Opus (`Application::Audio`, the
bitrate of `--voice-kbps`) and sends one every 20 ms, so a bot may push
a whole song at once — up to ten seconds ahead, after which its socket
waits (back-pressure), which is how `ffmpeg … | bot` pacing works out.
The speaking light goes on with the first frame and off 200 ms after
the last. `{"op":"voice","t":"STOP"}` drops what is queued (the bot
stopped the song). Stereo sources are the bot's to mix down (`-ac 1`
in ffmpeg); the apps decode mono.

## 3. Three bots

### 3.1 In Python, through the gateway

`pip install websockets`. The frames are in `examples/`.

`examples/echo_bot.py` — answers `!ping` with `pong` and greets whoever
joins:

```python
import json
from websockets.sync.client import connect

with connect("ws://127.0.0.1:8765/") as ws:
    hello = json.loads(ws.recv())
    print("gateway", hello["d"]["gateway"], "connected", hello["d"]["connected"])
    while True:
        f = json.loads(ws.recv())
        if f["op"] != "dispatch":
            continue
        if f["t"] == "MESSAGE_CREATE" and not f["d"]["own"] and f["d"]["text"] == "!ping":
            ws.send(json.dumps({"op": "action", "t": "CREATE_MESSAGE", "d": {"text": "pong"}}))
        if f["t"] == "MEMBER_ADD":
            ws.send(json.dumps({"op": "action", "t": "CREATE_MESSAGE",
                                "d": {"text": "hi " + f["d"]["name"]}}))
```

`examples/music_bot.py` — `!play <url>` joins the asker's channel and
pipes `yt-dlp` through `ffmpeg` into the voice sink; `!stop` stops. It
reads the asker's `channel_id` from `READY`'s roster and the
`VOICE_STATE` events it keeps up to date, and pushes ffmpeg's stdout
in 100 ms pieces — the gateway's back-pressure paces it. Its reason to
exist is `yt-dlp`: YouTube and the sites it knows.

### 3.2 In Rust, as one binary: `examples/music.rs`

```
cargo run -p gateway --example music -- --server play.example.com:51000     --name MusicBot --key musicbot.key --invite ABCD-EFGH-IJKL --library ~/Music
```

The same flags as the gateway (§1) plus `--library`, the folder
`!play <file>` may read from (never outside it; without the flag, URLs
only). In chat: `!play <file or url>` plays, or queues when something
is playing (the bot answers with the place in the line); `!skip`;
`!stop` (the queue goes too); `!queue`; `!help`. It decodes in-process
with symphonia (mp3, aac/m4a, flac, ogg/vorbis, wav) and resamples to
48 kHz mono with rubato, the crates the audio engine already uses; a
URL is fetched as it plays, so an internet radio stream plays until
`!stop`. A link to a YouTube, Spotify, SoundCloud or Bandcamp *page*
is refused with a line saying so: those are pages about a track, and
`music_bot.py`'s job. It says little in chat on purpose: the server's chat limit
(5 lines in 5 s) applies to a bot, and a chatty one loses the lines
that matter.

What it is built on is the library behind the binary:

```rust
use gateway::{bot::Bot, net::Config, wire::{Action, Outbound}};

let bot = Bot::connect(config, 64)?;          // threads of its own; returns at once
for event in bot.events() {                   // Outbound::Status / Dispatch / Error
    if let Outbound::Dispatch { t: "MESSAGE_CREATE", d } = &event
        && d["text"] == "!ping"
    {
        bot.act(Action::CreateMessage { text: "pong".into() });
    }
}
```

`bot.play(&samples)` plays f32 48 kHz mono in the bot's channel (it
blocks when ten seconds are queued — a decoder calling it runs at the
room's pace); `bot.stop_playing()` cuts it within a frame; `bot.voice()`
and `bot.actor()` are handles for other threads; `bot.shutdown()` says
goodbye to the server. Everything else — admission, the pin, reconnect
— is the gateway's, shared.

## 4. Security

- The bot's permissions are its roles on the server, checked there;
  nothing the gateway or the bot says changes them.
- The key file is the identity. The gateway writes it `0600`; a bot
  never sees it (the bot has the WebSocket, not the key).
- On the loopback no token is needed: whoever runs programs on the
  machine already owns the bot. Elsewhere `--token` is mandatory, and
  put TLS in front (a reverse proxy) — the gateway speaks plain `ws://`.
- A bot that stops reading its frames is dropped after 4096 queued
  frames, not buffered without bound; a bot pushing audio faster than
  the room plays waits on its socket.
- Chat rate limits apply to the bot as to anyone (`RATE_LIMITED`).

## 5. What is next

In the order of the review document's bot section: the `bot` role and
`/bot create` (step 1), slash commands with ephemeral replies (step 2:
`INTERACTION_CREATE` in, `INTERACTION_REPLY` out, the names are
reserved here), then voice *in* (binary frames of what the room says,
for a recorder or a soundboard that reacts), `BOT_QUIET` to spare the
bot the roster's lights, and the gateway built into the release and
the Docker image by CI.
