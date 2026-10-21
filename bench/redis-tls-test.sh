#!/usr/bin/env bash
# A throwaway TLS-only Redis for the TLS cluster test, on an isolated docker
# network (--internal: no route out) at a fixed address, with a private CA,
# a server certificate for that address and a client certificate (the
# server requires one: mutual TLS). Certificates are made locally with
# openssl. Prints the variables to export:
#   eval "$(bench/redis-tls-test.sh)"     # WSPRISM_TEST_REDIS_TLS, ..._TLS_DIR
#   bench/redis-tls-test.sh stop
set -euo pipefail
NET=wsp-redis-tls; NAME=wsp-redis-tls
SUBNET=172.30.77.0/24; IP=172.30.77.10; PORT=6380
DIR="${WSPRISM_REDIS_TLS_DIR:-$HOME/.cache/wsprism-bench/redis-tls}"
if [[ ${1:-} == stop ]]; then
  docker rm -f $NAME >/dev/null 2>&1 || true
  docker network rm $NET >/dev/null 2>&1 || true
  exit 0
fi

if [[ ! -f $DIR/client.key ]]; then
  mkdir -p "$DIR"
  (
    cd "$DIR"
    openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=wsprism test CA" \
      -keyout ca.key -out ca.crt 2>/dev/null
    openssl req -newkey rsa:2048 -nodes -subj "/CN=wsp-redis-tls" \
      -keyout server.key -out server.csr 2>/dev/null
    printf 'subjectAltName=IP:%s\nextendedKeyUsage=serverAuth\n' "$IP" > server.ext
    openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
      -days 3650 -extfile server.ext -out server.crt 2>/dev/null
    openssl req -newkey rsa:2048 -nodes -subj "/CN=wsprism-gateway" \
      -keyout client.key -out client.csr 2>/dev/null
    printf 'extendedKeyUsage=clientAuth\n' > client.ext
    openssl x509 -req -in client.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
      -days 3650 -extfile client.ext -out client.crt 2>/dev/null
    # Readable by the container's redis user (test material only).
    chmod 644 ./*.key
  )
fi

docker network inspect $NET >/dev/null 2>&1 \
  || docker network create --internal --subnet $SUBNET $NET >/dev/null
docker container inspect $NAME >/dev/null 2>&1 || docker run -d --name $NAME --network $NET --ip $IP \
  -v "$DIR:/tls:ro" redis:7.2 redis-server --save "" --appendonly no \
  --port 0 --tls-port $PORT \
  --tls-cert-file /tls/server.crt --tls-key-file /tls/server.key \
  --tls-ca-cert-file /tls/ca.crt --tls-auth-clients yes >/dev/null
[[ $(docker container inspect -f '{{.State.Running}}' $NAME) == true ]] || docker start $NAME >/dev/null
for _ in $(seq 50); do
  (exec 3<>/dev/tcp/$IP/$PORT) 2>/dev/null && break
  sleep 0.1
done
echo "export WSPRISM_TEST_REDIS_TLS=rediss://$IP:$PORT"
echo "export WSPRISM_TEST_REDIS_TLS_DIR=$DIR"
