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
|             | settings/admin windows; host-only bits behind the `desktop` feature |
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
  never dents anyone's voice
- **Accounts without registration** — your device's key is your
  identity; an invite code lets a new key in, with the role the invite
  carries. Roles (built-in and custom, with badges), moderation (mute,
  kick, ban, restrictions), and an admin panel for users, roles,
  invites, channels, files and the audit log
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
- **Global hotkeys** — toggle mute, deafen, and screen share from any
  window; rebind with live key capture
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

- Rust 1.95+ (`rust-toolchain.toml` pins it; workspace uses edition 2024)
- FFmpeg shared libraries (for screen share encode/decode)
- Formatting uses nightly-only options: `cargo +nightly fmt --all`

#### Windows: building FFmpeg with vcpkg

Windows doesn't have a system package manager for FFmpeg, so it's built
locally via [vcpkg](https://github.com/microsoft/vcpkg) with the codec
feature set baucord needs (H.264 via x264, plus hardware encode paths
for NVIDIA/AMD/Intel):

```powershell
git clone https://github.com/microsoft/vcpkg.git C:\vcpkg
cd C:\vcpkg
.\bootstrap-vcpkg.bat
.\vcpkg.exe install ffmpeg[x264,gpl,nvcodec,amf,qsv]:x64-windows-static-md
```

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

The production layout is Docker Compose on a Linux host.

> **As committed, the deploy files lag this text** (PRODUCTION-PLAN.md
> OPS-1): `docker-compose.yaml`, `config/baucord.yaml`, the `Dockerfile`
> and `scripts/setup.sh` still use UDP 5000, and `setup.sh` looks for
> `docker-compose.yml` and stops. Until that is fixed, rename or edit
> accordingly, and read the address and the bootstrap code from the log
> yourself: `docker compose logs baucord | grep -E 'Address for
> clients|Bootstrap invite'`.

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
that is the log.

### Layout

```
Dockerfile          # cargo-chef build, distroless runtime
docker-compose.yaml
config/
  baucord.yaml      # mounted as /app/server.yaml
scripts/
  setup.sh          # bootstrap / re-apply host + stack
  monitor-net.sh    # UDP / NIC drop logger
data/baucord/       # created by setup.sh; identity key, database, blobs, uploaded client logs (keep out of git, back it up)
```

### Bootstrapping a host

1. Open `udp/51000` in the cloud firewall. (No DNS or TLS certificate
   is needed: the server's own key is its identity.)
2. Set `server.public_address` in `config/baucord.yaml` to the host's
   public IP and port (the server prints it in its `Address for
   clients` line).
3. Clone this directory onto the host and run:

   ```bash
   sudo scripts/setup.sh
   ```

   It is idempotent — re-run it after pulling config changes. It:

   - checks for Docker and the compose plugin and installs them from the
     distro repos only if missing
   - creates `./data/baucord` owned by uid 10000 (the identity key is
     generated there on first start)
   - tunes the host for UDP fan-out: socket buffer caps and kernel
     ingress backlog (`/etc/sysctl.d/99-baucord-udp.conf`), conntrack
     sizing for Docker's port publishing, a boot-time service for NIC
     ring buffers / RPS, and disables cloud-init network hotplug
   - runs `docker compose up -d`
   - prints the address and identity line from the log (and, on a new
     server, the bootstrap admin invite — see "Server" above)

   The settings block at the top of the script (`MEDIA_PORT`,
   `MEDIA_VIA_DOCKER_NAT`, `BAUCORD_UID`, interface auto-detection) is
   the only thing you should need to touch.

4. Check it came up:

   ```bash
   docker compose ps
   docker compose logs -f baucord
   ss -uln | grep :51000
   ```

### Reading the server log

Two periodic lines carry most of what you'll ever need:

- `link: <user>: rtt, cwnd, path loss, tx/rx, mtu, dgram drops, groups
  behind N/M (cause), voice rx/expected/lost/late` — the relay's view of
  one client every `link_stats_interval_secs`. *Path loss* is the
  transport's own accounting and informational; *voice lost* is what
  listeners actually missed; *groups behind* is how often that client's
  downlink couldn't keep up with a share (the cause breakdown says
  which rule gave up).
- `telemetry: <user>: …` — the client's own numbers every 10 s: path,
  audio ladder, encoder rung/fps when sharing, decode stats when
  viewing.

A rising loss curve with flat RTT is the client's network; a rising RTT
with low loss is a queue somewhere — ours or a router's.

### Monitoring while it runs

A relay is a UDP fan-out — a ~177 KB keyframe goes to every viewer at
once — and the kernel drops those bursts silently; you see abandoned
video groups and idle timeouts, not errors. Run the monitor during load
(a 200-person test, or just always):

```bash
sudo nohup scripts/monitor-net.sh 10 >/dev/null 2>&1 &   # every 10s
tail -f /var/log/baucord-net.log
```

UDP counters are per network namespace, so the script samples the host
*and* the `baucord` container (`CONTAINERS="baucord other"` to add
more, `CONTAINERS=""` for host only) and writes one CSV row per
namespace per interval. Every `*_err` / `*_drop` column is the delta for
that interval, and `note` names the layer that dropped:

| note            | meaning                                                      |
| --------------- | ------------------------------------------------------------ |
| `RCVBUF`        | receive buffer overflow — app not draining, or `rmem` small  |
| `SNDBUF`        | send buffer overflow — keyframe burst > `wmem`, or link full |
| `SOFTNET`       | kernel backlog overflow — `netdev_max_backlog` / CPU bound   |
| `NIC`           | NIC ring or cloud pps/bandwidth allowance — see `ethtool -S` |
| `VETH`          | drops on the Docker bridge / veth                            |
| `COUNTER_RESET` | the container restarted                                      |

`max_txq_bytes` creeping toward `wmem_max` during keyframes means you're
close to `SNDBUF` drops — pace keyframes rather than raising the cap.
Sustained `softirq_pct` above ~30% means packet processing is CPU bound.
`ct_count` approaching `ct_max` means conntrack is about to start
dropping new clients.

If you leave it running long-term, add a logrotate rule:

```
# /etc/logrotate.d/baucord-net
/var/log/baucord-net.log { daily rotate 7 compress missingok copytruncate }
```

### Moving to host networking (optional)

For the last bit of headroom, run `baucord` with `network_mode: host`
instead of `ports:`. That removes Docker's DNAT and conntrack from the
media path. Then set `MEDIA_VIA_DOCKER_NAT="false"` in
`scripts/setup.sh` and re-run it — that enables a `notrack` rule for the
media port, which is only safe without DNAT.

## Known Limitations

The full, prioritized list is [`PRODUCTION-PLAN.md`](PRODUCTION-PLAN.md);
what a user notices:

- No end-to-end encryption: every hop is TLS-encrypted QUIC and the
  server is pinned, but the relay sees all media in the clear
- Push-to-talk is reserved in the settings format but not implemented
  yet — voice activation only for now
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
