#!/usr/bin/env bash
# scripts/setup.sh — bootstrap (or re-apply) a baucord relay host.
#
# Run as root from the folder that holds docker-compose.yaml,
# config/baucord.yaml and scripts/ (the files from
# https://baucord.com/deploy/, as the guide lays them out):
#     sudo scripts/setup.sh
#
# config/baucord.yaml is this host's server config, started from
# https://baucord.com/deploy/baucord.yaml. The guide:
# https://baucord.com/deploy.html
#
# Idempotent — re-run after pulling config changes. It:
#   1. checks Docker + the compose plugin are present (installs from Ubuntu repos if not)
#   2. creates ./data/baucord for the server identity key (container runs as uid 10000)
#   3. tunes the host for UDP fan-out (sysctls, conntrack, NIC ring buffers, RPS)
#   4. pulls the image and starts / updates the compose stack (one
#      service: baucord)
#   5. prints the server's address and identity from its log, and on a
#      new server the one-time admin invite code
#   6. installs + enables the network monitor as a systemd service
#      (baucord-monitor — CSV at /var/log/baucord-net.log, logrotate managed)
#
# Everything in step 3 is host-level: net.core.* sysctls are global and apply inside
# containers; NIC settings are physical; conntrack backs Docker's port publishing.
set -euo pipefail
cd "$(dirname "$0")/.."

# ── Settings ──────────────────────────────────────────────────────────────────
MEDIA_PORT="${MEDIA_PORT:-}" # UDP port baucord listens on; empty = server.port from config/baucord.yaml
MEDIA_VIA_DOCKER_NAT="true" # true  = baucord uses `ports:` (iptables DNAT -> conntrack required)
# false = baucord runs network_mode: host (media port can skip conntrack)
BAUCORD_UID="10000"       # uid:gid the mpwsh/baucord image runs as (user "runner")
DATA_DIR="./data/baucord" # mounted at /app/data; holds identity.key (the server's identity — back it up)
IFACE="${IFACE:-$(ip -o route show default | awk '{print $5; exit}')}"
# ──────────────────────────────────────────────────────────────────────────────

[[ $EUID -eq 0 ]] || {
  echo "run as root: sudo scripts/setup.sh" >&2
  exit 1
}
COMPOSE_YAML=""
for f in docker-compose.yaml docker-compose.yml compose.yaml compose.yml; do
  [[ -f "$f" ]] && { COMPOSE_YAML="$f"; break; }
done
[[ -n "$COMPOSE_YAML" ]] || {
  echo "no docker-compose.yaml here; run from the folder that holds it (see https://baucord.com/deploy.html)" >&2
  exit 1
}
CONFIG="config/baucord.yaml"
if [[ ! -f "$CONFIG" ]]; then
  if [[ -f config/baucord.example.yaml ]]; then
    cp config/baucord.example.yaml "$CONFIG"
    echo "created $CONFIG from config/baucord.example.yaml: set server.public_address in it, then re-run" >&2
  else
    echo "no $CONFIG; get the example and edit it:" >&2
    echo "  curl -fsSL https://baucord.com/deploy/baucord.yaml -o $CONFIG" >&2
  fi
  exit 1
fi
if grep -q '^ *public_address: *"203\.0\.113\.5' "$CONFIG"; then
  echo "warning: server.public_address in $CONFIG is still the example's; invites will print it" >&2
fi
# server.port: the first `port:` line inside the top-level `server:` block.
if [[ -z "$MEDIA_PORT" ]]; then
  MEDIA_PORT="$(awk '/^server:/ { s = 1; next } /^[^ #]/ { s = 0 } s && /^ +port:/ { print $2; exit }' "$CONFIG")"
  MEDIA_PORT="${MEDIA_PORT:-51000}"
fi
[[ -n "$IFACE" ]] || {
  echo "could not detect default interface; set IFACE=" >&2
  exit 1
}
log() { printf '\n→ %s\n' "$*"; }

apt_install() {
  if command -v apt-get >/dev/null; then
    DEBIAN_FRONTEND=noninteractive apt-get install -y -q "$@"
  else
    echo "no apt-get on this host; install manually: $*" >&2
    return 1
  fi
}

# ── 1. Docker + compose ───────────────────────────────────────────────────────
log "Checking Docker"
if ! command -v docker >/dev/null; then
  log "Docker not found — installing docker.io + docker-compose-v2 from distro repos"
  apt-get update -q
  apt_install docker.io docker-compose-v2
fi
if ! docker compose version >/dev/null 2>&1; then
  log "compose plugin not found — installing docker-compose-v2"
  apt-get update -q
  apt_install docker-compose-v2 || apt_install docker-compose-plugin
fi
systemctl enable --now docker >/dev/null
docker compose version

# ── 2. Data dir for the non-root container ────────────────────────────────────
log "Preparing $DATA_DIR (owner uid $BAUCORD_UID)"
mkdir -p "$DATA_DIR"
# 0.5 kept a netcode.key here; 1.0 generates identity.key on first start
# and clients pin it, so the directory must persist across recreates.
if [[ -f "$DATA_DIR/netcode.key" ]]; then
  echo "  netcode.key from the 0.5 line is no longer used; leaving it in place"
