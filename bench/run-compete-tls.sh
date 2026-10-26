#!/usr/bin/env bash
# The fan-out comparison over wss:// (external review #4, A2): production
# traffic is encrypted, and the cost of TLS changes both CPU and memory
# ratios.
#
#   bench/run-compete-tls.sh <name> [RUNS] [SCENARIOS] [TARGETS]
#     SCENARIOS  as bench/run-compete.sh (default "rooms:20 broadcast:10")
#     TARGETS    wsprism-tls wsprism-nginx centrifugo
#
# wsprism-tls: the gateway terminates TLS itself (`gateway.tls`).
# wsprism-nginx: behind a TLS-terminating proxy, here nginx (Ubuntu's 1.24 package,
# unpacked under the bench tools directory; nothing installed) on the same
# server cores, proxying to the gateway on loopback. The server cost is
# nginx + gateway (CPU and RSS summed). Centrifugo terminates TLS itself.
# Certificates: a private test CA made by this script; the load generator
# trusts it with --ca-file. PAYLOAD as in run-compete.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NAME="${1:?name}"
RUNS="${2:-3}"
SCENARIOS="${3:-rooms:20 broadcast:10}"
TARGETS="${4:-wsprism-tls wsprism-nginx centrifugo}"
SERVER_CPUS="${SERVER_CPUS:-0-7}"
LOADGEN_CPUS="${LOADGEN_CPUS:-8-23}"
NGINX_WORKERS="${NGINX_WORKERS:-8}"

OUT="$ROOT/bench/results/$NAME.jsonl"
mkdir -p "$ROOT/bench/results"
TOOLS="${WSPRISM_BENCH_TOOLS:-$HOME/.cache/wsprism-bench}"
CENTRIFUGO="$TOOLS/centrifugo-6.9.7/centrifugo"
NGINX_DIR="$TOOLS/nginx"
NGINX="$NGINX_DIR/root/usr/sbin/nginx"
if [[ ! -x "$NGINX" ]]; then
  mkdir -p "$NGINX_DIR/dl"
  (cd "$NGINX_DIR/dl" && apt-get download nginx)
  dpkg-deb -x "$NGINX_DIR"/dl/nginx_*.deb "$NGINX_DIR/root"
fi
[[ -x "$CENTRIFUGO" ]] || "$ROOT/bench/competitors/fetch.sh" >/dev/null
cargo build --release -q --manifest-path "$ROOT/Cargo.toml" -p wsprism-gateway -p wsprism-loadgen
GW="$ROOT/target/release/wsprism-gateway"
LG="$ROOT/target/release/wsprism-loadgen"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Private test CA and a server certificate for 10.77.0.1.
(
  cd "$WORK"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 2 \
    -subj "/CN=wsprism-bench-ca" -keyout ca.key -out ca.pem 2>/dev/null
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -subj "/CN=10.77.0.1" -keyout server.key -out server.csr 2>/dev/null
  printf 'subjectAltName=IP:10.77.0.1\n' > ext.cnf
  openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 2 \
    -extfile ext.cnf -out server.pem 2>/dev/null
)

# The gateway listens on loopback; nginx terminates TLS in front of it.
cp "$ROOT/bench/gateway.yaml" "$WORK/wsprism.yaml"   # listens on 127.0.0.1:18080
# The same config terminating TLS itself on the public address.
sed -e 's|listen: "127.0.0.1:18080"|listen: "10.77.0.1:18443"|' \
    -e "s|^gateway:|gateway:\n  tls: { cert_file: \"$WORK/server.pem\", key_file: \"$WORK/server.key\" }|" \
    "$ROOT/bench/gateway.yaml" > "$WORK/wsprism-tls.yaml"
