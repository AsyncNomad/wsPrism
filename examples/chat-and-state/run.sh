#!/usr/bin/env bash
# Start the gateway and the example backend; open http://localhost:3000 in
# two browser windows. Ctrl-C stops both.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
node="$("$here/../../sdk/js/find-node.sh")"
(cd "$here/../.." && cargo build -q -p wsprism-gateway --locked)
"$here/../../target/debug/wsprism-gateway" --config "$here/gateway.yaml" &
gw=$!
trap 'kill $gw 2>/dev/null' EXIT
"$node" "$here/backend.mjs"