fi
chown -R "$BAUCORD_UID:$BAUCORD_UID" "$DATA_DIR"
chmod 700 "$DATA_DIR"

# ── 3. Host tuning ────────────────────────────────────────────────────────────
log "Host tools"
pkgs="ethtool irqbalance"
[[ "$MEDIA_VIA_DOCKER_NAT" == "false" ]] && pkgs="$pkgs nftables"
need=""
for p in $pkgs; do dpkg -s "$p" >/dev/null 2>&1 || need="$need $p"; done
[[ -n "$need" ]] && apt_install $need
systemctl enable --now irqbalance >/dev/null 2>&1 || true

log "Writing /etc/sysctl.d/99-baucord-udp.conf"
cat >/etc/sysctl.d/99-baucord-udp.conf <<'EOF'
# Baucord — UDP tuning for voice/video relay (net.core.* applies inside containers)
#
# Per-socket buffer caps. Send is much larger than receive on purpose: a keyframe
# (~177 KB) fanned out to N viewers from one socket queues N*177 KB on that
# socket's send buffer (200 viewers ≈ 35 MB).
net.core.wmem_max = 67108864
net.core.rmem_max = 8388608
# Defaults apply to every socket on the box. Baucord should still set
# SO_SNDBUF/SO_RCVBUF explicitly on its relay socket.
net.core.rmem_default = 4194304
net.core.wmem_default = 4194304

# NIC -> IP stack backlog (per CPU). Default 1000 drops under UDP bursts.
net.core.netdev_max_backlog = 65536
net.core.netdev_budget = 600
net.core.netdev_budget_usecs = 8000

net.ipv4.udp_rmem_min = 16384
net.ipv4.udp_wmem_min = 16384
net.core.optmem_max = 65536

net.core.default_qdisc = fq
net.ipv4.ipfrag_high_thresh = 16777216
fs.file-max = 2097152
EOF

# Docker port publishing = DNAT = every client is a conntrack flow. A full table
# drops packets with nothing in the app logs. Load the module at boot *before*
# systemd-sysctl so the keys exist, and size the hash table to match (buckets are
# only derived from max at module load time).
log "Writing conntrack module + sysctl config"
echo nf_conntrack >/etc/modules-load.d/nf_conntrack.conf
echo "options nf_conntrack hashsize=262144" >/etc/modprobe.d/nf_conntrack.conf
modprobe nf_conntrack 2>/dev/null || true
cat >/etc/sysctl.d/99-baucord-conntrack.conf <<'EOF'
# Baucord — conntrack sizing for Docker-published UDP media
net.netfilter.nf_conntrack_max = 1048576
net.netfilter.nf_conntrack_udp_timeout = 30
net.netfilter.nf_conntrack_udp_timeout_stream = 120
EOF
if [[ -w /sys/module/nf_conntrack/parameters/hashsize ]]; then
  echo 262144 >/sys/module/nf_conntrack/parameters/hashsize || true
fi

log "Writing boot-time NIC tuning (/usr/local/sbin/baucord-net-tune)"
cat >/usr/local/sbin/baucord-net-tune <<EOF
#!/usr/bin/env bash
# Applied at boot by baucord-net-tune.service. Generated by scripts/setup.sh — edit that instead.
set -u
IFACE="$IFACE"
MEDIA_PORT="$MEDIA_PORT"
MEDIA_VIA_DOCKER_NAT="$MEDIA_VIA_DOCKER_NAT"

# NIC ring buffers -> hardware maximum
if command -v ethtool >/dev/null; then
  rx=\$(ethtool -g "\$IFACE" 2>/dev/null | awk '/Pre-set/{p=1} p&&/^RX:/{print \$2; exit}')
  tx=\$(ethtool -g "\$IFACE" 2>/dev/null | awk '/Pre-set/{p=1} p&&/^TX:/{print \$2; exit}')
  [[ -n "\$rx" ]] && ethtool -G "\$IFACE" rx "\$rx" 2>/dev/null
  [[ -n "\$tx" ]] && ethtool -G "\$IFACE" tx "\$tx" 2>/dev/null
fi
ip link set "\$IFACE" txqueuelen 10000

# RPS: spread receive processing across CPUs if the NIC has fewer queues than cores
ncpu=\$(nproc); nq=\$(ls -d /sys/class/net/"\$IFACE"/queues/rx-* | wc -l)
if (( nq < ncpu && ncpu <= 63 )); then
  mask=\$(printf '%x' \$(( (1 << ncpu) - 1 )))
  for q in /sys/class/net/"\$IFACE"/queues/rx-*; do echo "\$mask" > "\$q/rps_cpus"; done
  echo 32768 > /proc/sys/net/core/rps_sock_flow_entries
fi

# Only when baucord is on the host network: the media port never needs stateful
# tracking. NEVER enable while the port is published via docker 'ports:' — DNAT
# needs conntrack and this would blackhole all media.
if [[ "\$MEDIA_VIA_DOCKER_NAT" == "false" ]] && command -v nft >/dev/null; then
  nft -f - <<NFT
