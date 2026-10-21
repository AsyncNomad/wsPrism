#!/usr/bin/env bash
# Redis cluster broker cost (ADR 0011 A15): one gateway node with the Redis
# broker vs the single-node (memory) broker, same scenarios as
# run-compete.sh, inside bench/netlab.sh (traffic never leaves the machine).
#
#   bench/run-cluster.sh <name> [RUNS] [SCENARIOS] [TARGETS]
#
# TARGETS: memory-ext redis-ext redis-hot redis-hist
#   redis-hist: every room keeps history (the Lua publish path).
# Redis runs in the server namespace on its own core (REDIS_CPU, default 23);
# the gateway keeps cores 0-7 and the load generator gets 8-22. Redis's CPU
# and command counts come from INFO before and after each run and are written
# as a second JSON line ({"label": ..., "redis": {...}}) next to the
# loadgen's. NODES=2: the Redis targets run two gateway nodes (cores
# NODE_CPUS, default "0-3 4-7", ports 18080/18081) with every room spread
# over both; CPU/RSS are summed over the nodes. Binaries: the redis:7.2 image's redis-server / redis-cli,
# copied to ~/.cache/wsprism-bench/redis-7.2 (docker cp; no download).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NAME="${1:?name}"
RUNS="${2:-3}"
SCENARIOS="${3:-rooms:20 rooms:120 rooms:240 broadcast:10}"
TARGETS="${4:-memory-ext redis-ext redis-hot redis-hist}"
SERVER_CPUS="${SERVER_CPUS:-0-7}"
LOADGEN_CPUS="${LOADGEN_CPUS:-8-22}"
REDIS_CPU="${REDIS_CPU:-23}"
NODES="${NODES:-1}"
read -r NODE1_CPUS NODE2_CPUS <<<"${NODE_CPUS:-0-3 4-7}"
REDIS_DIR="${REDIS_DIR:-$HOME/.cache/wsprism-bench/redis-7.2}"
RS="$REDIS_DIR/redis-server"; RC="$REDIS_DIR/redis-cli"
[[ -x $RS && -x $RC ]] || { echo "missing $RS / $RC (docker cp from redis:7.2)" >&2; exit 1; }

OUT="$ROOT/bench/results/$NAME.jsonl"
cargo build --release -q --manifest-path "$ROOT/Cargo.toml" -p wsprism-gateway -p wsprism-loadgen
GW="$ROOT/target/release/wsprism-gateway"
LG="$ROOT/target/release/wsprism-loadgen"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
sed 's|listen: "127.0.0.1:18080"|listen: "10.77.0.1:18080"|' "$ROOT/bench/gateway.yaml" > "$WORK/memory.yaml"
sed 's|^gateway:|gateway:\n  broker: { kind: redis, url: "redis://10.77.0.1:6390", prefix: bench, pubsub_connections: 4 }|' \
  "$WORK/memory.yaml" > "$WORK/redis.yaml"
sed 's|^    policy:|    history: [{ rooms: "*", size: 200, ttl_s: 600 }]\n    policy:|' \
  "$WORK/redis.yaml" > "$WORK/hist.yaml"
grep -q 'kind: redis' "$WORK/redis.yaml" && grep -q 'history:' "$WORK/hist.yaml"
for c in redis hist; do sed 's|10.77.0.1:18080|10.77.0.1:18081|' "$WORK/$c.yaml" > "$WORK/${c}2.yaml"; done

scenario() {
  local kind="${1%%:*}" rate="${1#*:}"
  [[ "$rate" == "$1" ]] && rate=""
  case "$kind" in
    rooms) echo "--conns 1000 --rooms 100 --publishers 1000 --rate ${rate:-20} --payload 64 --duration 20" ;;
    broadcast) echo "--conns 10000 --rooms 10 --publishers 10 --rate ${rate:-10} --payload 64 --duration 20 --connect-concurrency 500" ;;
    *) echo "unknown scenario: $1" >&2; exit 2 ;;
  esac
}

