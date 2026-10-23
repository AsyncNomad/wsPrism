#!/usr/bin/env bash
# Run a server and a client in two network namespaces joined by a veth pair,
# with netem impairment applied per direction. No root needed: everything
# runs inside an unprivileged user namespace (unshare -Urn).
#
#   bench/netlab.sh --srv-egress PROFILE --cli-egress PROFILE \
#                   [--outage MEAN_PERIOD_MS:LEN_MS] --server "CMD" --client "CMD"
#
# Server: 10.77.0.1 (veth-srv), client: 10.77.0.2 (veth-cli).
# NETLAB_CLIENT_ADDRS=N gives the client N more addresses (10.77.0.3 ...)
# and the full port range, for tests with more than ~64k connections
# (wsprism-loadgen --local-addrs).
# The client command can use $SERVER_PID (same PID namespace).
#
# PROFILE (egress of that side's veth):
#   none
#   delay:<ms>
#   uniform:<loss%>:<delay_ms>
#   burst:<avg_loss%>:<avg_burst_len>:<delay_ms>   Gilbert-Elliott (simple Gilbert:
#        loss only in the bad state; r = 1/len, p = loss*r/(1-loss))
#
# --outage: Wi-Fi-like link blackout. Both directions drop everything for
# LEN_MS at once, at gaps drawn uniformly from [0.5, 1.5] x MEAN_PERIOD_MS
# (bash $RANDOM seeded with NETLAB_SEED, default 1, so the same seed gives
# the same schedule relative to the client start). Unlike `burst`, which is
# per packet and therefore spread over every connection sharing the veth,
# every connection sees the same time-correlated outage. The first outage
# can come no earlier than NETLAB_OUTAGE_START_S (default 0) after the client
# starts, so connection setup can be kept out of it.
#
# Offloads (TSO/GSO/GRO) are disabled on both veths so netem drops individual
# packets, not whole GSO super-packets. See docs/design/hot-lane-over-webtransport.md §7.2.
# NETLAB_OFFLOAD=on keeps them on — closer to a real server's cost, valid
# only without loss (a warning is printed otherwise).
set -euo pipefail

if [[ "${1:-}" != "__inner" ]]; then
  exec unshare -Urn --fork bash "$0" __inner "$@"
fi
shift

SRV_EGRESS=none CLI_EGRESS=none OUTAGE="" SERVER="" CLIENT="" SERVER_LOG="${NETLAB_SERVER_LOG:-/dev/null}"
OFFLOAD="${NETLAB_OFFLOAD:-off}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --srv-egress) SRV_EGRESS="$2"; shift 2 ;;
    --cli-egress) CLI_EGRESS="$2"; shift 2 ;;
    --outage) OUTAGE="$2"; shift 2 ;;
    --server) SERVER="$2"; shift 2 ;;
    --client) CLIENT="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[[ -n "$SERVER" && -n "$CLIENT" ]] || { echo "need --server and --client" >&2; exit 2; }

netem_args() { # PROFILE -> tc netem arguments
  IFS=: read -r kind a b c <<<"$1"
  case "$kind" in
    none) echo "" ;;
    delay) echo "delay ${a}ms" ;;
    uniform) echo "delay ${b:-0}ms loss ${a}%" ;;
    burst)
      python3 - "$a" "$b" "${c:-0}" <<'PY'
import sys
loss, length, delay = float(sys.argv[1]) / 100, float(sys.argv[2]), sys.argv[3]
r = 1 / length
p = loss * r / (1 - loss)
print(f"delay {delay}ms loss gemodel {p*100:.4f}% {r*100:.4f}%")
PY
      ;;
    *) echo "bad profile: $1" >&2; exit 2 ;;
  esac
}

ip link set lo up
unshare -n sleep infinity &
CHILD=$!
cleanup() { kill "${OUTAGE_PID:-}" "${SERVER_PID:-}" "$CHILD" 2>/dev/null || true; }
trap cleanup EXIT
sleep 0.1

ip link add veth-srv type veth peer name veth-cli
ip link set veth-cli netns "$CHILD"
ip addr add 10.77.0.1/24 dev veth-srv
ip link set veth-srv up
ethtool -K veth-srv tso "$OFFLOAD" gso "$OFFLOAD" gro "$OFFLOAD" >/dev/null 2>&1 || true
nsenter -t "$CHILD" -n bash -c "
  ip link set lo up
  ip addr add 10.77.0.2/24 dev veth-cli
  for i in \$(seq 1 ${NETLAB_CLIENT_ADDRS:-0}); do ip addr add 10.77.0.\$((2 + i))/24 dev veth-cli; done
  [[ ${NETLAB_CLIENT_ADDRS:-0} -gt 0 ]] && sysctl -qw net.ipv4.ip_local_port_range='1024 65535'
  ip link set veth-cli up
  ethtool -K veth-cli tso $OFFLOAD gso $OFFLOAD gro $OFFLOAD >/dev/null 2>&1 || true"

SRV_NETEM="$(netem_args "$SRV_EGRESS")"
CLI_NETEM="$(netem_args "$CLI_EGRESS")"
# An outage toggles netem on both sides, so both need a qdisc.
if [[ -n "$OUTAGE" ]]; then
  SRV_NETEM="${SRV_NETEM:-delay 0ms}"
  CLI_NETEM="${CLI_NETEM:-delay 0ms}"
fi
[[ -n "$SRV_NETEM" ]] && tc qdisc add dev veth-srv root netem $SRV_NETEM limit 100000
[[ -n "$CLI_NETEM" ]] && nsenter -t "$CHILD" -n tc qdisc add dev veth-cli root netem $CLI_NETEM limit 100000
echo "netlab: srv-egress=[${SRV_NETEM:-none}] cli-egress=[${CLI_NETEM:-none}] outage=[${OUTAGE:-none}] offload=$OFFLOAD" >&2
if [[ "$OFFLOAD" == on && ( "$SRV_NETEM$CLI_NETEM" == *loss* || -n "$OUTAGE" ) ]]; then
  echo "netlab: WARNING offload=on with loss: netem drops whole GSO super-packets" >&2
fi

# Both directions down for LEN ms (delay kept, loss 100%), then restored.
outage_loop() {
  local mean="${OUTAGE%%:*}" len="${OUTAGE##*:}" gap
  local srv_down="${SRV_NETEM%%loss*} loss 100%" cli_down="${CLI_NETEM%%loss*} loss 100%"
  RANDOM="${NETLAB_SEED:-1}"
  sleep "${NETLAB_OUTAGE_START_S:-0}"
  while true; do
    gap=$(( mean / 2 + RANDOM % (mean + 1) ))
    sleep "$(printf '%d.%03d' $((gap / 1000)) $((gap % 1000)))"
    tc qdisc change dev veth-srv root netem $srv_down limit 100000
    nsenter -t "$CHILD" -n tc qdisc change dev veth-cli root netem $cli_down limit 100000
    sleep "$(printf '%d.%03d' $((len / 1000)) $((len % 1000)))"
    tc qdisc change dev veth-srv root netem $SRV_NETEM limit 100000
    nsenter -t "$CHILD" -n tc qdisc change dev veth-cli root netem $CLI_NETEM limit 100000
  done
}

bash -c "$SERVER" >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
export SERVER_PID
sleep 1
if [[ -n "$OUTAGE" ]]; then
  outage_loop &
  OUTAGE_PID=$!
fi
nsenter -t "$CHILD" -n bash -c "$CLIENT"
