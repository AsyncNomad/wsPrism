#!/usr/bin/env bash
# Idle memory per connection gate (CI): 10,000 idle connections, one user
# each, on loopback; fails if the gateway's RSS per connection exceeds
# MEM_GATE_MAX_KB. Memory per connection is a headline number, and it once
# grew 4 KB (+25%) unnoticed: a per-session tokio mpsc channel allocates a
# 32-slot block up front (docs/BENCHMARKS.md "유휴 메모리 회귀").
#
#   cargo build --release -p wsprism-gateway -p wsprism-loadgen
#   bench/mem-gate.sh                 # MEM_GATE_MAX_KB=18.5 by default
#
# Loopback rather than bench/netlab.sh so it runs on a CI runner; RSS per
# connection does not depend on the network path. The value is the median
# of MEM_GATE_RUNS runs (default 3).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
GW=${MEM_GATE_GW:-$ROOT/target/release/wsprism-gateway}
LG=${MEM_GATE_LG:-$ROOT/target/release/wsprism-loadgen}
MAX=${MEM_GATE_MAX_KB:-18.5}
RUNS=${MEM_GATE_RUNS:-3}
PORT=${MEM_GATE_PORT:-28080}
ulimit -n 65536 2>/dev/null || true

W=$(mktemp -d)
trap 'kill ${GW_PID:-} 2>/dev/null || true; rm -rf "$W"' EXIT
sed "s|listen: \"127.0.0.1:18080\"|listen: \"127.0.0.1:$PORT\"|" "$ROOT/bench/gateway.yaml" > "$W/gw.yaml"

values=()
for run in $(seq 1 "$RUNS"); do
  RUST_LOG=error "$GW" --config "$W/gw.yaml" & GW_PID=$!
  for _ in $(seq 1 100); do
    curl -sf "http://127.0.0.1:$PORT/readyz" >/dev/null && break
    sleep 0.1
  done
  kb=$("$LG" gateway --target wsprism --lane ext --url "ws://127.0.0.1:$PORT" \
      --conns 10000 --rooms 100 --publishers 0 --duration 5 \
      --connect-concurrency 500 --distinct-users --seed "$run" --json \
      --server-pid "$GW_PID" --label "mem-gate run=$run" |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["server_rss_kb_per_conn"])')
  kill "$GW_PID"; wait "$GW_PID" 2>/dev/null || true
  echo "run $run: $kb KB/connection"
  values+=("$kb")
done
median=$(printf '%s\n' "${values[@]}" | sort -n | sed -n "$(( (RUNS + 1) / 2 ))p")
echo "median: $median KB/connection (limit $MAX)"
python3 -c "import sys; sys.exit(0 if float('$median') <= float('$MAX') else 1)" || {
  echo "idle memory per connection regressed: $median KB > $MAX KB" >&2
  exit 1
}
