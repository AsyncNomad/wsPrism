#!/usr/bin/env bash
# A throwaway Redis for the cluster tests on an isolated docker network
# (--internal: no route out). Prints the URL to export:
#   export WSPRISM_TEST_REDIS=$(bench/redis-test.sh)
#   bench/redis-test.sh stop
set -euo pipefail
NET=wsp-redis-test; NAME=wsp-redis
if [[ ${1:-} == stop ]]; then
  docker rm -f $NAME >/dev/null 2>&1 || true
  docker network rm $NET >/dev/null 2>&1 || true
  exit 0
fi
docker network inspect $NET >/dev/null 2>&1 || docker network create --internal $NET >/dev/null
docker inspect $NAME >/dev/null 2>&1 || docker run -d --name $NAME --network $NET redis:7.2 \
  redis-server --save "" --appendonly no >/dev/null
# Left stopped (e.g. after a reboot): start it again.
[[ $(docker inspect -f '{{.State.Running}}' $NAME) == true ]] || docker start $NAME >/dev/null
IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' $NAME)
for _ in $(seq 50); do
  (exec 3<>/dev/tcp/$IP/6379) 2>/dev/null && break
  sleep 0.1
done
echo "redis://$IP:6379"
