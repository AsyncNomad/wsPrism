#!/usr/bin/env bash
# Run a server and a client in two network namespaces joined by a veth pair,
# with netem impairment applied per direction. No root needed: everything
# runs inside an unprivileged user namespace (unshare -Urn).
#
#   bench/netlab.sh --srv-egress PROFILE --cli-egress PROFILE \
#                   --server "CMD" --client "CMD"
#
# Server: 10.77.0.1 (veth-srv), client: 10.77.0.2 (veth-cli).
# The client command can use $SERVER_PID (same PID namespace).
#
# PROFILE (egress of that side's veth):
#   none
#   delay:<ms>
#   uniform:<loss%>:<delay_ms>
#   burst:<avg_loss%>:<avg_burst_len>:<delay_ms>   Gilbert-Elliott (simple Gilbert:
#        loss only in the bad state; r = 1/len, p = loss*r/(1-loss))
#
# Offloads (TSO/GSO/GRO) are disabled on both veths so netem drops individual
# packets, not whole GSO super-packets. See docs/design/hot-lane-over-webtransport.md §7.2.
set -euo pipefail

if [[ "${1:-}" != "__inner" ]]; then
  exec unshare -Urn --fork bash "$0" __inner "$@"
fi
shift

SRV_EGRESS=none CLI_EGRESS=none SERVER="" CLIENT="" SERVER_LOG="${NETLAB_SERVER_LOG:-/dev/null}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --srv-egress) SRV_EGRESS="$2"; shift 2 ;;
    --cli-egress) CLI_EGRESS="$2"; shift 2 ;;
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
cleanup() { kill "${SERVER_PID:-}" "$CHILD" 2>/dev/null || true; }
trap cleanup EXIT
sleep 0.1

ip link add veth-srv type veth peer name veth-cli
ip link set veth-cli netns "$CHILD"
ip addr add 10.77.0.1/24 dev veth-srv
ip link set veth-srv up
ethtool -K veth-srv tso off gso off gro off >/dev/null 2>&1 || true
nsenter -t "$CHILD" -n bash -c '
  ip link set lo up
  ip addr add 10.77.0.2/24 dev veth-cli
  ip link set veth-cli up
  ethtool -K veth-cli tso off gso off gro off >/dev/null 2>&1 || true'

SRV_NETEM="$(netem_args "$SRV_EGRESS")"
CLI_NETEM="$(netem_args "$CLI_EGRESS")"
[[ -n "$SRV_NETEM" ]] && tc qdisc add dev veth-srv root netem $SRV_NETEM limit 100000
[[ -n "$CLI_NETEM" ]] && nsenter -t "$CHILD" -n tc qdisc add dev veth-cli root netem $CLI_NETEM limit 100000
echo "netlab: srv-egress=[${SRV_NETEM:-none}] cli-egress=[${CLI_NETEM:-none}]" >&2

bash -c "$SERVER" >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
export SERVER_PID
sleep 1
nsenter -t "$CHILD" -n bash -c "$CLIENT"
