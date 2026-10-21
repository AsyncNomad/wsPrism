#!/usr/bin/env bash
# Run the standard wsPrism benchmark scenarios against a local release build.
#
#   bench/run-baseline.sh [label]
#
# Starts the gateway with bench/gateway.yaml, runs each scenario with
# wsprism-loadgen, samples the gateway's RSS, and appends JSON lines to
# bench/results/<label>.jsonl. Methodology and caveats: docs/BENCHMARKS.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LABEL="${1:-$(git -C "$ROOT" rev-parse --short HEAD)}"
OUT="$ROOT/bench/results/$LABEL.jsonl"
URL="ws://127.0.0.1:18080"
mkdir -p "$ROOT/bench/results"
: > "$OUT"

cargo build --release -q --manifest-path "$ROOT/Cargo.toml" -p wsprism-gateway -p wsprism-loadgen
GW="$ROOT/target/release/wsprism-gateway"
LG="$ROOT/target/release/wsprism-loadgen"

WORK="$(mktemp -d)"
"$GW" --config "$ROOT/bench/gateway.yaml" > "$WORK/gateway.log" 2>&1 &
GW_PID=$!
trap 'kill $GW_PID 2>/dev/null || true; rm -rf "$WORK"' EXIT
for _ in $(seq 50); do curl -sf http://127.0.0.1:18080/healthz >/dev/null && break; sleep 0.1; done

rss_kb() { awk '/^VmRSS:/ {print $2}' "/proc/$GW_PID/status"; }

# run <name> <loadgen args...>: runs one scenario, recording peak gateway RSS.
run() {
  local name="$1"; shift
  local base peak=0 tmp
  base="$(rss_kb)"
  tmp="$(mktemp)"
  "$LG" gateway --url "$URL" --json "$@" > "$tmp" &
  local lg=$!
  while kill -0 "$lg" 2>/dev/null; do
    local r; r="$(rss_kb)"; (( r > peak )) && peak=$r
    sleep 0.2
  done
  wait "$lg"
  local line; line="$(cat "$tmp")"; rm -f "$tmp"
  echo "{\"scenario\":\"$name\",\"gateway_rss_base_kb\":$base,\"gateway_rss_peak_kb\":$peak,\"result\":$line}" | tee -a "$OUT"
  sleep 1 # let sessions close before the next scenario
}

#   name               conns  rooms  pubs  rate  lane  payload  duration
run idle_10k           --conns 10000 --rooms 100 --publishers 0                      --duration 5
run hot_100x10         --conns 1000  --rooms 10  --publishers 10 --rate 100 --lane hot --payload 64  --duration 10
run ext_100x10         --conns 1000  --rooms 10  --publishers 10 --rate 100 --lane ext --payload 64  --duration 10
run hot_room1000       --conns 1000  --rooms 1   --publishers 1  --rate 50  --lane hot --payload 64  --duration 10
run ext_room1000       --conns 1000  --rooms 1   --publishers 1  --rate 50  --lane ext --payload 64  --duration 10
run hot_heavy          --conns 5000  --rooms 50  --publishers 50 --rate 200 --lane hot --payload 64  --duration 10

echo "results: $OUT"
