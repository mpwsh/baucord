#!/usr/bin/env bash
# scripts/monitor-net.sh — log UDP / NIC drop counters while the relay runs.
#
# UDP socket counters are per network namespace, so the host's /proc/net/snmp says
# nothing about the baucord container. This samples the host AND each container in
# $CONTAINERS (via nsenter) and writes one CSV row per namespace per interval.
# Drop columns are deltas for the interval: non-zero means drops happened just now.
#
# Usage:      sudo scripts/monitor-net.sh [interval_s] [logfile]
#             defaults: 10s, /var/log/baucord-net.log
#             CONTAINERS="baucord" sudo scripts/monitor-net.sh 10   # sample more containers
# Background: sudo nohup scripts/monitor-net.sh 10 >/dev/null 2>&1 &
# Watch:      tail -f /var/log/baucord-net.log
set -u
INTERVAL="${1:-10}"
LOG="${2:-/var/log/baucord-net.log}"
CONTAINERS="${CONTAINERS-baucord}" # set CONTAINERS="" for host only
IFACE="${IFACE:-$(ip -o route show default | awk '{print $5; exit}')}"
[[ $EUID -eq 0 ]] || {
  echo "run as root (nsenter / ethtool need it)" >&2
  exit 1
}
[[ -n "$CONTAINERS" ]] && ! command -v docker >/dev/null && {
  echo "docker not found" >&2
  exit 1
}

