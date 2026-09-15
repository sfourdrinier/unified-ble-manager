#!/bin/sh
# WASM binding test: portable compile, dependency audit, real exchange in
# Node with an empty import object, and js-glue mapping proof. Fails loudly.
set -eu
cd "$(dirname "$0")"

WASM=target/wasm32-unknown-unknown/debug/ubm5_wasm_echo.wasm

echo "--- wasm: native unit tests"
cargo test

echo "--- wasm: portable build (default features, zero imports)"
cargo build --target wasm32-unknown-unknown
test -f "$WASM"

echo "--- wasm: dependency audit (no Tokio/fs/radio; no glue in default)"
cargo tree --target wasm32-unknown-unknown -e normal --prefix none 2>/dev/null | grep -qiE "tokio" \
  && { echo "FORBIDDEN DEP: tokio in wasm graph"; exit 1; } || true
cargo tree --target wasm32-unknown-unknown -e normal --prefix none 2>/dev/null | grep -qiE "wasm-bindgen" \
  && { echo "UNEXPECTED: wasm-bindgen in default wasm graph"; exit 1; } || true
grep -rnE "std::fs|tokio::|::radio" src/ \
  && { echo "FORBIDDEN USE in wasm sources"; exit 1; } || true
node -e "
const fs = require('node:fs');
WebAssembly.compile(fs.readFileSync('$WASM')).then(m => {
  const imports = WebAssembly.Module.imports(m);
  if (imports.length !== 0) {
    console.error('FORBIDDEN IMPORTS: ' + JSON.stringify(imports));
    process.exit(1);
  }
  console.log('wasm imports: none (portable)');
});"

echo "--- wasm: round-trip exchange (empty import object)"
node js/roundtrip.mjs "$WASM"

echo "--- wasm: js-glue mapping (wasm32 compile + export presence)"
CARGO_TARGET_DIR=target/wasm-glue cargo check --target wasm32-unknown-unknown --features js-glue
CARGO_TARGET_DIR=target/wasm-glue cargo build --target wasm32-unknown-unknown --features js-glue
GLUE_WASM=target/wasm-glue/wasm32-unknown-unknown/debug/ubm5_wasm_echo.wasm
for sym in echoBytes echoCounterU64 initContract describeJson; do
  strings "$GLUE_WASM" | grep -q "$sym" \
    || { echo "MISSING js-glue export mapping: $sym"; exit 1; }
done
echo "js-glue mappings present: echoBytes echoCounterU64 initContract describeJson"

echo "--- wasm: versions"
rustc --version
cargo --version
node --version
grep -A2 'name = "wasm-bindgen"' Cargo.lock | head -3 || echo "(wasm-bindgen: optional, not in default lock use)"
