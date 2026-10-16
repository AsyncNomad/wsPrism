#!/usr/bin/env bash
# Build the gateway and run the SDK tests against it (Node 22+ needed for the
# global WebSocket; set NODE=/path/to/node if `node` is older).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../.."
node="$("$here/find-node.sh")"
(cd "$root" && cargo build -q -p wsprism-gateway --locked)
exec "$node" --test "$here"/test/*.test.mjs
