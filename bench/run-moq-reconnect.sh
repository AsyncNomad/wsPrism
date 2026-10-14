#!/usr/bin/env bash
# Validation V6 M-5: one subscriber watches one publisher's track through
# moq-relay-ietf while the publisher goes away and comes back under the same
# namespace. Inside bench/netlab.sh (20 ms one-way, no loss).
#
#   bench/run-moq-reconnect.sh <abrupt|graceful> [GAP_S]
#
# abrupt:   publisher killed with SIGKILL after 3 s (no CONNECTION_CLOSE; the
#           relay notices only via the 10 s QUIC idle timeout), restarted
#           GAP_S later.
# graceful: publisher closes its session after 3 s, restarted GAP_S later.
# The JSON event log (wall-clock ms, both processes) goes to
# bench/results/moq-reconnect-<mode>-<gap>.log.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MODE="${1:?abrupt|graceful}"
GAP="${2:-0.5}"
RELAY="${MOQ_RELAY:-$HOME/.cargo/bin/moq-relay-ietf}"
P="$ROOT/spikes/moq-probe/target/release/moq-probe"
U="https://10.77.0.1:4443/wsprism"
OUT="$ROOT/bench/results/moq-reconnect-$MODE-$GAP.log"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

(cd "$ROOT/spikes/moq-probe" && cargo build --release -q)
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 10 \
  -subj "/CN=wsprism-v6" -addext "subjectAltName=IP:10.77.0.1" \
  -keyout "$WORK/key.pem" -out "$WORK/cert.pem" 2>/dev/null

case "$MODE" in
  abrupt)   FIRST="$P pub --url $U --secs 1000 & A=\$!; sleep 3; kill -9 \$A; echo '{\"event\":\"pub_killed\",\"t_ms\":'\$(date +%s%3N)'}' >&2" ;;
  graceful) FIRST="$P pub --url $U --secs 3 --graceful" ;;
  *) echo "mode: abrupt|graceful" >&2; exit 2 ;;
esac

"$ROOT/bench/netlab.sh" --srv-egress delay:20 --cli-egress delay:20 \
  --server "$RELAY --bind 10.77.0.1:4443 --tls-cert $WORK/cert.pem --tls-key $WORK/key.pem --coordinator-file $WORK/coord.json" \
  --client "{ $P sub --url $U --secs 25 & S=\$!; sleep 0.5; $FIRST; sleep $GAP; $P pub --url $U --secs 15 --graceful; wait \$S; } 2>&1" \
  > "$OUT"
echo "-> $OUT" >&2
