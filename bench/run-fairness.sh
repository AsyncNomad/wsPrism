#!/usr/bin/env bash
# Tenant fairness (0.1.x): does one tenant's flood slow another tenant down?
# Tenant B runs a light load alone, then the same load while tenant A floods
# the same gateway; compare B's latency and delivery. Inside bench/netlab.sh
# (traffic never leaves the machine), like run-cluster.sh.
#
#   bench/run-fairness.sh <name> [RUNS] [TARGETS] [A_RATES]
#
# TARGETS: memory (one node) and/or redis (one node on Redis: the shared
# outbox, ADR 0011 B4). A_RATES: tenant A's publishes per connection and
# second (1000 connections: 100 = 100k publishes/s, 0.9M deliveries/s;
# 200 = near one node's Redis ceiling). Cores: gateway 0-7, A's load
# generator 8-17, B's 18-22, Redis 23. One JSON line per B measurement in
# bench/results/<name>.jsonl (label: target, A's rate or "alone", run).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NAME="${1:?name}"
RUNS="${2:-3}"
TARGETS="${3:-memory redis}"
A_RATES="${4:-100 200}"
REDIS_DIR="${REDIS_DIR:-$HOME/.cache/wsprism-bench/redis-7.2}"
RS="$REDIS_DIR/redis-server"; RC="$REDIS_DIR/redis-cli"
[[ -x $RS && -x $RC ]] || { echo "missing $RS / $RC (docker cp from redis:7.2)" >&2; exit 1; }

OUT="$ROOT/bench/results/$NAME.jsonl"
cargo build --release -q --manifest-path "$ROOT/Cargo.toml" -p wsprism-gateway -p wsprism-loadgen
# FAIRNESS_GW: another gateway binary (A/B against an older build).
GW="${FAIRNESS_GW:-$ROOT/target/release/wsprism-gateway}"
LG="$ROOT/target/release/wsprism-loadgen"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
# Two identical tenants: bench (A) and bench2 (B).
python3 - "$ROOT/bench/gateway.yaml" "$WORK/memory.yaml" <<'PY'
import sys
s = open(sys.argv[1]).read().replace('listen: "127.0.0.1:18080"', 'listen: "10.77.0.1:18080"')
head, tenants = s.split("tenants:\n", 1)
open(sys.argv[2], "w").write(head + "tenants:\n" + tenants + tenants.replace("- id: bench", "- id: bench2", 1))
PY
sed 's|^gateway:|gateway:\n  broker: { kind: redis, url: "redis://10.77.0.1:6390", prefix: bench, pubsub_connections: 4 }|' \
  "$WORK/memory.yaml" > "$WORK/redis.yaml"
grep -q 'id: bench2' "$WORK/memory.yaml" && grep -q 'kind: redis' "$WORK/redis.yaml"

B_LOAD="--conns 200 --rooms 20 --publishers 200 --rate 10 --payload 64 --duration 20"
a_load() { echo "--conns 1000 --rooms 100 --publishers 1000 --rate $1 --payload 64 --duration 30"; }

total=0
for run in $(seq "$RUNS"); do for t in $TARGETS; do for a in alone $A_RATES; do
  total=$((total + 1))
  label="target=$t a_rate=$a run=$run"
  if [[ -f "$OUT" ]] && grep -qF "\"label\":\"$label\"" "$OUT"; then
    echo "[$total] skip (done): $label" >&2; continue
  fi
  echo "[$total] $label" >&2
  gw="taskset -c 0-7 env RUST_LOG=warn $GW --config $WORK/$t.yaml"
  if [[ $t == redis ]]; then
    cat > "$WORK/server.sh" <<SRV
taskset -c 23 $RS --bind 10.77.0.1 --port 6390 --save "" --appendonly no --protected-mode no --loglevel warning >/dev/null &
until $RC -h 10.77.0.1 -p 6390 ping >/dev/null 2>&1; do sleep 0.05; done
exec $gw
SRV
  else
    echo "exec $gw" > "$WORK/server.sh"
  fi
  b="taskset -c 18-22 $LG gateway --target wsprism --lane ext --url ws://10.77.0.1:18080 --tenant bench2 --distinct-users $B_LOAD --seed $run --json --label '$label'"
  if [[ $a == alone ]]; then
    echo "$b" > "$WORK/client.sh"
  else
    # A first (ramp-up), B measured while A runs.
    cat > "$WORK/client.sh" <<CLI
taskset -c 8-17 $LG gateway --target wsprism --lane ext --url ws://10.77.0.1:18080 --tenant bench --distinct-users $(a_load "$a") --seed $run --json >/dev/null 2>&1 &
sleep 6
$b
wait
CLI
  fi
  echo "$RC -h 10.77.0.1 -p 6390 shutdown nosave >/dev/null 2>&1 || true" >> "$WORK/client.sh"
  "$ROOT/bench/netlab.sh" --srv-egress none --cli-egress none --server "sh $WORK/server.sh" --client "sh $WORK/client.sh" \
    2>>"${FAIRNESS_LOG:-/dev/null}" >> "$OUT" || echo "[$total] FAILED: $label" >&2
done; done; done
echo "done: $total runs -> $OUT" >&2
