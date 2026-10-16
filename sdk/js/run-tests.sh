#!/usr/bin/env bash
# Build the gateway and run the SDK tests against it (Node 22+ needed for the
# global WebSocket; set NODE=/path/to/node if `node` is older).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../.."
node="${NODE:-node}"
major="$("$node" -p 'process.versions.node.split(".")[0]')"
if [ "$major" -lt 22 ]; then
  for c in "$HOME"/.nvm/versions/node/v2[2-9]*/bin/node; do
    [ -x "$c" ] && node="$c"
  done
  major="$("$node" -p 'process.versions.node.split(".")[0]')"
  [ "$major" -ge 22 ] || { echo "Node 22+ required (found $major); set NODE=" >&2; exit 1; }
fi
(cd "$root" && cargo build -q -p wsprism-gateway --locked)
exec "$node" --test "$here"/test/*.test.mjs
