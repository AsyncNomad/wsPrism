#!/usr/bin/env bash
# Same-session A/B of two gateway binaries, interleaved run by run (CPU noise
# between sessions is ~20%, so before/after must be measured together).
#   AB_OLD=<binary> AB_OUT=<name> AB_RATES="20 120 240" bench/run-ab-gateway.sh
# OLD defaults to a worktree build of the previous commit; NEW to target/release.
set -euo pipefail
ROOT=$HOME/code/wsPrism
OUT=$ROOT/bench/results/${AB_OUT:-ab-coalesce}.jsonl
OLD=${AB_OLD:-/tmp/claude-1000/wsp-old-target/release/wsprism-gateway}
NEW=$ROOT/target/release/wsprism-gateway
LG=$ROOT/target/release/wsprism-loadgen
W=$(mktemp -d); trap 'rm -rf $W' EXIT
# Per-variant config: AB_TICK_OLD / AB_TICK_NEW set policy.hot_tick_hz (same
# binary can be compared under two configs with AB_OLD=target/release/...).
for v in old new; do
  tick_var="AB_TICK_${v^^}"; tick="${!tick_var:-0}"
  sed 's|listen: "127.0.0.1:18080"|listen: "10.77.0.1:18080"|' $ROOT/bench/gateway.yaml \
    | sed "s|      hot_requires_active_room: true|      hot_requires_active_room: true\n      hot_tick_hz: $tick|" > $W/wsprism-$v.yaml
  grep -q "hot_tick_hz: $tick" $W/wsprism-$v.yaml
done
# AB_RATES entries: <rate> (rooms: 100 rooms of 10, all publish) or
# b<rate> (broadcast: 10 rooms of 1000, one publisher each).
scen() {
  case "$1" in
    b*) echo "--conns 10000 --rooms 10 --publishers 10 --rate ${1#b} --payload 64 --duration 20 --connect-concurrency 500" ;;
    *) echo "--conns 1000 --rooms 100 --publishers 1000 --rate $1 --payload 64 --duration 20" ;;
  esac
}
for run in 1 2 3; do for rate in ${AB_RATES:-20 60 120}; do for v in old new; do
  name="rooms:$rate"; [[ $rate == b* ]] && name="broadcast:${rate#b}"
  label="scenario=$name target=wsprism-ext-$v run=$run"
  grep -qF "\"label\":\"$label\"" $OUT 2>/dev/null && continue
  bin=$OLD; [[ $v == new ]] && bin=$NEW
  $ROOT/bench/netlab.sh --srv-egress none --cli-egress none \
    --server "taskset -c 0-7 env RUST_LOG=warn $bin --config $W/wsprism-$v.yaml" \
    --client "taskset -c 8-23 $LG gateway --target wsprism --lane ${AB_LANE:-ext} --url ws://10.77.0.1:18080 $(scen $rate) --seed $run --json --server-pid \$SERVER_PID --label '$label'" \
    2>/dev/null >> $OUT || echo "FAILED $label"
done; done; done
echo done