table inet baucord_notrack
delete table inet baucord_notrack
table inet baucord_notrack {
  chain pre { type filter hook prerouting priority raw; policy accept; udp dport \$MEDIA_PORT notrack; }
  chain out { type filter hook output priority raw; policy accept; udp sport \$MEDIA_PORT notrack; }
}
NFT
fi
exit 0
EOF
chmod 0755 /usr/local/sbin/baucord-net-tune

cat >/etc/systemd/system/baucord-net-tune.service <<'EOF'
[Unit]
Description=Baucord NIC / netfilter tuning
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/baucord-net-tune
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF

# Cloud-init: don't let network hotplug rewrite the NIC config under us
mkdir -p /etc/cloud/cloud.cfg.d
cat >/etc/cloud/cloud.cfg.d/99-disable-hotplug.cfg <<'EOF'
updates:
  network:
    when: []
EOF

log "Applying host tuning"
sysctl --system >/dev/null
systemctl daemon-reload
systemctl enable --now baucord-net-tune.service >/dev/null
systemctl restart baucord-net-tune.service
sysctl net.core.rmem_max net.core.wmem_max net.core.netdev_max_backlog \
  net.netfilter.nf_conntrack_max 2>/dev/null | sed 's/^/  /'

# ── 4. Stack ──────────────────────────────────────────────────────────────────
# Pull first, so a re-run also updates the server to the latest release
# (a compose file that builds from source instead skips the pull).
log "Pulling the image and starting / updating the compose stack"
docker compose pull
docker compose up -d --remove-orphans

# ── 5. Address, identity and the first admin invite ──────────────────────────
# At startup the server logs "Address for clients: <address> — identity
# SHA256:…": what people type, and the key their app pins. While nobody
# holds the admin role it also logs a one-time admin invite code (1 use,
# 1 hour): use it from your own app, then make invites in the app.
log "Address and identity"
address=""
for _ in $(seq 1 30); do
  address="$(docker compose logs --no-color baucord 2>/dev/null | grep -m1 'Address for clients' || true)"
  [[ -n "$address" ]] && break
  sleep 1
done
if [[ -n "$address" ]]; then
  echo "  ${address#*Address for clients: }"
  bootstrap="$(docker compose logs --no-color baucord 2>/dev/null | grep 'Bootstrap invite' | tail -1 || true)"
  if [[ -n "$bootstrap" ]]; then
    echo "  first admin: ${bootstrap#*Bootstrap invite }"
  fi
else
  echo "  not in the log yet; run: docker compose logs baucord | grep -E 'Address for clients|Bootstrap invite'"
fi

# ── 6. Network monitor service ────────────────────────────────────────────────
# The monitor answered "is the server the problem?" decisively in two
# incidents — and missed a third because a manually-nohup'd copy had been
# stopped and never restarted. As a service it is always on, survives
# reboots, and restarts itself. The script itself is unchanged; it needs
# root (nsenter into the container netns, ethtool).
log "Installing baucord-monitor service"
install -m 0755 scripts/monitor-net.sh /usr/local/bin/baucord-monitor

cat >/etc/systemd/system/baucord-monitor.service <<'UNIT'
[Unit]
Description=baucord network monitor (per-netns UDP/NIC drop counters)
Documentation=file:///usr/local/bin/baucord-monitor
# Not Requires=docker: the script handles an absent/stopped container
# gracefully (empty rows), and host-level counters matter even then.
After=network-online.target docker.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/baucord-monitor 10 /var/log/baucord-net.log
Restart=always
RestartSec=5
# Needs real root (nsenter/ethtool); harden only what can't break it.
ProtectHome=true
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
UNIT

cat >/etc/logrotate.d/baucord-net <<'ROT'
# ~1.7 MB/day at 10s intervals; keep ~2 months.
# copytruncate: the monitor appends by path, and its CSV header is only
# written at startup — truncating in place keeps one continuous process.
# (Rotated files start headerless; the column order is in the script.)
/var/log/baucord-net.log {
  weekly
  rotate 8
  compress
  delaycompress
  missingok
  notifempty
  copytruncate
}
ROT

systemctl daemon-reload
systemctl enable baucord-monitor >/dev/null
# restart (not start): re-running setup picks up script changes; the
# "# start/resume" header line the script writes doubles as a marker of
# when setup was re-applied.
systemctl restart baucord-monitor

# ── Status ────────────────────────────────────────────────────────────────────
log "Status"
docker compose ps
sleep 2
if ss -uln | grep -q ":$MEDIA_PORT "; then
  echo "  udp/$MEDIA_PORT is listening on the host (published from the baucord container)"
else
  echo "  WARNING: nothing listening on udp/$MEDIA_PORT — check: docker compose logs baucord"
fi
echo "  make sure the firewall allows udp/$MEDIA_PORT (nothing else is needed: no TCP, no TLS certificate)"
echo "  network monitor: systemctl status baucord-monitor   (log: tail -f /var/log/baucord-net.log)"
