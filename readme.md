# baucord

A lightweight voice, text, and screen sharing application built with Rust.

Uses [Slint](https://slint.dev) for the UI, `renet` + `renet_netcode` for
network transport, and FFmpeg for video encoding/decoding. Audio runs on
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
  default, switch from the connect screen. Secure servers hand out
  netcode connect tokens via a small HTTP issuer (optional shared
  password)
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

Open mode (`authentication.secure: false`) accepts anyone and is meant
for LAN/dev — the server logs loudly about it. Secure mode requires
clients to present a netcode connect token from the built-in HTTP token
issuer (see `authentication.issuer`, optional shared password; the
signing key is generated on first secure start).

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

### server setup

```
# ── UDP socket buffer tuning ─────────────────────────────────────────
#
# Linux defaults are ~208 KB for send and ~1 MB for receive.
# Baucord relays video keyframes in bursts (~177 KB per keyframe per
# viewer), so we need larger buffers to avoid silent packet drops
# that cause netcode timeouts.
#
# 8 MB send/recv gives headroom for multiple concurrent streams.
cat > /etc/sysctl.d/99-baucord-udp.conf <<EOF
# Baucord — UDP socket buffer tuning for video relay
net.core.wmem_max = 67108864
net.core.rmem_max = 8388608
EOF

systemctl disable --now docker 2>/dev/null || true
systemctl disable --now containerd 2>/dev/null || true

# ── Disable cloud-init network hotplug ───────────────────────────────
mkdir -p /etc/cloud/cloud.cfg.d
cat > /etc/cloud/cloud.cfg.d/99-disable-hotplug.cfg <<EOF
updates:
  network:
    when: []
EOF
```

## Known Limitations

- No end-to-end encryption: secure mode protects the transport with
  netcode connect tokens, but the relay sees all traffic in the clear;
  open mode is for LAN/dev and shouldn't cross the internet
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