#!/bin/sh
set -eu
cd "$(dirname "$0")"

if ! command -v wasm-bindgen >/dev/null 2>&1; then
    echo 'Install the WASM tools first: rustup target add wasm32-unknown-unknown' >&2
    echo 'Then: cargo install wasm-bindgen-cli --version 0.2.129 --locked' >&2
    exit 1
fi
if [ "$(wasm-bindgen --version)" != 'wasm-bindgen 0.2.129' ]; then
    echo 'This client requires wasm-bindgen-cli 0.2.129 (matching Cargo.toml).' >&2
    exit 1
fi

"${CARGO:-cargo}" build --locked --release --target wasm32-unknown-unknown
mkdir -p dist
wasm-bindgen "${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/release/capturefab_web.wasm" \
    --target web --out-dir dist --out-name capturefab_web --no-typescript
cp index.html style.css bootstrap.js dist/
echo 'Built web-client/dist. Build it into capturefab with --features embedded-web, or serve it with --web-dir web-client/dist.'
