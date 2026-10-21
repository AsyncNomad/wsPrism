#!/usr/bin/env bash
# Download pinned competitor gateway binaries for the WebSocket comparison
# (ADR 0004 rev 3) and verify their published SHA-256 checksums.
# Installs into ${WSPRISM_BENCH_TOOLS:-~/.cache/wsprism-bench}/<name>-<version>/.
# Only the download leaves the machine; benchmark traffic stays in netlab.
set -euo pipefail

TOOLS="${WSPRISM_BENCH_TOOLS:-$HOME/.cache/wsprism-bench}"
CENTRIFUGO_VERSION=6.9.7
SOCKUDO_VERSION=5.1.0

fetch() { # name version url sha256-source-url archive
  local name="$1" ver="$2" url="$3" sums="$4" file="$5" dir="$TOOLS/$1-$2"
  if [[ -f "$dir/.ok" ]]; then echo "$name $ver: present ($dir)"; return; fi
  rm -rf "$dir" && mkdir -p "$dir/dl"
  curl -fsSL -o "$dir/dl/$file" "$url"
  curl -fsSL -o "$dir/dl/sums" "$sums"
  (cd "$dir/dl" && grep -F "$file" sums | awk '{print $1"  "$2}' | sed "s|  .*|  $file|" | sha256sum -c -)
  tar -xzf "$dir/dl/$file" -C "$dir"
  touch "$dir/.ok"
  echo "$name $ver: installed ($dir)"
}

fetch centrifugo "$CENTRIFUGO_VERSION" \
  "https://github.com/centrifugal/centrifugo/releases/download/v$CENTRIFUGO_VERSION/centrifugo_${CENTRIFUGO_VERSION}_linux_amd64.tar.gz" \
  "https://github.com/centrifugal/centrifugo/releases/download/v$CENTRIFUGO_VERSION/centrifugo_${CENTRIFUGO_VERSION}_checksums.txt" \
  "centrifugo_${CENTRIFUGO_VERSION}_linux_amd64.tar.gz"

fetch sockudo "$SOCKUDO_VERSION" \
  "https://github.com/sockudo/sockudo/releases/download/v$SOCKUDO_VERSION/sockudo-v$SOCKUDO_VERSION-x86_64-unknown-linux-gnu.tgz" \
  "https://github.com/sockudo/sockudo/releases/download/v$SOCKUDO_VERSION/sockudo-v$SOCKUDO_VERSION-x86_64-unknown-linux-gnu.tgz.sha256" \
  "sockudo-v$SOCKUDO_VERSION-x86_64-unknown-linux-gnu.tgz"