grep -q 'tls: { cert_file' "$WORK/wsprism-tls.yaml"
mkdir -p "$WORK/nginx/tmp"
cat > "$WORK/nginx.conf" <<EOF
daemon off;
# netlab's user namespace makes nginx root there; nobody is not mapped in
# it, so stay "root" (this user) instead of switching to nobody.
user root root;
worker_processes $NGINX_WORKERS;
worker_rlimit_nofile 1048576;
pid $WORK/nginx/nginx.pid;
error_log $WORK/nginx/error.log error;
events { worker_connections 65536; multi_accept on; }
http {
  access_log off;
  client_body_temp_path $WORK/nginx/tmp/body;
  proxy_temp_path $WORK/nginx/tmp/proxy;
  fastcgi_temp_path $WORK/nginx/tmp/fastcgi;
  uwsgi_temp_path $WORK/nginx/tmp/uwsgi;
  scgi_temp_path $WORK/nginx/tmp/scgi;
  upstream gateway { server 127.0.0.1:18080; }
  server {
    listen 10.77.0.1:18443 ssl;
    ssl_certificate $WORK/server.pem;
    ssl_certificate_key $WORK/server.key;
    ssl_protocols TLSv1.3;
    tcp_nodelay on;
    location / {
      proxy_pass http://gateway;
      proxy_http_version 1.1;
      proxy_set_header Upgrade \$http_upgrade;
      proxy_set_header Connection "upgrade";
      proxy_read_timeout 1h;
      proxy_buffering off;
    }
  }
}
EOF

# Centrifugo with its own TLS, on the bench config otherwise unchanged.
python3 - "$ROOT/bench/competitors/centrifugo.json" "$WORK" > "$WORK/centrifugo.json" <<'EOF'
import json, sys
c = json.load(open(sys.argv[1]))
w = sys.argv[2]
c["http_server"]["tls"] = {"enabled": True, "cert_pem": f"{w}/server.pem", "key_pem": f"{w}/server.key"}
print(json.dumps(c))
EOF

scenario() {
  local kind="${1%%:*}" rate="${1#*:}"
  [[ "$rate" == "$1" ]] && rate=""
  case "$kind" in
    rooms) echo "--conns 1000 --rooms 100 --publishers 1000 --rate ${rate:-20} --payload ${PAYLOAD:-64} --duration 20" ;;
    broadcast) echo "--conns 10000 --rooms 10 --publishers 10 --rate ${rate:-10} --payload ${PAYLOAD:-64} --duration 20 --connect-concurrency 500" ;;
    *) echo "unknown scenario: $1" >&2; exit 2 ;;
  esac
}

# target NAME -> "SERVER_CMD|LOADGEN_TARGET_ARGS|EXTRA_PIDS_CMD"
target() {
  case "$1" in
    wsprism-tls) echo "exec env RUST_LOG=warn $GW --config $WORK/wsprism-tls.yaml|--target wsprism --lane ext --distinct-users --url wss://10.77.0.1:18443|" ;;
    wsprism-nginx) echo "$NGINX -p $WORK/nginx -e $WORK/nginx/error.log -c $WORK/nginx.conf & exec env RUST_LOG=warn $GW --config $WORK/wsprism.yaml|--target wsprism --lane ext --distinct-users --url wss://10.77.0.1:18443|pgrep -d, -f '^nginx: (master|worker)'" ;;
    centrifugo) echo "$CENTRIFUGO --config $WORK/centrifugo.json|--target centrifugo --url wss://10.77.0.1:9101|" ;;
    *) echo "unknown target: $1" >&2; exit 2 ;;
  esac
}

total=0
for run in $(seq "$RUNS"); do for sc in $SCENARIOS; do for t in $TARGETS; do
  total=$((total + 1))
  label="scenario=$sc payload=${PAYLOAD:-64} tls=1 target=$t run=$run"
  if [[ -f "$OUT" ]] && grep -qF "\"label\":\"$label\"" "$OUT"; then
    echo "[$total] skip (done): $label" >&2; continue
  fi
  echo "[$total] $label" >&2
  IFS='|' read -r server lgargs extra <<<"$(target "$t")"
  pids='$SERVER_PID'
  [[ -n "$extra" ]] && pids="\$SERVER_PID,\$($extra)"
  "$ROOT/bench/netlab.sh" --srv-egress none --cli-egress none \
    --server "taskset -c $SERVER_CPUS bash -c \"$server\"" \
    --client "sleep 1; taskset -c $LOADGEN_CPUS $LG gateway $lgargs --ca-file $WORK/ca.pem $(scenario "$sc") --seed $run --json --server-pid $pids --label '$label'" \
    2>>"${COMPETE_LOG:-/dev/null}" >> "$OUT" || echo "[$total] FAILED: $label" >&2
  # nginx outlives the gateway netlab stops; end it before the next run.
  [[ -f "$WORK/nginx/nginx.pid" ]] && kill "$(cat "$WORK/nginx/nginx.pid")" 2>/dev/null || true
  sleep 1
done; done; done
echo "done: $total runs -> $OUT" >&2
