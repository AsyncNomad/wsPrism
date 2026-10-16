#!/usr/bin/env bash
# CPU-profile the gateway under a fan-out load, inside bench/netlab.sh
# (traffic never leaves the machine). Needs `perf` and `inferno`.
#
#   bench/profile-gateway.sh <name> [SCENARIO_ARGS] [FEATURES]
#   -> bench/results/profile/<name>.{svg,top.txt} (+ <name>.folded, raw, not committed)
#
# Built with the `profiling` cargo profile (release + line tables) and frame
# pointers, sampled with --call-graph fp at 999 Hz. The CPU/latency numbers the
# load generator prints are NOT valid here (perf adds overhead); use the A/B
# runner for those.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NAME="${1:?name}"
SCEN="${2:---conns 1000 --rooms 100 --publishers 1000 --rate 120 --payload 64 --duration 15}"
FEATURES="${3:-}"
OUT="$ROOT/bench/results/profile"; mkdir -p "$OUT"
TGT="$ROOT/target/profiling-fp"
RUSTFLAGS="-C force-frame-pointers=yes" CARGO_TARGET_DIR="$TGT" \
  cargo build -q --profile profiling --manifest-path "$ROOT/Cargo.toml" -p wsprism-gateway ${FEATURES:+--features $FEATURES}
cargo build -q --release --manifest-path "$ROOT/Cargo.toml" -p wsprism-loadgen
GW="$TGT/profiling/wsprism-gateway"; LG="$ROOT/target/release/wsprism-loadgen"
W="$(mktemp -d)"; trap 'rm -rf "$W"' EXIT
sed 's|listen: "127.0.0.1:18080"|listen: "10.77.0.1:18080"|' "$ROOT/bench/gateway.yaml" > "$W/wsprism.yaml"
"$ROOT/bench/netlab.sh" --srv-egress none --cli-egress none \
  --server "taskset -c 0-7 perf record -F 999 --call-graph fp -o $W/perf.data -- env RUST_LOG=warn $GW --config $W/wsprism.yaml" \
  --client "taskset -c 8-23 $LG gateway --target wsprism --lane ext --url ws://10.77.0.1:18080 $SCEN --seed 1 --json --label 'profile $NAME'" \
  > "$OUT/$NAME.loadgen.json" 2>"$W/netlab.err" || true
sleep 2   # let perf finish writing after the server is stopped
perf script -i "$W/perf.data" 2>/dev/null | inferno-collapse-perf > "$OUT/$NAME.folded"
inferno-flamegraph < "$OUT/$NAME.folded" > "$OUT/$NAME.svg"
python3 "$ROOT/bench/profile-top.py" "$OUT/$NAME.folded" > "$OUT/$NAME.top.txt"
echo "profile -> $OUT/$NAME.{folded,svg,top.txt}"
