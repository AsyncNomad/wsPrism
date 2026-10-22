#!/usr/bin/env bash
# Session churn (ADR 0013 prerequisite): what connecting, joining and
# leaving cost the control Redis, and where it saturates, before rooms are
# sharded. One gateway node on cores 0-7, the load generator on 8-22, Redis
# on 23, inside bench/netlab.sh (traffic never leaves the machine).
#
#   bench/run-churn.sh <name> [RUNS] [TARGETS] [CASES]
#
# TARGETS: memory (no Redis; the gateway's own cost) and/or redis.
# CASES: "<workers>x<joins>" (concurrent session loops x rooms joined per
# session), e.g. "100x1 400x1 100x5 400x5". Each session connects, waits
# for sys.authed, joins, closes. Output: the loadgen's JSON line and, for
# redis, Redis INFO deltas ({"label": ..., "redis": {...}}) in
# bench/results/<name>.jsonl.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NAME="${1:?name}"
RUNS="${2:-3}"
TARGETS="${3:-memory redis}"
CASES="${4:-100x1 400x1 100x5 400x5}"
DURATION="${DURATION:-20}"
ROOMS="${ROOMS:-1000}"   # room pool; few rooms = already subscribed, no SUBSCRIBE per join
REDIS_DIR="${REDIS_DIR:-$HOME/.cache/wsprism-bench/redis-7.2}"
RS="$REDIS_DIR/redis-server"; RC="$REDIS_DIR/redis-cli"
[[ -x $RS && -x $RC ]] || { echo "missing $RS / $RC (docker cp from redis:7.2)" >&2; exit 1; }

OUT="$ROOT/bench/results/$NAME.jsonl"
cargo build --release -q --manifest-path "$ROOT/Cargo.toml" -p wsprism-gateway -p wsprism-loadgen
GW="${CHURN_GW:-$ROOT/target/release/wsprism-gateway}"
LG="$ROOT/target/release/wsprism-loadgen"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
sed 's|listen: "127.0.0.1:18080"|listen: "10.77.0.1:18080"|' "$ROOT/bench/gateway.yaml" > "$WORK/memory.yaml"
sed 's|^gateway:|gateway:\n  broker: { kind: redis, url: "redis://10.77.0.1:6390", prefix: bench, pubsub_connections: 4 }|' \
  "$WORK/memory.yaml" > "$WORK/redis.yaml"
grep -q 'kind: redis' "$WORK/redis.yaml"
# PRESENCE_EVENTS=1: the tenant announces joins and leaves (member_joined /
# member_left frames, one per join and leave).
if [[ ${PRESENCE_EVENTS:-0} == 1 ]]; then
  for c in memory redis; do sed -i 's|^    policy:|    policy:\n      presence_events: true|' "$WORK/$c.yaml"; done
  grep -q 'presence_events: true' "$WORK/redis.yaml"
fi

total=0
for run in $(seq "$RUNS"); do for c in $CASES; do for t in $TARGETS; do
  total=$((total + 1))
  workers="${c%x*}"; joins="${c#*x}"
  label="target=$t workers=$workers joins=$joins rooms=$ROOMS presence_events=${PRESENCE_EVENTS:-0} run=$run"
  if [[ -f "$OUT" ]] && grep -qF "\"label\":\"$label\"" "$OUT"; then
    echo "[$total] skip (done): $label" >&2; continue
  fi
  echo "[$total] $label" >&2
  gw="taskset -c 0-7 env RUST_LOG=warn $GW --config $WORK/$t.yaml"
  lg="taskset -c 8-22 $LG churn --url ws://10.77.0.1:18080 --workers $workers --joins $joins --rooms $ROOMS --duration $DURATION --seed $run --json --server-pid \$SERVER_PID --label '$label'"
  if [[ $t == memory ]]; then
    server="$gw"; client="$lg"
  else
    cat > "$WORK/server.sh" <<SRV
taskset -c 23 $RS --bind 10.77.0.1 --port 6390 --save "" --appendonly no --protected-mode no --loglevel warning >/dev/null &
until $RC -h 10.77.0.1 -p 6390 ping >/dev/null 2>&1; do sleep 0.05; done
exec $gw
SRV
    cat > "$WORK/client.sh" <<CLI
info() { $RC -h 10.77.0.1 -p 6390 info all | tr -d '\r' > "\$1"; }
info $WORK/before
$lg
info $WORK/after
$RC -h 10.77.0.1 -p 6390 shutdown nosave >/dev/null 2>&1 || true
python3 $ROOT/bench/redis-info-delta.py $WORK/before $WORK/after '$label'
CLI
    server="sh $WORK/server.sh"; client="sh $WORK/client.sh"
  fi
  "$ROOT/bench/netlab.sh" --srv-egress none --cli-egress none --server "$server" --client "$client" \
    2>>"${CHURN_LOG:-/dev/null}" >> "$OUT" || echo "[$total] FAILED: $label" >&2
done; done; done
echo "done: $total runs -> $OUT" >&2
