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
python3 -B "$ROOT/bindings/test_guard_wiring.py"
python3 "$ROOT/bindings/guard_wiring.py"

echo "--- napi: build addon"
# Single source of truth (build + platform-correct stage): shared with
# pretest:package and CI (Node, so the platform tag is exact on every OS).
node "$ROOT/scripts/ci/build-napi-addon.js"

echo "--- napi: reclaim lifecycle feature regressions"
# `reclaim-test-support` is intentionally unavailable to shipped builds, but
# its capacity assertion guards the production admission order: public alias
# exhaustion must fail before a core recovery lease can be attached. Keep the
# feature explicit here so CI cannot silently omit this regression.
cargo test --locked --manifest-path "$ROOT/bindings/napi/Cargo.toml" \
  --features reclaim-test-support \
  recording::scan_recovery_handle_tests::capacity_rejection_does_not_attach_a_hidden_core_lease \
  --lib -- --exact

echo "--- napi: round-trip exchange"
node js/roundtrip.cjs

echo "--- napi: dispatch round-trip (F01 UbmCentral over ubm-desktop)"
node js/dispatch_roundtrip.cjs

echo "--- napi: terminal evidence survives process restart"
node js/terminal_evidence_restart_roundtrip.cjs

echo "--- napi: ordered group synthetic JavaScript round-trip"
node js/ordered_roundtrip.cjs

echo "--- napi: ordered marker synthetic JavaScript round-trip"
node js/ordered_marker_roundtrip.cjs

echo "--- napi: ordered recording synthetic JavaScript round-trip"
node js/ordered_recording_roundtrip.cjs

echo "--- napi: offline legacy recovery and receipt replay"
node js/legacy_recovery_roundtrip.cjs

echo "--- napi: scan recovery Promise ABI"
node js/scan_recovery_promise_abi.cjs

echo "--- napi: process-exit (10s budget)"
timeout 10 node js/exit_probe.cjs

echo "--- napi: versions"
rustc --version
cargo --version
node --version
cargo tree --depth 0 -p ubm5_napi_echo 2>/dev/null || true
grep -A2 'name = "napi"' "$ROOT/Cargo.lock" | head -6
