#!/usr/bin/env bash
# Build the gateway and run the example's end-to-end test (Node 22+).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
node="$("$here/../../sdk/js/find-node.sh")"
(cd "$here/../.." && cargo build -q -p wsprism-gateway --locked)
# --test-force-exit: a client left open by a failing test must not keep the
# runner alive (its reconnect timers would hang CI).
exec "$node" --test --test-force-exit "$here"/test/*.test.mjs
