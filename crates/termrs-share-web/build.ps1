# Build the termrs share page (wasm iroh client) into ./web/wasm.
#
# Requires:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version 0.2.122
#
# The result in ./web is a static site: host it anywhere (e.g. GitHub Pages)
# and set `[share] page_url` in termrs's config to its URL.

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$profile = if ($args -contains "--debug") { "debug" } else { "wasm-release" }
$profileDir = if ($profile -eq "debug") { "debug" } else { "wasm-release" }
$target = Join-Path (Resolve-Path (Join-Path $here "..\..")) "target\wasm32-unknown-unknown\$profileDir\termrs_share_web.wasm"
$out = Join-Path $here "web\wasm"

Push-Location $here
try {
    if ($profile -eq "debug") {
        cargo build --target wasm32-unknown-unknown
    } else {
        cargo build --profile wasm-release --target wasm32-unknown-unknown
    }
    wasm-bindgen $target --out-dir $out --target web --weak-refs
    Write-Host "share page ready: $here\web"
} finally {
    Pop-Location
}
