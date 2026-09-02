# baucord

A lightweight voice, text, and screen sharing application built with Rust.

Uses [Slint](https://slint.dev) for the UI, QUIC (`quinn-proto`, driven
without a runtime) for network transport, and FFmpeg for video
encoding/decoding. Audio runs on
dedicated threads with crossbeam channels. Screen sharing follows the same
architecture: capture → encode → network → decode → viewer window. The
server is a simple relay — no mixing, no transcoding.

## Workspace

| crate      | what it is                                                         |
| ---------- | ------------------------------------------------------------------ |
| `protocol` | wire format: messages, transport channels, voice/video packing     |
| `server`   | the dedicated relay (lib + bin, so integration tests can drive it) |
| `client`   | networking, settings, and the voice/screen worker threads          |
| `app-core` | platform-agnostic app logic: roster/chat view models, image pipes  |
| `desktop`  | the desktop shell (binary: `baucord`)                              |
| `ui`       | the Slint UI and its Rust glue                                     |
| `i18n`     | Fluent translations — English, Español (AR), Português (BR)        |
| `audio`    | capture/playback, Opus, VAD, denoise, jitter buffer                |
| `video`    | FFmpeg H.264 encode/decode, screen capture                         |

## Features

- **Voice chat** — Opus over UDP with voice activity detection, per-user
  volume/mute (right-click any user; settings persist per peer), and an
  adaptive jitter buffer with arrival-time jitter estimation, burst-safe
  drain, and per-peer limiting
- **Voice channels** — server-defined rooms (TeamSpeak style). Everyone
  lands in the default channel (the Lobby); channels can have a join
  password and a user limit. The channel list is **hot-reloadable**:
  edit `server.yaml` while the server runs and connected clients resync
  in place — nobody is ever kicked, members of a removed channel are
  moved to the default
- **Screen sharing** — H.264 via FFmpeg with the source's audio, 720p or
  1080p at 30 or 60 fps. Streams are visible to **your channel only or
  the whole server** (sharer's choice, per share). Viewers get a
  resizable, aspect-ratio-preserving window with live stats; viewer loss
  and RTT feed back to the sharer for rate adaptation, and viewers can
  request keyframes for fast recovery
- **Text chat** — one room for the whole server regardless of voice
  channel, with message history, inline links, images, gifs, and an
  emoji picker
- **Multiple servers** — save several servers in settings, pick a
  default, switch from the connect screen. Every connection is
  encrypted and the server's identity is pinned (from an invite
  string, or on first use); secure servers additionally require a
  shared room password
- **Global hotkeys** — toggle mute, deafen, and screen share from any
  window; rebind with live key capture
- **Three languages** — English, Español (Argentina), Português (Brasil)
- **Stays out of the way** — closes to the tray with the call running;
  the whole client idles around 30–50 MB of RAM
- **Cross-platform** — Windows, Linux, and macOS

## Building and Running

### Prerequisites

- Rust 1.95+ (workspace uses edition 2024)
- FFmpeg shared libraries (for screen share encode/decode)

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

### Server

```bash
cargo run -p server --release
```

Listens on `0.0.0.0:5000` by default. Configuration lives in
`server.yaml` (written with defaults on first start; `--config`,
`--host`, `--port` override). Channels look like this:

```yaml
channels:
  - name: Lobby
    default: true # exactly one channel must be the default
  - name: Gaming
    password: "hunter2" # optional
    user_limit: 8 # optional
```

Omit the `channels` key entirely for a single open Lobby. The
`channels` section hot-applies on file save while the server runs; a
bad edit is logged and ignored, never applied. Everything else in the
file needs a restart.

Every connection is QUIC over UDP, encrypted, and authenticated by the
server's Ed25519 identity key (generated on first start in
`identity.dir`, default `data/baucord`; clients pin its fingerprint).
Open mode (`authentication.secure: false`) then accepts anyone and is
meant for LAN/dev — the server logs loudly about it. Secure mode
requires clients to send `authentication.password` in their hello
(checked in constant time, failures rate-limited per address).

The startup log prints the identity and an invite string,
`host:port#SHA256:…`; hand that to people so their client verifies the
server before saying hello. To rotate the key, stop the server and run
`baucord-server rotate-key` — the new key is signed by the old one, so
pinned clients migrate automatically.

### Client

```bash
cargo run -p desktop --release   # binary is named `baucord`
```

Pick a nickname, pick a server, connect. Settings persist to
`settings.json` — next to the executable when that directory is
writable (portable installs), otherwise in the platform config dir
(`%APPDATA%\baucord`, `~/.config/baucord`, `~/Library/Application
Support/baucord`).

Client and server must be from the same release: the protocol id is
bumped on every breaking wire change, and older peers are refused at
the handshake instead of misbehaving mid-call.

## Deploying a server

The production layout is Docker Compose on a Linux host:

```
client ──udp/5000 (QUIC)──▶ baucord   (relay, published directly)
```

One service: everything — media, control, admission — rides the single
QUIC port, so there is no HTTP issuer, no reverse proxy and no
certificate to provision. Media UDP is published straight from the
`baucord` container (not through a userspace proxy: that added copies
per packet and hid real client addresses from the relay).

The `baucord` image runs as uid `10000`; the identity key is kept in
`./data/baucord` on the host so `docker compose up` recreates keep the
identity every client has pinned. Back that directory up.

### Layout

```
docker-compose.yml
config/
  baucord.yaml      # mounted as /app/server.yaml
scripts/
  setup.sh          # bootstrap / re-apply host + stack
  monitor-net.sh    # UDP / NIC drop logger
data/baucord/       # created by setup.sh; holds identity.key (keep out of git, back it up)
```

### Bootstrapping a host

1. Open `udp/5000` in the cloud firewall. (No DNS or TLS certificate
   is needed: the server's own key is its identity.)
2. Set `server.public_address` in `config/baucord.yaml` to the host's
   public IP and port (it is printed in the invite string), and
   `authentication.password` to the room secret.
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
   - prints the invite string (`host:port#SHA256:…`) from the log

   The settings block at the top of the script (`MEDIA_PORT`,
   `MEDIA_VIA_DOCKER_NAT`, `BAUCORD_UID`, interface auto-detection) is
   the only thing you should need to touch.

4. Check it came up:

   ```bash
   docker compose ps
   docker compose logs -f baucord
   ss -uln | grep :5000
   ```

### Monitoring while it runs

A relay is a UDP fan-out — a ~177 KB keyframe goes to every viewer at
once — and the kernel drops those bursts silently; you see idle
timeouts and abandoned video groups, not errors. Run the monitor during load (a 200-person test,
or just always):

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

- No end-to-end encryption: every hop is TLS-encrypted QUIC and the
  server is pinned, but the relay sees all media in the clear; open
  mode (no password) is for LAN/dev
- Push-to-talk is reserved in the settings format but not implemented
  yet — voice activation only for now
- Chat history is not replayed to new joiners; you see the room from
  the moment you connect
- Channel identity is the channel _name_: renaming one in `server.yaml`
  counts as remove + create, so a hot reload moves its members to the
  default channel
- Channel and server passwords are stored in plain text on both ends
  (`server.yaml`, `settings.json`) — they're shared room secrets, not
  personal credentials; treat them accordingly
- Speaking indicators are global: you see the green dot for people in
  other channels even though you can't hear them
- Resampling between different audio sample rates is basic
- Screen share viewer creates a new FFmpeg decoder per keyframe
  (stateless recovery, not optimal for bandwidth)
