#!/usr/bin/env bash
# Idle memory per connection (10k conns): one shared user vs one user per
# connection (--distinct-users), for one gateway binary.
#   MEM_GW=<binary> MEM_BUILD=<label> MEM_OUT=<name> bench/run-mem-users.sh
set -euo pipefail
ROOT=$HOME/code/wsPrism; OUT=$ROOT/bench/results/${MEM_OUT:-mem-users}.jsonl
GW=${MEM_GW:-$ROOT/target/release/wsprism-gateway}; LG=$ROOT/target/release/wsprism-loadgen
W=$(mktemp -d); trap 'rm -rf $W' EXIT
sed 's|listen: "127.0.0.1:18080"|listen: "10.77.0.1:18080"|' $ROOT/bench/gateway.yaml > $W/wsprism.yaml
for run in 1 2 3; do for mode in shared distinct; do
  label="scenario=idle10k users=$mode build=${MEM_BUILD:-base} run=$run"
  grep -qF "\"label\":\"$label\"" $OUT 2>/dev/null && continue
  extra=""; [[ $mode == distinct ]] && extra="--distinct-users"
  $ROOT/bench/netlab.sh --srv-egress none --cli-egress none \
    --server "taskset -c 0-7 env RUST_LOG=warn $GW --config $W/wsprism.yaml" \
    --client "taskset -c 8-23 $LG gateway --target wsprism --lane ext --url ws://10.77.0.1:18080 --conns 10000 --rooms 100 --publishers 0 --duration 5 --connect-concurrency 500 $extra --seed $run --json --server-pid \$SERVER_PID --label '$label'" \
    2>/dev/null >> $OUT || echo "FAILED $label"
done; done
echo done
