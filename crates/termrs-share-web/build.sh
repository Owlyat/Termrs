#!/usr/bin/env bash
# Build the termrs share page (wasm iroh client) into ./web/wasm.
#
# Requires:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version 0.2.122
#
# The result in ./web is a static site: host it anywhere (e.g. GitHub Pages)
# and set `[share] page_url` in termrs's config to its URL.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
profile="wasm-release"
if [[ "${1:-}" == "--debug" ]]; then
  profile="debug"
fi

cd "$here"
cargo build --profile "$profile" --target wasm32-unknown-unknown
wasm-bindgen "../../target/wasm32-unknown-unknown/$profile/termrs_share_web.wasm" \
  --out-dir web/wasm --target web --weak-refs
echo "share page ready: $here/web"