# target -> "CONFIG|LANE"
target() {
  case "$1" in
    memory-ext) echo "memory|ext" ;;
    redis-ext) echo "redis|ext" ;;
    redis-hot) echo "redis|hot" ;;
    redis-hist) echo "hist|ext" ;;
    *) echo "unknown target: $1" >&2; exit 2 ;;
  esac
}

total=0
for run in $(seq "$RUNS"); do for sc in $SCENARIOS; do for t in $TARGETS; do
  total=$((total + 1))
  label="scenario=$sc target=$t run=$run"
  [[ $NODES == 2 && $t != memory-* ]] && label="scenario=$sc target=$t nodes=2 run=$run"
  if [[ -f "$OUT" ]] && grep -qF "\"label\":\"$label\"" "$OUT"; then
    echo "[$total] skip (done): $label" >&2; continue
  fi
  echo "[$total] $label" >&2
  IFS='|' read -r cfg lane <<<"$(target "$t")"
  gw="taskset -c $SERVER_CPUS env RUST_LOG=warn $GW --config $WORK/$cfg.yaml"
  lg="taskset -c $LOADGEN_CPUS $LG gateway --target wsprism --lane $lane --url ws://10.77.0.1:18080 --distinct-users $(scenario "$sc") --seed $run --json --server-pid \$SERVER_PID --label '$label'"
  if [[ $cfg == memory ]]; then
    server="$gw"; client="$lg"
  elif [[ $NODES == 2 ]]; then
    # Node 2 starts in the background (its pid in a file); node 1 is exec'd
    # (SERVER_PID). The client waits for node 2, then stops it afterwards.
    cat > "$WORK/server.sh" <<SRV
taskset -c $REDIS_CPU $RS --bind 10.77.0.1 --port 6390 --save "" --appendonly no --protected-mode no --loglevel warning >/dev/null &
until $RC -h 10.77.0.1 -p 6390 ping >/dev/null 2>&1; do sleep 0.05; done
taskset -c $NODE2_CPUS env RUST_LOG=warn $GW --config $WORK/${cfg}2.yaml &
echo \$! > $WORK/pid2
exec taskset -c $NODE1_CPUS env RUST_LOG=warn $GW --config $WORK/$cfg.yaml
SRV
    lg2="taskset -c $LOADGEN_CPUS $LG gateway --target wsprism --lane $lane --url ws://10.77.0.1:18080,ws://10.77.0.1:18081 --distinct-users $(scenario "$sc") --seed $run --json --server-pid \$SERVER_PID,\$(cat $WORK/pid2) --label '$label'"
    cat > "$WORK/client.sh" <<CLI
until [ -s $WORK/pid2 ] && (exec 3<>/dev/tcp/10.77.0.1/18081) 2>/dev/null; do sleep 0.05; done
info() { $RC -h 10.77.0.1 -p 6390 info all | tr -d '\r' > "\$1"; }
info $WORK/before
$lg2
info $WORK/after
kill \$(cat $WORK/pid2) 2>/dev/null || true
rm -f $WORK/pid2
$RC -h 10.77.0.1 -p 6390 shutdown nosave >/dev/null 2>&1 || true
python3 $ROOT/bench/redis-info-delta.py $WORK/before $WORK/after '$label'
CLI
    server="sh $WORK/server.sh"; client="bash $WORK/client.sh"
  else
    # Redis next to the gateway (same namespace, its own core); the gateway
    # is exec'd so SERVER_PID is the gateway. The client records Redis INFO
    # around the run and shuts Redis down. Scripts avoid nested quoting.
    cat > "$WORK/server.sh" <<SRV
taskset -c $REDIS_CPU $RS --bind 10.77.0.1 --port 6390 --save "" --appendonly no --protected-mode no --loglevel warning >/dev/null &
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
    2>>"${CLUSTER_LOG:-/dev/null}" >> "$OUT" || echo "[$total] FAILED: $label" >&2
done; done; done
echo "done: $total runs -> $OUT" >&2
