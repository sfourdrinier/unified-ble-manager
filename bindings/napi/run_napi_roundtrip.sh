#!/bin/sh
# N-API binding test: builds the real addon and drives the real exchange.
# Fails loudly on any step. Records exact versions at the end.
set -eu
cd "$(dirname "$0")"

echo "--- napi: build addon"
cargo build
cp target/debug/libubm5_napi_echo.so ubm_echo.linux-x64.node

echo "--- napi: round-trip exchange"
node js/roundtrip.cjs

echo "--- napi: process-exit (10s budget)"
timeout 10 node js/exit_probe.cjs

echo "--- napi: versions"
rustc --version
cargo --version
node --version
cargo tree --depth 0 -p ubm5_napi_echo 2>/dev/null || true
grep -A2 'name = "napi"' Cargo.lock | head -6
