#!/usr/bin/env bash
# Print a Node 22+ binary: $NODE, `node`, or the newest nvm one.
set -euo pipefail
ok() { [ -x "$(command -v "$1" 2>/dev/null || echo "$1")" ] && [ "$("$1" -p 'process.versions.node.split(".")[0]')" -ge 22 ]; }
for c in "${NODE:-}" node $(ls -d "$HOME"/.nvm/versions/node/v*/bin/node 2>/dev/null | sort -V -r); do
  [ -n "$c" ] && ok "$c" && { command -v "$c"; exit 0; }
done
echo "Node 22+ required; set NODE=/path/to/node" >&2
exit 1
