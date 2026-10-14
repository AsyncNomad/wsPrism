#!/usr/bin/env bash
# Validation V6: the statesync scenario through a MoQ relay (moq-relay-ietf)
# inside bench/netlab.sh. Same cell and JSON as bench/run-v.sh, transport
# "moq", label tick=0 (the relay forwards each object immediately; it has no
# tick). Results append to bench/results/<name>.jsonl.
#
#   bench/run-moq.sh <name> [RUNS] [LOSSES] [EXTRA_PROBE_ARGS]
#
# Needs: cargo install moq-relay-ietf --version 0.7.29 --locked
# Resumable like run-v.sh: a label already in the output file is skipped.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NAME="${1:?name}"
RUNS="${2:-3}"
LOSSES="${3:-none:20 uniform:1:20 burst:1:4:20}"
EXTRA="${4:---rooms 10 --room-size 10 --rate 60 --sample-hz 60 --warmup 3 --duration 10}"

OUT="$ROOT/bench/results/$NAME.jsonl"
mkdir -p "$ROOT/bench/results"
RELAY="${MOQ_RELAY:-$HOME/.cargo/bin/moq-relay-ietf}"
PROBE="$ROOT/spikes/moq-probe/target/release/moq-probe"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

(cd "$ROOT/spikes/moq-probe" && cargo build --release -q)
# Self-signed cert for the relay address inside netlab; clients skip verification.
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 10 \
  -subj "/CN=wsprism-v6" -addext "subjectAltName=IP:10.77.0.1" \
  -keyout "$WORK/key.pem" -out "$WORK/cert.pem" 2>/dev/null

profile() { case "$1" in none:*) echo "delay:${1#none:}" ;; *) echo "$1" ;; esac; }

total=0
for run in $(seq "$RUNS"); do for loss in $LOSSES; do
  total=$((total + 1))
  label="loss=$loss tick=0 run=$run"
  if [[ -f "$OUT" ]] && grep -F "\"label\":\"$label\"" "$OUT" | grep -qF '"transport":"moq"'; then
    echo "[$total] skip (done): moq $label" >&2
    continue
  fi
  echo "[$total] moq $label" >&2
  prof="$(profile "$loss")"
  "$ROOT/bench/netlab.sh" --srv-egress "$prof" --cli-egress "$prof" \
    --server "$RELAY --bind 10.77.0.1:4443 --tls-cert $WORK/cert.pem --tls-key $WORK/key.pem --coordinator-file $WORK/coord-$total.json" \
    --client "$PROBE --url https://10.77.0.1:4443/wsprism $EXTRA --server-pid \$SERVER_PID --label '$label'" \
    2>>"${MOQ_LOG:-/dev/null}" >> "$OUT" || echo "[$total] FAILED: moq $label" >&2
done; done
echo "done: $total runs -> $OUT" >&2
