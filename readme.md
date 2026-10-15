# baucord

A lightweight voice, text, and screen sharing application built with Rust,
for Windows, macOS, Linux, Android and iPhone.

Uses [Slint](https://slint.dev) for the UI on every platform, QUIC
(`quinn-proto`, driven from the app's own threads with no async runtime)
for network transport, and FFmpeg for video on the desktop (MediaCodec on
Android, VideoToolbox on iOS). Audio runs on dedicated threads with
crossbeam channels. Screen sharing follows the same architecture:
capture → encode → network → decode → viewer. The server is a simple
relay — no mixing, no transcoding.

Working on the code? Start with [`START-HERE.md`](START-HERE.md).
[`ARCHITECTURE.md`](ARCHITECTURE.md) is how the pieces fit together,
[`PRODUCTION-PLAN.md`](PRODUCTION-PLAN.md) what is left.

## Workspace

| crate       | what it is                                                          |
| ----------- | ------------------------------------------------------------------- |
| `protocol`  | wire format: messages, lanes, framing, voice/video packing          |
| `transport` | QUIC without a runtime: driver, identity + pinning, UDP, simulator  |
| `server`    | the dedicated relay (lib + bin, so integration tests can drive it)  |
| `client`    | networking, settings, and the voice/screen worker threads           |
| `app-core`  | platform-agnostic app logic: roster/chat view models, image pipes   |
| `frontend`  | the UI logic every host shares: `AppState`, the Slint models, the   |
|             | settings/admin pages; host-only bits behind the `desktop` feature   |
| `desktop`   | the desktop host (binary: `baucord`): `main.rs` and nothing else    |
| `ui`        | the Slint UI and its Rust glue                                      |
| `i18n`      | Fluent translations — English, Español (AR), Português (BR)         |
| `audio`     | capture/playback, Opus, VAD, denoise, jitter buffer, rate ladder,   |
|             | echo cancellation, the AAudio/VPIO/WASAPI backends                  |
| `video`     | H.264 encode/decode (FFmpeg, MediaCodec, VideoToolbox), capture     |
| `android`   | the Android host (a cdylib; the Gradle project is `android/`)      |
| `ios`       | the iPhone host (a binary; the Xcode project is `ios/`)            |

## Features

- **Voice chat** — Opus over QUIC datagrams with voice activity
  detection, per-user volume/mute (right-click any user; settings
  persist per peer), and an adaptive jitter buffer with arrival-time
  jitter estimation, burst-safe drain, and per-peer limiting. The
  encoder's bitrate/redundancy ladder is driven by what the relay
  actually received of your voice, reported back every two seconds
- **Voice channels** — server-defined rooms (TeamSpeak style).
  Connecting puts you in the **lobby**: you see who is where, nobody
  sees you, and nothing you say goes anywhere until you pick a
  channel — that is when the others get "X joined General". Channels
  can have a join password and a user limit. The channel list is
  **hot-reloadable**:
  edit `server.yaml` while the server runs and connected clients resync
  in place — nobody is ever kicked, members of a removed channel are
  moved to the lobby
- **Screen sharing** — H.264 via FFmpeg (with the source's audio on
  Windows), 720p or
  1080p at 30 or 60 fps. Streams are visible to **your channel only or
  the whole server** (sharer's choice, per share). Video travels in
  keyframe-led groups: a viewer that falls behind loses whole groups,
  never its voice, and resumes at the next keyframe; the sharer's
  bitrate ladder steps on the groups its own uplink abandoned. Viewers
  get a resizable, aspect-ratio-preserving window with live stats and
  can request keyframes for fast recovery
- **Drag and drop** — drag a user onto a channel to move them (needs
  the permission); drop a file or picture from the file manager onto
  the window to send it.
- **Text chat** — one room for the whole server regardless of voice
  channel, with message history, inline links, images, gifs, and an
  emoji picker. Images are relayed on a paced stream so a 10 MB paste
  never dents anyone's voice. Joins, leaves and shares go to an
  Activity view behind General's header instead of between the
  messages
- **Accounts without registration** — your device's key is your
  identity; an invite code lets a new key in, with the role the invite
  carries. Roles (built-in and custom, with badges), moderation (mute,
  kick, ban, restrictions), and an admin panel for users, roles,
  invites, channels, files, reports and the audit log
- **Block, report, delete your account** — block someone to stop
  seeing and hearing them (and their DMs); report a message or a
  person to the server's moderators; delete your account on a server,
  with everything you posted, from Settings
- **History, DMs and files** — the server keeps General's history and
  replays it, direct messages are delivered when the other side comes
  back, files and pictures travel by content hash
- **Profiles** — a name colour and style, an avatar, levels
- **Multiple servers** — save several servers in settings, pick a
  default, switch from the connect screen. Every connection is
  encrypted and the server's identity is pinned — from an invite
  string (`host:port#SHA256:…`) or on first use; a server whose key
  changed is refused before anything is sent
- **Stays connected** — a dropped session (Wi-Fi roam, ISP hiccup,
  server restart) reconnects on its own with backoff; short blackouts
  under 30 s don't even end the session
- **Global hotkeys and push-to-talk** — toggle mute, deafen, and
  screen share from any window, or hold a key to talk; rebind with
  live key capture (desktop)
- **Echo cancellation** — the platform's own canceller, verified on the
  stream that was opened (Windows and Android; written for iPhone and
  awaiting confirmation on a device)
- **Three languages** — English, Español (Argentina), Português (Brasil)
- **Stays out of the way** — closes to the tray with the call running;
  the client idles around 40 MB of RAM on Windows (more on macOS and
  Linux, where the figure counts shared pages)
- **Cross-platform** — Windows, Linux and macOS, plus Android (a
  sideloaded APK) and iPhone with the same engine and UI: voice in the
  background, stream viewing, and notifications for DMs and a call
  starting while the app stays connected

## Building and Running

### Prerequisites

- Rust 1.95+ (`rust-toolchain.toml` pins it; edition 2024)
- FFmpeg shared libraries (for screen share encode/decode)
- Formatting uses nightly-only options: `cargo +nightly fmt --all`

#### Windows: building FFmpeg with vcpkg

Windows doesn't have a system package manager for FFmpeg, so it's built
locally via [vcpkg](https://github.com/microsoft/vcpkg) with the codec
feature set baucord needs (H.264 via x264, plus hardware encode paths
for NVIDIA and AMD):

```powershell
git clone https://github.com/microsoft/vcpkg.git C:\vcpkg
cd C:\vcpkg
git checkout 34823ada10080ddca99b60e85f80f55e18a44eea
.\bootstrap-vcpkg.bat
.\vcpkg.exe install ffmpeg[x264,gpl,nvcodec,amf]:x64-windows-static-md --recurse
```

The checkout pins vcpkg to its FFmpeg 7.1.2 port: `ffmpeg-next` 8 does
not build against the FFmpeg that vcpkg's HEAD ships. `qsv` is left
out on purpose, as in the release build.

This can take 30+ minutes the first time — it's compiling x264 and the
hardware codec bindings from source, not just fetching a binary.

Add vcpkg to your `PATH` (or point `FFMPEG_DIR` at the built output) so
the build can find it:

```powershell
$env:FFMPEG_DIR = "C:\vcpkg\installed\x64-windows-static-md"
```

Add that line to your PowerShell profile if you don't want to set it
every session. Once it's set, `cargo build` / `cargo run -p desktop`
will pick up the libraries automatically.

### Tests

```bash
cargo test --all
```

The transport and server suites run entirely in-process on a simulated
network with a fake clock (delay, jitter, loss, bandwidth, address
changes), so "a viewer on a 0.5 Mbit/s link watching an 8 Mbit/s share"
or "a 20 s blackout" are ordinary unit tests that finish in milliseconds.
No sockets, no timing flakiness.

### Server

```bash
cargo run -p server --release
```

Listens on `0.0.0.0:51000` by default. It was 5000 until 1.1.5; some
ISP gateways police UDP/5000 and people on them heard each other at
16 kbps until the relay moved. A server still on 5000 says so at
startup. To move one without a flag day, keep both doors open in
Docker while saved addresses catch up — `"51000:51000/udp"` and
`"5000:51000/udp"` both map to the one socket — and set
`public_address` to the new port so invites carry it. Configuration
lives in `server.yaml` (written with defaults on first start;
`--config`, `--host`, `--port` override). The parts you will touch:

```yaml
server:
  name: Baucord Server
  public_address: "203.0.113.5:51000"  # only used to print the invite
identity:
  dir: data/baucord                     # identity.key lives here — back it up
logging:
  link_stats_interval_secs: 2           # per-client `link:` lines (0 = off)
  telemetry: true                       # log clients' `telemetry:` samples
  file: server.log                      # "" = stderr only (containers)
channels:
  - name: General
  - name: Gaming
    password: "hunter2"                 # optional
    user_limit: 8                       # optional
```

Omit the `channels` key entirely for a single open `General`. The lobby
is not a channel in this list: the server adds it itself, everyone
connects into it, and it never shows in a client that knows it. A
`default: true` left over from older files is ignored with a warning.
The
`channels` section hot-applies on file save while the server runs; a
bad edit is logged and ignored, never applied. Everything else in the
file needs a restart.

Every connection is QUIC over UDP, encrypted, and authenticated by the
server's Ed25519 identity key (generated on first start in
`identity.dir`; clients pin its fingerprint). Since 1.1 the door is an
**invite**: a client's device key is its account, and a key the server
does not know needs an invite code (failures are rate-limited per
address). On any start while nobody holds the admin role, the server
logs a one-shot admin invite (`No admin yet. Bootstrap invite
(admin, 1 use, 1 h): code …`) — use it from your client, then make
invites from the admin panel or with `server invite create`. The 1.0 `authentication` section
(a shared room password) is ignored, with a warning.

The startup log prints the address and the identity (`Address for
clients: host:port — identity SHA256:…`); hand both to people (with an
invite code) so their client verifies the server before saying hello. To rotate the key, stop the server and run
`server rotate-key` — the new key is signed by the old one, so pinned
clients migrate automatically.

`SIGTERM`/`SIGINT` close every session with a "server restarting"
reason; clients reconnect on their own about two seconds later, so a
deploy is a blink, not a 30-second dropout.

### Client

```bash
cargo run -p desktop --release   # binary is named `baucord`
```

Pick a nickname, pick a server, connect. Settings persist to
`settings.json` — next to the executable when that directory is
writable (portable installs), otherwise in the platform config dir
(`%APPDATA%\baucord`, `~/.config/baucord`, `~/Library/Application
Support/baucord`). The client's own identity key (`identity.key`) lives
next to it; it signs the handshake so the server can derive a stable
user id, and never leaves the machine.

One setting has no switch in the app: `"pointer": false` under
`"share"` in `settings.json` keeps the mouse pointer out of a Windows
screen share (it is drawn in by default).

A server entry holds the address, the pinned fingerprint, an invite
code waiting to be used, and the channel passwords you saved. Paste an invite string into the identity field to verify
a server up front; leave it empty and it is pinned on the first
successful connection.

Client and server must speak the same protocol: the QUIC ALPN string
(`baucord/1`) is bumped on every breaking wire change, and older peers
are refused at the handshake with an "update baucord" message instead
of misbehaving mid-call. Non-breaking capabilities are negotiated as
feature bits in `Hello`/`Welcome`.

## Deploying a server

The production layout is Docker Compose on a Linux host. The guide
for running one is on the website:
[baucord.com/deploy.html](https://baucord.com/deploy.html)
(`web/deploy.html`). What follows is what a maintainer needs beside
it: how the image and the published files come to be.

The compose file runs the published image `mpwsh/baucord:latest`,
which `docker.yaml` builds from the `Dockerfile` and pushes on every
release (amd64 only; also tagged with the version and the commit; a
pre-release does not move `latest`). If a release's image build fails,
*Run workflow* on the `docker` workflow with the release's tag rebuilds
and pushes it. For another
architecture or unreleased code, replace `image:` with `build: .`.
`web.yaml` publishes `docker-compose.yaml`, `scripts/setup.sh`,
`scripts/monitor-net.sh` and `config/baucord.example.yaml` (as
`baucord.yaml`) to `baucord.com/deploy/` whenever they change, and
puts the compose file into the guide. A host needs only those four
files, not a clone.

```
client ──udp/51000 (QUIC)─▶ baucord   (relay, published directly)
```

One service: everything — media, control, admission — rides the single
QUIC port, so there is no HTTP issuer, no reverse proxy and no
certificate to provision. Media UDP is published straight from the
`baucord` container (not through a userspace proxy: that added copies
per packet and hid real client addresses from the relay).

The image is distroless and runs as uid `10000`; the identity key is
kept in `./data/baucord` on the host so `docker compose up` recreates
keep the identity every client has pinned. Back that directory up. The
container config sets `logging.file: ""` — Docker captures stderr, and
that is the log; compose keeps it to 5 × 10 MB.

### Layout

```
Dockerfile          # cargo-chef build, distroless runtime
docker-compose.yaml
config/
  baucord.example.yaml  # a new host starts here (a server test keeps it loading)
  baucord.yaml      # this host's config (the folder is mounted at /app/config)
scripts/
  setup.sh          # bootstrap / re-apply host + stack
  monitor-net.sh    # UDP / NIC drop logger
data/baucord/       # created by setup.sh; identity key, database, blobs, uploaded client logs (keep out of git, back it up)
```

### Running one

Setting up a host, reading the server's log, the network monitor and
host networking are the operator's guide, and it lives in one place:
[baucord.com/deploy.html](https://baucord.com/deploy.html)
(`web/deploy.html`). It used to be repeated here, to be changed
together; now a change to how a server is run is a change to that
page.

## Known Limitations

The full, prioritized list is [`PRODUCTION-PLAN.md`](PRODUCTION-PLAN.md);
what a user notices:

- No end-to-end encryption: every hop is TLS-encrypted QUIC and the
  server is pinned, but the relay sees all media in the clear
- Phones get notifications only while the app stays connected; there is
  no push service yet, so a closed app hears nothing
- Server addresses are IP:port only (no host names yet)
- Channel identity in `server.yaml` is the channel _name_: renaming one
  counts as remove + create, so a hot reload moves its members to the
  lobby
- Channel passwords are stored in plain text on both ends
  (`server.yaml`, the database, `settings.json`) — they're shared room
  secrets, not personal credentials; treat them accordingly
- Speaking indicators are global: you see the green dot for people in
  other channels even though you can't hear them
- A server whose identity changed must be accepted by editing (or
  clearing) the pin in Settings; there is no accept/reject dialog yet
- Screen sharing on Wayland falls back to slow screenshots; no echo
  cancellation on macOS or Linux yet

## Licence

baucord is Apache-2.0. A desktop build as a whole is distributed under
the GNU GPL version 3, because it links FFmpeg and x264 statically; the
phone apps do not. Every third-party part and its licence is listed in
the apps (Settings → Licenses) and on the website
([baucord.com/licenses.html](https://baucord.com/licenses.html)),
refreshed by CI before every build — see
[`licenses/README.md`](licenses/README.md).
