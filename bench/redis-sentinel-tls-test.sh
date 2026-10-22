#!/usr/bin/env bash
# Like redis-sentinel-test.sh, but everything over TLS with client
# certificates required (mutual TLS): a primary, a replica (replicating
# over TLS) and one Sentinel (quorum 1, talking TLS to both), on an isolated
# docker network (--internal: no route out) at fixed addresses. A private CA
# made locally with openssl; each server certificate names its address
# (Sentinel hands out addresses, and clients check them against the
# certificate). Prints the variables to export:
#   eval "$(bench/redis-sentinel-tls-test.sh)"
#     # WSPRISM_TEST_SENTINEL_TLS, ..._TLS_PRIMARY, ..._TLS_DIR
#   bench/redis-sentinel-tls-test.sh stop
# WSPRISM_SENTINEL_DOWN_AFTER_MS (default 1000), as in redis-sentinel-test.sh.
set -euo pipefail
NET=wsp-sentinel-tls; SUBNET=172.30.79.0/24
P=172.30.79.10; R=172.30.79.11; S=172.30.79.20
DIR="${WSPRISM_SENTINEL_TLS_DIR:-$HOME/.cache/wsprism-bench/sentinel-tls}"
if [[ ${1:-} == stop ]]; then
  docker rm -f wsp-stls-primary wsp-stls-replica wsp-stls-sentinel >/dev/null 2>&1 || true
  docker network rm $NET >/dev/null 2>&1 || true
  exit 0
fi

if [[ ! -f $DIR/client.key ]]; then
  mkdir -p "$DIR"
  (
    cd "$DIR"
    openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=wsprism test CA" \
      -keyout ca.key -out ca.crt 2>/dev/null
    # One server certificate for all three (their addresses as SANs); Redis
    # and Sentinel also present it as clients to each other.
    openssl req -newkey rsa:2048 -nodes -subj "/CN=wsp-sentinel-tls" \
      -keyout server.key -out server.csr 2>/dev/null
    printf 'subjectAltName=IP:%s,IP:%s,IP:%s\nextendedKeyUsage=serverAuth,clientAuth\n' "$P" "$R" "$S" > server.ext
    openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
      -days 3650 -extfile server.ext -out server.crt 2>/dev/null
    openssl req -newkey rsa:2048 -nodes -subj "/CN=wsprism-gateway" \
      -keyout client.key -out client.csr 2>/dev/null
    printf 'extendedKeyUsage=clientAuth\n' > client.ext
    openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
      -days 3650 -extfile client.ext -out client.crt 2>/dev/null
    chmod 644 ./*.key # readable by the containers' redis user (test material only)
  )
fi

docker network inspect $NET >/dev/null 2>&1 \
  || docker network create --internal --subnet $SUBNET $NET >/dev/null
TLS=(--port 0 --tls-port 6379 --tls-cert-file /tls/server.crt --tls-key-file /tls/server.key
     --tls-ca-cert-file /tls/ca.crt --tls-auth-clients yes --tls-replication yes)
up() { # name ip args...
  local name=$1 ip=$2; shift 2
  docker container inspect "$name" >/dev/null 2>&1 \
    || docker run -d --name "$name" --network $NET --ip "$ip" -v "$DIR:/tls:ro" "$@" >/dev/null
  [[ $(docker container inspect -f '{{.State.Running}}' "$name") == true ]] || docker start "$name" >/dev/null
}
up wsp-stls-primary $P redis:7.2 redis-server --save "" --appendonly no "${TLS[@]}"
up wsp-stls-replica $R redis:7.2 redis-server --save "" --appendonly no "${TLS[@]}" --replicaof $P 6379
if ! docker container inspect wsp-stls-sentinel >/dev/null 2>&1; then
  CONF_DIR="$DIR/conf-$(date +%s%N)"
  mkdir -p "$CONF_DIR"
  cat > "$CONF_DIR/sentinel.conf" <<CONF
port 0
tls-port 26379
tls-cert-file /tls/server.crt
tls-key-file /tls/server.key
tls-ca-cert-file /tls/ca.crt
tls-auth-clients yes
tls-replication yes
sentinel monitor mymaster $P 6379 1
sentinel down-after-milliseconds mymaster ${WSPRISM_SENTINEL_DOWN_AFTER_MS:-1000}
sentinel failover-timeout mymaster 3000
sentinel parallel-syncs mymaster 1
CONF
  chmod 777 "$CONF_DIR"; chmod 666 "$CONF_DIR/sentinel.conf"
  docker run -d --name wsp-stls-sentinel --network $NET --ip $S -v "$DIR:/tls:ro" -v "$CONF_DIR:/conf" \
    redis:7.2 redis-sentinel /conf/sentinel.conf >/dev/null
fi
[[ $(docker container inspect -f '{{.State.Running}}' wsp-stls-sentinel) == true ]] || docker start wsp-stls-sentinel >/dev/null
for _ in $(seq 100); do
  (exec 3<>/dev/tcp/$S/26379) 2>/dev/null && break
  sleep 0.1
done
echo "export WSPRISM_TEST_SENTINEL_TLS=rediss://$S:26379"
echo "export WSPRISM_TEST_SENTINEL_TLS_PRIMARY=wsp-stls-primary"
echo "export WSPRISM_TEST_SENTINEL_TLS_DIR=$DIR"
