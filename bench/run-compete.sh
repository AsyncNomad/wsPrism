#!/usr/bin/env bash
# WebSocket fan-out comparison: wsPrism vs Centrifugo vs Sockudo (Pusher
# protocol), same scenario, same machine, inside bench/netlab.sh (traffic
# never leaves the machine). ADR 0004 rev 3.
#
#   bench/run-compete.sh <name> [RUNS] [SCENARIOS] [TARGETS]
#
# SCENARIOS: space-separated NAME[:RATE] from scenario() below, e.g. "rooms:20 rooms:60".
# TARGETS:   wsprism-ext wsprism-hot centrifugo sockudo
# Each run appends one JSON line to bench/results/<name>.jsonl. Resumable:
# a (target, label) already in the file is skipped.
#
# Servers are pinned to SERVER_CPUS (default 0-7), the load generator to
# LOADGEN_CPUS (default 8-23), so they do not compete for cores and every
# server gets the same budget. Binaries: bench/competitors/fetch.sh.
# WSPRISM_LG_EXTRA: extra loadgen flags for the wsPrism targets, e.g.
# "--distinct-users" (one user per connection, like the other servers'
# anonymous connections).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NAME="${1:?name}"
RUNS="${2:-3}"
SCENARIOS="${3:-rooms:20 broadcast:10}"
TARGETS="${4:-wsprism-ext wsprism-hot centrifugo sockudo}"
SERVER_CPUS="${SERVER_CPUS:-0-7}"
LOADGEN_CPUS="${LOADGEN_CPUS:-8-23}"

OUT="$ROOT/bench/results/$NAME.jsonl"
mkdir -p "$ROOT/bench/results"
TOOLS="${WSPRISM_BENCH_TOOLS:-$HOME/.cache/wsprism-bench}"
"$ROOT/bench/competitors/fetch.sh" >/dev/null
CENTRIFUGO="$TOOLS/centrifugo-6.9.7/centrifugo"
SOCKUDO="$TOOLS/sockudo-5.1.0/sockudo"
cargo build --release -q --manifest-path "$ROOT/Cargo.toml" -p wsprism-gateway -p wsprism-loadgen
GW="$ROOT/target/release/wsprism-gateway"
LG="$ROOT/target/release/wsprism-loadgen"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
# The benchmark gateway config, listening inside netlab.
sed 's|listen: "127.0.0.1:18080"|listen: "10.77.0.1:18080"|' "$ROOT/bench/gateway.yaml" > "$WORK/wsprism.yaml"
grep -q '10.77.0.1:18080' "$WORK/wsprism.yaml"

# scenario NAME[:RATE] -> loadgen arguments. RATE (messages/s per publisher)
# sweeps the load toward saturation.
scenario() {
  local kind="${1%%:*}" rate="${1#*:}"
  [[ "$rate" == "$1" ]] && rate=""
  case "$kind" in
    # 100 game rooms of 10, everyone publishes (state sync on WebSocket).
    # Deliveries to others/s = 9000 x RATE (default 20 Hz -> 180k/s).
    rooms) echo "--conns 1000 --rooms 100 --publishers 1000 --rate ${rate:-20} --payload 64 --duration 20" ;;
    # 10 rooms of 1000 listeners, 1 publisher per room (live feed).
    # Deliveries/s = 9990 x RATE (default 10 Hz -> 100k/s).
    broadcast) echo "--conns 10000 --rooms 10 --publishers 10 --rate ${rate:-10} --payload 64 --duration 20 --connect-concurrency 500" ;;
    *) echo "unknown scenario: $1" >&2; exit 2 ;;
  esac
}

# target NAME -> "SERVER_CMD|LOADGEN_TARGET_ARGS". Log levels: warn/error for all
# (per-connection info logs would be measured as server cost).
target() {
  case "$1" in
    wsprism-ext) echo "env RUST_LOG=warn $GW --config $WORK/wsprism.yaml|--target wsprism --lane ext --url ws://10.77.0.1:18080 ${WSPRISM_LG_EXTRA:-}" ;;
    wsprism-hot) echo "env RUST_LOG=warn $GW --config $WORK/wsprism.yaml|--target wsprism --lane hot --url ws://10.77.0.1:18080 ${WSPRISM_LG_EXTRA:-}" ;;
    centrifugo) echo "$CENTRIFUGO --config $ROOT/bench/competitors/centrifugo.json|--target centrifugo --url ws://10.77.0.1:9101" ;;
    sockudo) echo "env RUST_LOG=warn $SOCKUDO --config $ROOT/bench/competitors/sockudo.json|--target pusher --url ws://10.77.0.1:6001" ;;
    *) echo "unknown target: $1" >&2; exit 2 ;;
  esac
}

total=0
for run in $(seq "$RUNS"); do for sc in $SCENARIOS; do for t in $TARGETS; do
  total=$((total + 1))
  label="scenario=$sc target=$t run=$run"
  if [[ -f "$OUT" ]] && grep -qF "\"label\":\"$label\"" "$OUT"; then
    echo "[$total] skip (done): $label" >&2
    continue
  fi
  echo "[$total] $label" >&2
  IFS='|' read -r server lgargs <<<"$(target "$t")"
  "$ROOT/bench/netlab.sh" --srv-egress none --cli-egress none \
    --server "taskset -c $SERVER_CPUS $server" \
    --client "taskset -c $LOADGEN_CPUS $LG gateway $lgargs $(scenario "$sc") --seed $run --json --server-pid \$SERVER_PID --label '$label'" \
    2>>"${COMPETE_LOG:-/dev/null}" >> "$OUT" || echo "[$total] FAILED: $label" >&2
done; done; done
echo "done: $total runs -> $OUT" >&2
