#!/usr/bin/env bash
# Build the gateway and run the example's end-to-end test (Node 22+).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
node="$("$here/../../sdk/js/find-node.sh")"
(cd "$here/../.." && cargo build -q -p wsprism-gateway --locked)
exec "$node" --test "$here"/test/*.test.mjs