# ── readers (first arg: "host" or a container PID) ───────────────────────────
in_ns() {
  local p=$1
  shift
  if [[ $p == host ]]; then "$@"; else nsenter -t "$p" -n "$@"; fi
}
ns_pid() { docker inspect -f '{{.State.Pid}}' "$1" 2>/dev/null | grep -vx 0; }
snmp4() { in_ns "$1" awk -v f="$2" '/^Udp:/{ if(!h){for(i=1;i<=NF;i++)idx[$i]=i;h=1} else print $(idx[f]) }' /proc/net/snmp 2>/dev/null; }
snmp6() { in_ns "$1" awk -v f="Udp6$2" '$1==f{v=$2} END{print v+0}' /proc/net/snmp6 2>/dev/null; }
udp() {
  local a b
  a=$(snmp4 "$1" "$2")
  b=$(snmp6 "$1" "$2")
  echo $((${a:-0} + ${b:-0}))
}
sock_q() { in_ns "$1" ss -uanH 2>/dev/null | awk '{ if($2>r) r=$2; if($3>t) t=$3 } END{print (r+0)","(t+0)}'; }
udp_mem() {
  local m
  m=$(in_ns "$1" awk '/^UDP:/{print $NF}' /proc/net/sockstat 2>/dev/null)
  echo "${m:-0}"
}
# host-only
softnet_drops() {
  local s=0 d
  while read -r _ d _; do s=$((s + 16#$d)); done </proc/net/softnet_stat
  echo "$s"
}
nic_drops() { ethtool -S "$IFACE" 2>/dev/null | awk '/drop|miss|exceed|discard|no_buf/ {s+=$2} END{print s+0}'; }
veth_drops() { cat /sys/class/net/{veth*,br-*,docker0}/statistics/{rx,tx}_dropped 2>/dev/null | awk '{s+=$1} END{print s+0}'; }
ct() {
  local c=/proc/sys/net/netfilter/nf_conntrack_count m=/proc/sys/net/netfilter/nf_conntrack_max
  if [[ -r $c ]]; then echo "$(cat $c),$(cat $m)"; else echo "na,na"; fi
}
cpu_snap() {
  read -r _ u n s i w q sq _ </proc/stat
  CPU_TOT=$((u + n + s + i + w + q + sq))
  CPU_SIRQ=$sq
}
clamp() { if (($1 < 0)); then echo 0; else echo "$1"; fi; }

# ── log header + context ─────────────────────────────────────────────────────
if [[ ! -s "$LOG" ]]; then
  {
    echo "# baucord-monitor start $(date -Is) iface=$IFACE interval=${INTERVAL}s containers='$CONTAINERS'"
    echo "# rmem_max wmem_max backlog ct_max = $(sysctl -n net.core.rmem_max net.core.wmem_max net.core.netdev_max_backlog net.netfilter.nf_conntrack_max 2>/dev/null | paste -sd' ')"
    command -v ethtool >/dev/null || echo "# ethtool not found: nic_drop column will be 0"
    echo "ts,ns,udp_in_pps,udp_out_pps,in_err,rcvbuf_err,sndbuf_err,max_rxq_bytes,max_txq_bytes,udp_mem_pages,softnet_drop,nic_drop,veth_drop,ct_count,ct_max,load1,softirq_pct,note"
  } >>"$LOG"
else
  echo "# resume $(date -Is) iface=$IFACE interval=${INTERVAL}s containers='$CONTAINERS'" >>"$LOG"
fi
trap 'echo "# stop $(date -Is)" >> "$LOG"; exit 0' INT TERM
echo "logging to $LOG every ${INTERVAL}s (Ctrl-C to stop)"

# ── main loop ────────────────────────────────────────────────────────────────
declare -A P
SN=0 NIC=0 VE=0 CPU_TOT=0 CPU_SIRQ=0
while :; do
  for c in host $CONTAINERS; do
    if [[ $c == host ]]; then h=host; else h=$(ns_pid "$c"); fi
    if [[ -z "$h" ]]; then
      echo "$(date -Is),$c,,,,,,,,,,,,,,,,not running" >>"$LOG"
      continue
    fi

    IN=$(udp "$h" InDatagrams) OUT=$(udp "$h" OutDatagrams) IERR=$(udp "$h" InErrors)
    RERR=$(udp "$h" RcvbufErrors) SERR=$(udp "$h" SndbufErrors)
    Q=$(sock_q "$h") MEM=$(udp_mem "$h")
    hostcols=",,,,,,"
    if [[ $c == host ]]; then
      SN=$(softnet_drops) NIC=$(nic_drops) VE=$(veth_drops)
      cpu_snap
    fi

    # Keys are one word (`host_in`): bash keeps a space in a key, so the
    # `P[$c, in]` written and the `P[$c,in]` checked were two keys, the
    # check never matched, and no data row was ever written (review
    # P1-21). A one-word key leaves a formatter nothing to split.
    if [[ -v P[${c}_in] ]]; then
      note=""
      ((IN < P[${c}_in])) && note+="COUNTER_RESET(container restarted?) "
      d_in=$(clamp $(((IN - P[${c}_in]) / INTERVAL))) d_out=$(clamp $(((OUT - P[${c}_out]) / INTERVAL)))
      d_ierr=$(clamp $((IERR - P[${c}_ierr]))) d_rerr=$(clamp $((RERR - P[${c}_rerr]))) d_serr=$(clamp $((SERR - P[${c}_serr])))
      ((d_rerr > 0)) && note+="RCVBUF(app not draining or rmem too small) "
      ((d_serr > 0)) && note+="SNDBUF(wmem too small or link saturated) "
      ((d_ierr > d_rerr)) && note+="INERR(csum/other) "
      if [[ $c == host ]]; then
        d_sn=$(clamp $((SN - P[host_sn]))) d_nic=$(clamp $((NIC - P[host_nic]))) d_ve=$(clamp $((VE - P[host_ve])))
        dt=$((CPU_TOT - P[host_tot]))
        sirq=0
        ((dt > 0)) && sirq=$(((CPU_SIRQ - P[host_sirq]) * 100 / dt))
        ((d_sn > 0)) && note+="SOFTNET(netdev_max_backlog/CPU) "
        ((d_nic > 0)) && note+="NIC(ring buffer or cloud pps/bw limit - ethtool -S) "
        ((d_ve > 0)) && note+="VETH(docker bridge/veth drops) "
        hostcols="$d_sn,$d_nic,$d_ve,$(ct),$(cut -d' ' -f1 /proc/loadavg),$sirq"
      fi
      printf '%s,%s,%d,%d,%d,%d,%d,%s,%s,%s,%s\n' \
        "$(date -Is)" "$c" "$d_in" "$d_out" "$d_ierr" "$d_rerr" "$d_serr" "$Q" "$MEM" "$hostcols" "${note% }" >>"$LOG"
    fi

    P[${c}_in]=$IN P[${c}_out]=$OUT P[${c}_ierr]=$IERR P[${c}_rerr]=$RERR P[${c}_serr]=$SERR
    if [[ $c == host ]]; then
      P[host_sn]=$SN P[host_nic]=$NIC P[host_ve]=$VE P[host_tot]=$CPU_TOT P[host_sirq]=$CPU_SIRQ
    fi
  done
  sleep "$INTERVAL"
done
