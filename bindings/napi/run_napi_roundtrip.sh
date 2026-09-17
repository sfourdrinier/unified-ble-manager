#!/bin/sh
# N-API binding test: builds the real addon and drives the real exchange.
# Fails loudly on any step. Records exact versions at the end.
# Workspace layout: this crate is a member of the root workspace, so build
# artifacts land in the workspace target dir and the lockfile is the unified
# root Cargo.lock.
set -eu
cd "$(dirname "$0")"

ROOT="../.."

echo "--- napi: production guards (L1 safe-Rust + L2 doc names)"
python3 "$ROOT/bindings/guard_wiring.py"

echo "--- napi: build addon"
# Single source of truth (build + platform-correct stage): shared with CI.
sh ./build-addon.sh

echo "--- napi: round-trip exchange"
node js/roundtrip.cjs

echo "--- napi: dispatch round-trip (F01 UbmCentral over ubm-desktop)"
node js/dispatch_roundtrip.cjs

echo "--- napi: process-exit (10s budget)"
timeout 10 node js/exit_probe.cjs

echo "--- napi: versions"
rustc --version
cargo --version
node --version
cargo tree --depth 0 -p ubm5_napi_echo 2>/dev/null || true
grep -A2 'name = "napi"' "$ROOT/Cargo.lock" | head -6
