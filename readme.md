# baucord

A lightweight voice, text, and screen sharing application built with Rust.

Uses `fltk` for the UI, `renet` for network communication, and `ffmpeg` for video encoding/decoding.

Audio runs on dedicated threads with crossbeam channels.
Screen sharing follows the same architecture: capture → encode → network → decode → viewer window.
The server is a simple relay — no mixing, no transcoding.

## Features

- **Voice chat** with voice activity detection (VAD) and per-user volume controls
- **Screen sharing** with H.264 encoding (FFmpeg), aspect-ratio-preserving viewer, and stream audio capture
- **Text chat** with message history
- **Global hotkeys** for toggle mute, toggle deafen, and toggle screen share (configurable in settings)
- **Per-user audio controls** — right-click a user to mute or adjust their volume
- **Adaptive jitter buffer** with arrival-time jitter estimation, burst-safe drain, and per-peer limiting
- **Cross-platform** — Windows, Linux, and macOS

## Building and Running

### Prerequisites

- Rust 1.87+
- CMake (for FLTK)
- FFmpeg shared libraries (for screen share encode/decode)

### Server

```bash
cargo run --bin server --release
```

Starts on `127.0.0.1:5000` by default. See `config.yaml` for configuration.

### Client

```bash
cargo run --bin client --release
```

Enter a nickname and server address to connect.

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
net.core.wmem_max     = 8388608
net.core.wmem_default = 8388608
net.core.rmem_max     = 8388608
net.core.rmem_default = 1048576
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

## What Works

- Voice transmission via VAD (automatic) or push-to-talk (configurable)
- Mute, deafen, and screen share toggles via UI buttons or global hotkeys
- Screen sharing at 720p/1080p with configurable quality, viewer window with volume/mute controls and live stats (FPS, throughput, total data transferred)
- Per-user mute and volume adjustment (right-click context menu)
- Audio settings: input/output device selection, gain, gate threshold
- Keyboard shortcut configuration with live key capture
- Settings persist to `settings.json` next to the binary

## Known Limitations

- Resampling between different audio sample rates is basic
- No end-to-end encryption
- Server is a relay with no authentication
- Screen share viewer creates a new FFmpeg decoder per keyframe (stateless recovery, not optimal for bandwidth)
