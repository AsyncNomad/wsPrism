#!/usr/bin/env bash
# Validation V matrix (docs/design/hot-lane-over-webtransport.md §7): runs the
# spike relay and `wsprism-loadgen statesync` inside bench/netlab.sh for every
# combination and appends one JSON line per run to bench/results/<name>.jsonl.
#
#   bench/run-v.sh <name> [RUNS] [LOSSES] [TICKS] [TRANSPORTS] [EXTRA_LOADGEN_ARGS]
#   RELAY_EXTRA="--quic-lean" bench/run-v.sh ...   (extra relay flags)
#
# Defaults = the G1/G5 judgment cell: 20 ms one-way, 60 Hz, 10 rooms x 10.
# Order interleaves transports within each (run, loss, tick) so all three are
# measured close together in time.
#
# Resumable: a (transport, label) already present in the output file is
# skipped, so re-running the same command after an interruption only runs
# what is missing. Delete the file to start over.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NAME="${1:?name}"
RUNS="${2:-3}"
LOSSES="${3:-none:20 uniform:1:20 burst:1:4:20}"
TICKS="${4:-0 60 30}"
TRANSPORTS="${5:-ws wss wt}"
EXTRA="${6:---rooms 10 --room-size 10 --rate 60 --sample-hz 60 --warmup 3 --duration 10}"

OUT="$ROOT/bench/results/$NAME.jsonl"
mkdir -p "$ROOT/bench/results"
SPIKE="$ROOT/spikes/wt-hotlane/target/release/wt-hotlane-spike"
LG="$ROOT/target/release/wsprism-loadgen"
CERT="$(mktemp -d)"
trap 'rm -rf "$CERT"' EXIT

(cd "$ROOT/spikes/wt-hotlane" && cargo build --release -q)
cargo build --release -q --manifest-path "$ROOT/Cargo.toml" -p wsprism-loadgen

port() { case "$1" in ws) echo 9001 ;; wss) echo 9002 ;; wt) echo 9003 ;; esac; }
profile() { # "none:20" means no loss, 20 ms delay
  case "$1" in none:*) echo "delay:${1#none:}" ;; *) echo "$1" ;; esac
}

total=0
for run in $(seq "$RUNS"); do for loss in $LOSSES; do for tick in $TICKS; do for t in $TRANSPORTS; do
  total=$((total + 1))
  prof="$(profile "$loss")"
  label="loss=$loss tick=$tick run=$run"
  if [[ -f "$OUT" ]] && grep -qF "\"label\":\"$label\"" "$OUT" \
      && grep -F "\"label\":\"$label\"" "$OUT" | grep -qF "\"transport\":\"$t\""; then
    echo "[$total] skip (done): $t $label" >&2
    continue
  fi
  echo "[$total] $t $label" >&2
  "$ROOT/bench/netlab.sh" --srv-egress "$prof" --cli-egress "$prof" \
    --server "$SPIKE --$t 10.77.0.1:$(port "$t") --tick-hz $tick --cert-dir $CERT ${RELAY_EXTRA:-}" \
    --client "$LG statesync --transport $t --addr 10.77.0.1:$(port "$t") $EXTRA --server-pid \$SERVER_PID --label '$label'" \
    2>/dev/null >> "$OUT" || echo "[$total] FAILED: $t $label" >&2
done; done; done; done
echo "done: $total runs -> $OUT" >&2
