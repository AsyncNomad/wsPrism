#!/usr/bin/env bash
# Many idle connections (external review #4, A1): process memory per
# connection where it decides server cost, the kernel's share, and the
# latency of light traffic on top. wsPrism and Centrifugo, same netlab.
#
#   bench/run-idle-scale.sh [NAME] [CONNS] [RUNS] [TARGETS] [SCENARIOS]
#     NAME       results file bench/results/NAME.jsonl (default idle-scale)
#     CONNS      connections (default 50000; mind the machine: both ends
#                and the load generator share its memory)
#     TARGETS    wsprism centrifugo
#     SCENARIOS  idle (nobody publishes) light (100 publishers x 10/s in
#                1,000 rooms: at 50k connections ~49k deliveries/s)
#
# Each record adds `kernel`: /proc/meminfo and /proc/net/sockstat deltas from
# before the client starts to the moment the most client sockets are open.
# Both ends of every connection are on this machine, so the kernel delta
# covers two sockets per connection; `kernel.kb_per_conn_server_est` halves
# it (assumes both ends cost the same, which holds for idle sockets).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NAME="${1:-idle-scale}"
CONNS="${2:-50000}"
RUNS="${3:-1}"
TARGETS="${4:-wsprism centrifugo}"
SCENARIOS="${5:-idle light}"
SERVER_CPUS="${SERVER_CPUS:-0-7}"
LOADGEN_CPUS="${LOADGEN_CPUS:-8-23}"
# Client source addresses: ~60k connections each.
ADDRS=$(( (CONNS + 59999) / 60000 ))
LOCAL=$(seq -s, -f "10.77.0.%g" 2 $((ADDRS + 1)))

OUT="$ROOT/bench/results/$NAME.jsonl"
mkdir -p "$ROOT/bench/results"
TOOLS="${WSPRISM_BENCH_TOOLS:-$HOME/.cache/wsprism-bench}"
CENTRIFUGO="$TOOLS/centrifugo-6.9.7/centrifugo"
[[ -x "$CENTRIFUGO" ]] || { echo "run bench/competitors/fetch.sh first" >&2; exit 2; }
cargo build --release -q --manifest-path "$ROOT/Cargo.toml" -p wsprism-gateway -p wsprism-loadgen
GW="$ROOT/target/release/wsprism-gateway"
LG="$ROOT/target/release/wsprism-loadgen"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
sed 's|listen: "127.0.0.1:18080"|listen: "10.77.0.1:18080"|' "$ROOT/bench/gateway.yaml" > "$WORK/wsprism.yaml"

scenario() {
  case "$1" in
    idle) echo "--rooms 1000 --publishers 0 --duration 10" ;;
    light) echo "--rooms 1000 --publishers 100 --rate 10 --payload 64 --duration 20" ;;
    *) echo "unknown scenario: $1" >&2; exit 2 ;;
  esac
}
target() {
  case "$1" in
    wsprism) echo "env RUST_LOG=error $GW --config $WORK/wsprism.yaml|--target wsprism --lane ext --distinct-users --url ws://10.77.0.1:18080" ;;
    centrifugo) echo "$CENTRIFUGO --config $ROOT/bench/competitors/centrifugo.json|--target centrifugo --url ws://10.77.0.1:9101" ;;
    *) echo "unknown target: $1" >&2; exit 2 ;;
  esac
}

# Kernel snapshot: one line of key=value (kB, pages for tcp_mem).
cat > "$WORK/snap.sh" <<'EOF'
#!/usr/bin/env bash
awk '/^(MemAvailable|Slab|SUnreclaim|SReclaimable):/{gsub(":","",$1); printf "%s=%s ", $1, $2}' /proc/meminfo
awk '/^TCP:/{print "tcp_inuse=" $3 " tcp_mem_pages=" $NF}' /proc/net/sockstat
EOF
# Wait until the kernel has finished freeing the previous run (sockets,
# a torn-down network namespace): Slab steady within 4 MB over 2 s, at most
# 30 s. A baseline taken earlier made the kernel delta swing 1-10 KB/conn.
cat > "$WORK/settle.sh" <<'EOF'
prev=0
for _ in $(seq 15); do
  cur=$(awk '/^Slab:/{print $2}' /proc/meminfo)
  (( prev > 0 && cur - prev < 4096 && prev - cur < 4096 )) && break
  prev=$cur; sleep 2
done
EOF
chmod +x "$WORK/snap.sh" "$WORK/settle.sh"

for run in $(seq "$RUNS"); do for sc in $SCENARIOS; do for t in $TARGETS; do
  label="scenario=$sc target=$t conns=$CONNS run=$run"
  if [[ -f "$OUT" ]] && grep -qF "\"label\":\"$label\"" "$OUT"; then
    echo "skip (done): $label" >&2; continue
  fi
  echo "$label" >&2
  IFS='|' read -r server lgargs <<<"$(target "$t")"
  # e.g. BEFORE_RUN='pkill -f some-memory-hog' (the caller's choice)
  [[ -n "${BEFORE_RUN:-}" ]] && eval "$BEFORE_RUN"
  K="$WORK/kernel.log"; : > "$K"
  NETLAB_CLIENT_ADDRS=$ADDRS "$ROOT/bench/netlab.sh" --srv-egress none --cli-egress none \
    --server "taskset -c $SERVER_CPUS $server" \
    --client "$WORK/settle.sh; $WORK/snap.sh > $K.base; (while sleep 1; do $WORK/snap.sh >> $K; done) & S=\$!; \
      taskset -c $LOADGEN_CPUS $LG gateway $lgargs --conns $CONNS $(scenario "$sc") \
        --connect-concurrency 500 --local-addrs $LOCAL --seed $run --json \
        --server-pid \$SERVER_PID --label '$label'; kill \$S" \
    2>>"${IDLE_SCALE_LOG:-/dev/null}" > "$WORK/rec.json" || { echo "FAILED: $label" >&2; continue; }
  python3 - "$WORK/rec.json" "$K.base" "$K" "$CONNS" >> "$OUT" <<'EOF'
import json, sys
rec = json.load(open(sys.argv[1]))
parse = lambda l: {k: int(v) for k, v in (kv.split("=") for kv in l.split())}
base = parse(open(sys.argv[2]).read())
snaps = [parse(l) for l in open(sys.argv[3]) if l.strip()]
peak = max(snaps, key=lambda s: s["tcp_inuse"])
conns = int(sys.argv[4])
d = {k: peak[k] - base[k] for k in base}
rec["kernel"] = {
    "client_sockets": peak["tcp_inuse"],
    "slab_kb": d["Slab"], "tcp_buf_kb": d["tcp_mem_pages"] * 4,
    "kb_per_conn_both_ends": round((d["Slab"] + d["tcp_mem_pages"] * 4) / conns, 2),
    "kb_per_conn_server_est": round((d["Slab"] + d["tcp_mem_pages"] * 4) / conns / 2, 2),
}
print(json.dumps(rec))
EOF
  tail -1 "$OUT" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(" ", d["label"], "rss/conn", d["server_rss_kb_per_conn"], "kernel", d["kernel"], "lat_us", d.get("latency_us"), "delivery", d.get("delivery_ratio"))' >&2
done; done; done
echo "done -> $OUT" >&2
