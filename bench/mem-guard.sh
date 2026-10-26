#!/usr/bin/env bash
# Run a benchmark, and kill it if the machine runs low on memory.
#
#   bench/mem-guard.sh CMD [ARGS...]
#     MEM_GUARD_MIN_MB   kill below this MemAvailable (default 3000)
#     MEM_GUARD_LOG      where to log MemAvailable and the bench processes'
#                        RSS every 0.5 s (default /dev/null)
#
# Server, client and both ends' kernel sockets share one machine in these
# benchmarks. Running out of memory there does not fail the benchmark, it
# takes the machine (or a WSL VM, with everything in it) down: a 100k
# connection run did, twice. CMD runs in its own session, and its whole
# process group is killed.
set -uo pipefail
MIN_KB=$(( ${MEM_GUARD_MIN_MB:-3000} * 1024 ))
LOG=${MEM_GUARD_LOG:-/dev/null}
setsid "$@" & P=$!
while kill -0 "$P" 2>/dev/null; do
  avail=$(awk '/MemAvailable/{print $2}' /proc/meminfo)
  procs=$(ps -eo rss,comm | awk '/wsprism-|centrifugo|sockudo|nginx/{a[$2]+=$1} END{for(k in a) printf "%s=%dMB ",k,a[k]/1024}')
  echo "$(date +%T) avail=$((avail / 1024))MB $procs" >> "$LOG"
  if (( avail < MIN_KB )); then
    echo "mem-guard: MemAvailable $((avail / 1024)) MB, killing the benchmark" >&2
    kill -9 -- -"$P" 2>/dev/null
  fi
  sleep 0.5
done
wait "$P"
