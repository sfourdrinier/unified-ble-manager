#!/bin/sh
# WASM binding test: portable compile, dependency audit, real exchange in
# Node with an empty import object, and js-glue mapping proof. Fails loudly.
# Workspace layout: this crate is a member of the root workspace, so build
# artifacts land in the workspace target dir and the lockfile is the unified
# root Cargo.lock.
set -eu
cd "$(dirname "$0")"

ROOT="../.."
WASM="$ROOT/target/wasm32-unknown-unknown/debug/ubm5_wasm_echo.wasm"

echo "--- wasm: production guards (L1 safe-Rust + L2 doc names)"
python3 "$ROOT/bindings/guard_wiring.py"

echo "--- wasm: native unit tests"
cargo test -p ubm5_wasm_echo --locked

echo "--- wasm: portable build (default features, zero imports)"
cargo build -p ubm5_wasm_echo --target wasm32-unknown-unknown --locked
test -f "$WASM"

echo "--- wasm: dependency audit (no Tokio/fs/radio; no glue in default; wired core present)"
cargo tree -p ubm5_wasm_echo --target wasm32-unknown-unknown -e normal --prefix none 2>/dev/null | grep -qiE "tokio" \
  && { echo "FORBIDDEN DEP: tokio in wasm graph"; exit 1; } || true
cargo tree -p ubm5_wasm_echo --target wasm32-unknown-unknown -e normal --prefix none 2>/dev/null | grep -qiE "wasm-bindgen" \
  && { echo "UNEXPECTED: wasm-bindgen in default wasm graph"; exit 1; } || true
cargo tree -p ubm5_wasm_echo --target wasm32-unknown-unknown -e normal --prefix none 2>/dev/null | grep -q "ubm-core" \
  || { echo "MISSING: ubm-core not in wasm graph (seam unwired)"; exit 1; }
echo "wired: ubm-core in wasm graph"
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
CARGO_TARGET_DIR=target/wasm-glue cargo check -p ubm5_wasm_echo --target wasm32-unknown-unknown --features js-glue --locked
CARGO_TARGET_DIR=target/wasm-glue cargo build -p ubm5_wasm_echo --target wasm32-unknown-unknown --features js-glue --locked
GLUE_WASM=target/wasm-glue/wasm32-unknown-unknown/debug/ubm5_wasm_echo.wasm
for sym in echoBytes echoCounterU64 initContract describeJson centralStatus driveExpireSweep driveDestroy requestBleTransition; do
  strings "$GLUE_WASM" | grep -q "$sym" \
    || { echo "MISSING js-glue export mapping: $sym"; exit 1; }
done
echo "js-glue mappings present: echoBytes echoCounterU64 initContract describeJson centralStatus driveExpireSweep driveDestroy requestBleTransition"

echo "--- wasm: versions"
rustc --version
cargo --version
node --version
grep -A2 'name = "wasm-bindgen"' "$ROOT/Cargo.lock" | head -3 || echo "(wasm-bindgen: optional, not in default lock use)"
