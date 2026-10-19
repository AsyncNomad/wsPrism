#!/usr/bin/env bash
# Instructions per delivery (or per operation) on the engine paths, counted
# exactly with callgrind (ADR 0011 B2). WSL2 has no CPU PMU, so `perf stat
# -e instructions` is unavailable; callgrind is deterministic instead.
#   ICOUNT_BUILD=<label> ICOUNT_OUT=<name> bench/run-icount.sh [scenario...]
# Appends one JSON line per scenario to bench/results/<name>.jsonl.
set -euo pipefail
ROOT=$HOME/code/wsPrism; OUT=$ROOT/bench/results/${ICOUNT_OUT:-icount}.jsonl
BIN=$ROOT/target/profiling/examples/icount
command -v valgrind >/dev/null || { echo "valgrind not installed" >&2; exit 1; }
cargo build --manifest-path $ROOT/Cargo.toml --profile profiling -q -p wsprism-engine --example icount
REV=$(git -C $ROOT rev-parse --short HEAD)$(git -C $ROOT diff --quiet HEAD -- crates || echo -dirty)
W=$(mktemp -d); trap 'rm -rf $W' EXIT
scenarios=("$@"); [[ ${#scenarios[@]} -eq 0 ]] && scenarios=(ext hot hot_keyed api churn disconnect)
# Fan-out paths repeat to <0.01%; join/leave paths hash with random seeds
# (std HashMap, DashMap) and vary ~0.5%, so take the median of 3 runs.
for s in "${scenarios[@]}"; do
  irs=()
  for run in 1 2 3; do
    units=$(valgrind -q --tool=callgrind --toggle-collect='icount::measured*' \
      --callgrind-out-file=$W/$s.out $BIN $s | sed -n 's/^units=//p')
    irs+=("$(awk '/^totals:/ {print $2}' $W/$s.out)")
  done
  read -r lo ir hi < <(printf '%s\n' "${irs[@]}" | sort -n | xargs)
  printf '{"scenario":"%s","build":"%s","rev":"%s","ir":%s,"units":%s,"ir_per_unit":%s,"spread_pct":%s}\n' \
    "$s" "${ICOUNT_BUILD:-base}" "$REV" "$ir" "$units" \
    "$(awk "BEGIN{printf \"%.1f\", $ir/$units}")" "$(awk "BEGIN{printf \"%.2f\", ($hi-$lo)*100/$ir}")" \
    | tee -a $OUT
done
