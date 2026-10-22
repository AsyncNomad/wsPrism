#!/usr/bin/env bash
# A throwaway Redis primary + replica + one Sentinel (quorum 1, failure
# detected after 1 s) on an isolated docker network (--internal: no route
# out) at fixed addresses, for the Sentinel failover test. Prints the
# variables to export:
#   eval "$(bench/redis-sentinel-test.sh)"   # WSPRISM_TEST_SENTINEL, ..._PRIMARY
#   bench/redis-sentinel-test.sh stop
set -euo pipefail
NET=wsp-sentinel; SUBNET=172.30.78.0/24
P=172.30.78.10; R=172.30.78.11; S=172.30.78.20
DIR="${WSPRISM_SENTINEL_DIR:-$HOME/.cache/wsprism-bench/sentinel}"
if [[ ${1:-} == stop ]]; then
  docker rm -f wsp-sen-primary wsp-sen-replica wsp-sen-sentinel >/dev/null 2>&1 || true
  docker network rm $NET >/dev/null 2>&1 || true
  exit 0
fi

docker network inspect $NET >/dev/null 2>&1 \
  || docker network create --internal --subnet $SUBNET $NET >/dev/null
up() { # name ip args...
  local name=$1 ip=$2; shift 2
  docker container inspect "$name" >/dev/null 2>&1 \
    || docker run -d --name "$name" --network $NET --ip "$ip" "$@" >/dev/null
  [[ $(docker container inspect -f '{{.State.Running}}' "$name") == true ]] || docker start "$name" >/dev/null
}
up wsp-sen-primary $P redis:7.2 redis-server --save "" --appendonly no
up wsp-sen-replica $R redis:7.2 redis-server --save "" --appendonly no --replicaof $P 6379
# Sentinel rewrites its configuration file (and takes it over): a fresh,
# writable copy in a new directory whenever the container is created.
if ! docker container inspect wsp-sen-sentinel >/dev/null 2>&1; then
  CONF_DIR="$DIR/$(date +%s%N)"
  mkdir -p "$CONF_DIR"
  cat > "$CONF_DIR/sentinel.conf" <<CONF
port 26379
sentinel monitor mymaster $P 6379 1
sentinel down-after-milliseconds mymaster 1000
sentinel failover-timeout mymaster 3000
sentinel parallel-syncs mymaster 1
CONF
  chmod 777 "$CONF_DIR"; chmod 666 "$CONF_DIR/sentinel.conf"
  docker run -d --name wsp-sen-sentinel --network $NET --ip $S -v "$CONF_DIR:/conf" \
    redis:7.2 redis-sentinel /conf/sentinel.conf >/dev/null
fi
[[ $(docker container inspect -f '{{.State.Running}}' wsp-sen-sentinel) == true ]] || docker start wsp-sen-sentinel >/dev/null
for _ in $(seq 100); do
  (exec 3<>/dev/tcp/$S/26379) 2>/dev/null && break
  sleep 0.1
done
echo "export WSPRISM_TEST_SENTINEL=redis://$S:26379"
echo "export WSPRISM_TEST_SENTINEL_PRIMARY=wsp-sen-primary"
